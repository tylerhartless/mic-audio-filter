//! Live preview and "Hear myself".
//!
//! On Linux the mic only runs while something records from the virtual mic,
//! so a silent `pw-record` keeps it running while the window is open, and
//! `pw-loopback` plays it into your headphones. On macOS the mic always runs,
//! and the window plays the virtual device into your headphones itself.

#[cfg(pipewire_backend)]
use std::io;
#[cfg(pipewire_backend)]
use std::process::{Child, Command, ExitStatus, Stdio};

/// Whether the window has to keep the mic running to show levels.
pub const NEEDS_PREVIEW: bool = cfg!(pipewire_backend);

/// What stopped, and why.
#[derive(Default)]
pub struct Problems {
    pub preview: Option<String>,
    pub monitor: Option<String>,
}

#[derive(Default)]
pub struct Listen {
    /// A silent recorder that keeps the virtual mic, and so the meters,
    /// running while the window is open.
    #[cfg(pipewire_backend)]
    preview: Option<Child>,
    /// pw-loopback playing the virtual mic into your headphones.
    #[cfg(pipewire_backend)]
    monitor: Option<Child>,
    #[cfg(cpal_backend)]
    monitor: Option<crate::cpal_engine::Monitor>,
}

impl Listen {
    /// Starts or stops each helper to match what's wanted. `output_name` is
    /// the `[output] name` from the config.
    #[cfg(pipewire_backend)]
    pub fn sync(&mut self, output_name: &str, preview: bool, monitor: bool) -> Problems {
        let preview_props =
            "{ node.name = \"mic-filter.gui-preview\" node.description = \"Mic Filter preview\" }";
        let preview = sync_child(&mut self.preview, preview, || {
            spawn(
                "pw-record",
                &[
                    "--target",
                    output_name,
                    "-P",
                    preview_props,
                    "--rate",
                    "48000",
                    "--channels",
                    "1",
                    "/dev/null",
                ],
            )
        });
        let monitor = sync_child(&mut self.monitor, monitor, || {
            spawn(
                "pw-loopback",
                &["-C", output_name, "-n", "mic-filter-monitor"],
            )
        });
        Problems {
            preview: preview.err().map(|e| e.to_string()),
            monitor: monitor.err().map(|e| e.to_string()),
        }
    }

    #[cfg(cpal_backend)]
    pub fn sync(&mut self, output_name: &str, _preview: bool, monitor: bool) -> Problems {
        use crate::cpal_engine::Monitor;
        let mut problems = Problems::default();
        if let Some(e) = self.monitor.as_ref().and_then(Monitor::error) {
            self.monitor = None;
            problems.monitor = Some(e);
        } else if !monitor {
            self.monitor = None;
        } else if self.monitor.is_none() {
            match Monitor::start(output_name) {
                Ok(m) => self.monitor = Some(m),
                Err(e) => problems.monitor = Some(format!("{e:#}")),
            }
        }
        problems
    }
}

#[cfg(pipewire_backend)]
impl Drop for Listen {
    fn drop(&mut self) {
        for child in [self.preview.take(), self.monitor.take()]
            .into_iter()
            .flatten()
        {
            stop(child);
        }
    }
}

/// Starts a helper process that dies with this window, so a crash can't
/// leave the mic open.
#[cfg(pipewire_backend)]
fn spawn(program: &str, args: &[&str]) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: prctl is async-signal-safe, which is all pre_exec requires.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd.spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("could not start {program}: {e}")))
}

#[cfg(pipewire_backend)]
fn stop(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Starts or stops a helper process to match `want`. Errors if the process
/// can't start, or exits by itself.
#[cfg(pipewire_backend)]
fn sync_child(
    slot: &mut Option<Child>,
    want: bool,
    start: impl FnOnce() -> io::Result<Child>,
) -> io::Result<()> {
    if let Some(child) = slot {
        let exited: Option<ExitStatus> = child.try_wait()?;
        if let Some(status) = exited {
            *slot = None;
            if want {
                return Err(io::Error::other(format!(
                    "the helper process exited ({status})"
                )));
            }
        } else if !want {
            stop(slot.take().unwrap());
        }
    }
    if want && slot.is_none() {
        *slot = Some(start()?);
    }
    Ok(())
}
