//! "Set from my voice": listens while you talk, then puts the voice gate's
//! unmute level a safe distance below your speech.

use crate::ipc::Snapshot;
use std::time::{Duration, Instant};

const LISTEN: Duration = Duration::from_secs(4);
/// Readings with at least this voice probability count as you talking.
const VOICE: f32 = 0.6;
/// How far below your typical speech peak the unmute level goes. Quieter
/// syllables still open the gate, and sounds from across the room, which
/// reach a close mic much quieter, don't.
const MARGIN_DB: f32 = 15.0;
/// Less talking than this (in meter readings, about 50 ms each) is too
/// little to judge from.
const MIN_READINGS: usize = 10;

pub struct Calibration {
    started: Instant,
    peaks: Vec<f32>,
}

impl Calibration {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            peaks: Vec::new(),
        }
    }

    /// Adds one meter reading. Only readings that sound like a voice count.
    pub fn add(&mut self, reading: &Snapshot) {
        if reading.vad >= VOICE && reading.gate_db > -60.0 {
            self.peaks.push(reading.gate_db);
        }
    }

    pub fn seconds_left(&self) -> u64 {
        LISTEN
            .saturating_sub(self.started.elapsed())
            .as_secs_f32()
            .ceil() as u64
    }

    pub fn done(&self) -> bool {
        self.started.elapsed() >= LISTEN
    }

    /// The unmute level, in whole dB, or `None` if it heard too little
    /// talking. The median ignores a stray pop or a quiet trailing word.
    pub fn level(mut self) -> Option<f32> {
        if self.peaks.len() < MIN_READINGS {
            return None;
        }
        self.peaks.sort_by(f32::total_cmp);
        let median = self.peaks[self.peaks.len() / 2];
        Some((median - MARGIN_DB).round().clamp(-60.0, -10.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(gate_db: f32, vad: f32) -> Snapshot {
        Snapshot {
            gate_db,
            vad,
            ..Snapshot::default()
        }
    }

    #[test]
    fn level_sits_below_typical_speech() {
        let mut c = Calibration::new();
        for i in 0..40 {
            // Speech peaks around -20 dB, one loud pop, and pauses.
            c.add(&reading(-20.0 - (i % 5) as f32, 0.95));
            c.add(&reading(-55.0, 0.1));
        }
        c.add(&reading(-2.0, 0.9));
        assert_eq!(c.level(), Some(-37.0));
    }

    #[test]
    fn too_little_talking_gives_no_level() {
        let mut c = Calibration::new();
        for _ in 0..100 {
            c.add(&reading(-30.0, 0.2));
        }
        for _ in 0..5 {
            c.add(&reading(-20.0, 0.9));
        }
        assert_eq!(c.level(), None);
    }
}
