//! Live meter feed from the running service to the GUI, over a Unix socket in
//! `$XDG_RUNTIME_DIR`. The feed is read-only: settings travel through the
//! config file, which the service already reloads live.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub fn socket_path(output_name: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    dir.join(format!("mic-filter-{output_name}.sock"))
}

/// One meter update. Levels are dBFS peaks since the previous update.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub input_db: f32,
    pub output_db: f32,
    pub reduction_db: f32,
    pub vad: f32,
    /// Some app is recording from the virtual mic, so audio is flowing.
    pub streaming: bool,
    /// The level the voice gate measures, and whether it let sound through.
    pub gate_db: f32,
    pub gate_open: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            input_db: -120.0,
            output_db: -120.0,
            reduction_db: 0.0,
            vad: 0.0,
            streaming: false,
            gate_db: -120.0,
            gate_open: false,
        }
    }
}

impl Snapshot {
    fn encode(&self) -> String {
        format!(
            "{:.1} {:.1} {:.1} {:.3} {} {:.1} {}\n",
            self.input_db.max(-120.0),
            self.output_db.max(-120.0),
            self.reduction_db,
            self.vad,
            u8::from(self.streaming),
            self.gate_db.max(-120.0),
            u8::from(self.gate_open),
        )
    }

    #[cfg_attr(not(feature = "gui"), allow(dead_code))]
    fn decode(line: &str) -> Option<Snapshot> {
        let mut parts = line.split_whitespace();
        let mut next = || parts.next()?.parse::<f32>().ok();
        Some(Snapshot {
            input_db: next()?,
            output_db: next()?,
            reduction_db: next()?,
            vad: next()?,
            streaming: next()? != 0.0,
            gate_db: next()?,
            gate_open: next()? != 0.0,
        })
    }
}

/// Service side. Every published snapshot goes to every connected client.
pub struct MeterServer {
    latest: Arc<(Mutex<(u64, Snapshot)>, Condvar)>,
    path: PathBuf,
}

impl MeterServer {
    pub fn start(path: PathBuf) -> io::Result<MeterServer> {
        if UnixStream::connect(&path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another mic-filter is already serving {}", path.display()),
            ));
        }
        // Left over from an instance that didn't shut down cleanly.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        let latest = Arc::new((Mutex::new((0, Snapshot::default())), Condvar::new()));

        let shared = latest.clone();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let shared = shared.clone();
                thread::spawn(move || serve(conn, &shared));
            }
        });
        Ok(MeterServer { latest, path })
    }

    pub fn publish(&self, snapshot: Snapshot) {
        let (lock, cvar) = &*self.latest;
        let mut slot = lock.lock().unwrap();
        *slot = (slot.0 + 1, snapshot);
        cvar.notify_all();
    }
}

fn serve(mut conn: UnixStream, shared: &(Mutex<(u64, Snapshot)>, Condvar)) {
    let (lock, cvar) = shared;
    let mut seen = 0;
    loop {
        let line = {
            let slot = lock.lock().unwrap();
            let (slot, _) = cvar
                .wait_timeout_while(slot, Duration::from_secs(1), |s| s.0 == seen)
                .unwrap();
            seen = slot.0;
            slot.1.encode()
        };
        if conn.write_all(line.as_bytes()).is_err() {
            return;
        }
    }
}

impl Drop for MeterServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// GUI side: keeps a connection to the service open, reconnecting every
/// second while it's down. `on_update` gets `None` while disconnected.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn spawn_client(path: PathBuf, on_update: impl Fn(Option<Snapshot>) + Send + 'static) {
    thread::spawn(move || loop {
        if let Ok(conn) = UnixStream::connect(&path) {
            read_feed(conn, &on_update);
        }
        on_update(None);
        thread::sleep(Duration::from_secs(1));
    });
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn read_feed(conn: UnixStream, on_update: &impl Fn(Option<Snapshot>)) {
    for line in BufReader::new(conn).lines() {
        let Ok(line) = line else { return };
        if let Some(snapshot) = Snapshot::decode(&line) {
            on_update(Some(snapshot));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn snapshot_round_trips() {
        let s = Snapshot {
            input_db: -18.3,
            output_db: -6.1,
            reduction_db: 4.5,
            vad: 0.875,
            streaming: true,
            gate_db: -27.4,
            gate_open: true,
        };
        assert_eq!(Snapshot::decode(s.encode().trim()), Some(s));
        assert_eq!(Snapshot::decode("garbage"), None);
    }

    #[test]
    fn clients_receive_published_snapshots() {
        let path =
            std::env::temp_dir().join(format!("mic-filter-test-{}.sock", std::process::id()));
        let server = MeterServer::start(path.clone()).unwrap();
        assert!(
            MeterServer::start(path.clone()).is_err(),
            "second server must not steal the socket"
        );

        let (tx, rx) = mpsc::channel();
        let conn = UnixStream::connect(&path).unwrap();
        thread::spawn(move || read_feed(conn, &move |s| tx.send(s).unwrap()));

        let sent = Snapshot {
            input_db: -12.0,
            streaming: true,
            ..Snapshot::default()
        };
        // Publish until the client has connected and sees it.
        let got = (0..50).find_map(|_| {
            server.publish(sent);
            rx.recv_timeout(Duration::from_millis(100))
                .ok()
                .flatten()
                .filter(|s| *s == sent)
        });
        assert_eq!(got, Some(sent));
        drop(server);
        assert!(!path.exists());
    }
}
