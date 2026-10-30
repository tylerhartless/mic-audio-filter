//! What both audio backends share: status messages, the `--meter` line, and
//! config hot-reload.

use crate::config::{self, Config};
use mic_filter_dsp::MeterReading;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Set while `--meter` has a live line drawn on stderr.
pub static METER_LINE: AtomicBool = AtomicBool::new(false);

/// eprintln! that first clears the meter line, so messages don't get glued
/// onto it. The meter redraws below on its next tick.
macro_rules! status {
    ($($arg:tt)*) => {{
        if $crate::service::METER_LINE.swap(false, std::sync::atomic::Ordering::Relaxed) {
            eprint!("\r\x1b[2K");
        }
        eprintln!($($arg)*);
    }};
}

pub struct Options {
    pub config_path: PathBuf,
    pub config: Config,
    pub meter: bool,
}

/// Ends the meter line, so the shell prompt starts on a fresh line.
pub fn finish_meter() {
    if METER_LINE.swap(false, Ordering::Relaxed) {
        eprintln!();
    }
}

/// Reloads the config file when its modification time changes.
pub struct ConfigWatcher {
    path: PathBuf,
    modified: Option<SystemTime>,
    current: Config,
    last_notice: Option<Instant>,
}

impl ConfigWatcher {
    pub fn new(path: PathBuf, current: Config) -> Self {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            path,
            modified,
            current,
            last_notice: None,
        }
    }

    /// Returns `(previous, new)` if the file changed and is valid.
    pub fn poll(&mut self) -> Option<(Config, Config)> {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified == self.modified {
            return None;
        }
        self.modified = modified;
        match config::Config::load(&self.path) {
            Ok(new) if new == self.current => None,
            Ok(new) => {
                if new.output != self.current.output {
                    status!("config reloaded; restart mic-filter to apply [output] changes");
                } else if self
                    .last_notice
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(3))
                {
                    // Throttled: dragging a GUI slider rewrites the file many
                    // times a second.
                    status!("config reloaded");
                    self.last_notice = Some(Instant::now());
                }
                let old = std::mem::replace(&mut self.current, new.clone());
                Some((old, new))
            }
            Err(e) => {
                status!("config not reloaded: {e:#}");
                None
            }
        }
    }
}

/// Redraws the `--meter` line. `idle` says why there's nothing to show while
/// audio isn't flowing.
pub fn print_meter(r: MeterReading, streaming: bool, idle: &str) {
    let mut out = std::io::stderr().lock();
    METER_LINE.store(true, Ordering::Relaxed);
    if !streaming {
        let _ = write!(out, "\r\x1b[2Kidle: {idle}");
    } else {
        let _ = write!(
            out,
            "\r\x1b[2Kin {:>6.1} dB {}  out {:>6.1} dB {}  comp -{:>4.1} dB  voice {:>3.0}%",
            r.input_peak_db.max(-99.9),
            bar(r.input_peak_db),
            r.output_peak_db.max(-99.9),
            bar(r.output_peak_db),
            r.reduction_db,
            r.vad * 100.0,
        );
    }
    let _ = out.flush();
}

/// A 20-cell bar spanning -60..0 dBFS.
fn bar(db: f32) -> String {
    let cells = (((db + 60.0) / 3.0).clamp(0.0, 20.0)) as usize;
    format!("[{}{}]", "#".repeat(cells), " ".repeat(20 - cells))
}
