//! RNNoise noise suppression (via the pure-Rust `nnnoiseless` port) with an
//! optional voice-activity gate.

use crate::dynamics::db_to_lin;
use crate::settings::DenoiseSettings;
use nnnoiseless::DenoiseState;

/// RNNoise works on fixed 10 ms frames at 48 kHz.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;

/// RNNoise expects samples scaled like 16-bit integers.
const I16_SCALE: f32 = 32768.0;

/// Once open, the gate closes this far below its opening level, so a sound
/// sitting right at the level doesn't flutter it.
const GATE_HYSTERESIS_DB: f32 = 6.0;

#[derive(Debug, Clone, Copy)]
pub struct DenoiseParams {
    pub enabled: bool,
    strength: f32,
    vad_threshold: f32,
    hold_frames: u32,
    /// Peak levels the gate needs to open and to stay open, in RNNoise's
    /// 16-bit scale.
    open_level: f32,
    close_level: f32,
}

impl DenoiseParams {
    pub fn new(s: &DenoiseSettings, sample_rate: f32) -> Self {
        let frame_ms = FRAME as f32 * 1000.0 / sample_rate;
        Self {
            enabled: s.enabled,
            strength: s.strength.clamp(0.0, 1.0),
            vad_threshold: s.vad_threshold.clamp(0.0, 1.0),
            hold_frames: (s.vad_hold_ms / frame_ms).ceil() as u32,
            open_level: db_to_lin(s.gate_level_db) * I16_SCALE,
            close_level: db_to_lin(s.gate_level_db - GATE_HYSTERESIS_DB) * I16_SCALE,
        }
    }
}

pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    /// The frame being collected from the input.
    collecting: [f32; FRAME],
    /// The last finished frame, played out while the next one is collected.
    ready: [f32; FRAME],
    /// Raw input lined up with what RNNoise is currently emitting (RNNoise
    /// itself lags one frame), used to blend when strength < 1.
    dry: [f32; FRAME],
    scratch: [f32; FRAME],
    pos: usize,
    gate: f32,
    hold_left: u32,
    last_vad: f32,
    /// Highest frame peak the gate measured, and whether it was open, since
    /// the last [`Self::take_gate_reading`].
    level_peak: f32,
    was_open: bool,
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    /// Allocates the RNNoise state; call this off the audio thread.
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            collecting: [0.0; FRAME],
            ready: [0.0; FRAME],
            dry: [0.0; FRAME],
            scratch: [0.0; FRAME],
            pos: 0,
            gate: 1.0,
            hold_left: 0,
            last_vad: 0.0,
            level_peak: 0.0,
            was_open: false,
        }
    }

    /// Clears buffered audio so re-enabling doesn't replay stale samples.
    pub fn reset(&mut self) {
        self.collecting = [0.0; FRAME];
        self.ready = [0.0; FRAME];
        self.dry = [0.0; FRAME];
        self.pos = 0;
        self.gate = 1.0;
        self.hold_left = 0;
    }

    /// Voice probability RNNoise reported for the most recent frame.
    pub fn last_vad(&self) -> f32 {
        self.last_vad
    }

    /// The highest peak level the gate measured (linear, after noise
    /// removal), and whether the gate was open at any point, since the last
    /// call.
    pub fn take_gate_reading(&mut self) -> (f32, bool) {
        let reading = (self.level_peak / I16_SCALE, self.was_open);
        self.level_peak = 0.0;
        self.was_open = false;
        reading
    }

    /// Delay this stage adds: one frame of buffering plus RNNoise's own frame.
    pub fn latency_samples(&self) -> usize {
        2 * FRAME
    }

    /// Denoises `buf` in place. Any block size works; output lags input by
    /// [`Self::latency_samples`].
    pub fn process(&mut self, p: &DenoiseParams, buf: &mut [f32]) {
        for s in buf.iter_mut() {
            self.collecting[self.pos] = *s * I16_SCALE;
            *s = self.ready[self.pos];
            self.pos += 1;
            if self.pos == FRAME {
                self.run_frame(p);
                self.pos = 0;
            }
        }
    }

    fn run_frame(&mut self, p: &DenoiseParams) {
        let vad = self
            .state
            .process_frame(&mut self.scratch, &self.collecting);
        self.last_vad = vad;

        // The gate opens for frames that sound like a voice and are loud
        // enough to be the person at the mic.
        let level = self.scratch.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        self.level_peak = self.level_peak.max(level);
        let min_level = if self.gate > 0.0 {
            p.close_level
        } else {
            p.open_level
        };
        let target = if p.vad_threshold <= 0.0 || (vad >= p.vad_threshold && level >= min_level) {
            self.hold_left = p.hold_frames;
            1.0
        } else if self.hold_left > 0 {
            self.hold_left -= 1;
            1.0
        } else {
            0.0
        };

        // Ramp the gate across the frame so opening and closing don't click.
        let start = self.gate;
        let step = (target - start) / FRAME as f32;
        let wet = p.strength;
        let dry = 1.0 - wet;
        for i in 0..FRAME {
            let gate = start + step * (i + 1) as f32;
            self.ready[i] = (wet * self.scratch[i] + dry * self.dry[i]) * gate / I16_SCALE;
        }
        self.gate = target;
        self.was_open |= target > 0.0;
        self.dry = self.collecting;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::DenoiseSettings;

    fn params(strength: f32, vad_threshold: f32) -> DenoiseParams {
        DenoiseParams::new(
            &DenoiseSettings {
                enabled: true,
                strength,
                vad_threshold,
                vad_hold_ms: 0.0,
                gate_level_db: -80.0,
            },
            48_000.0,
        )
    }

    /// The voice gate at a -40 dB unmute level, holding over brief dips in
    /// RNNoise's voice score.
    fn gate_params() -> DenoiseParams {
        DenoiseParams::new(
            &DenoiseSettings {
                enabled: true,
                strength: 1.0,
                vad_threshold: 0.5,
                vad_hold_ms: 100.0,
                gate_level_db: -40.0,
            },
            48_000.0,
        )
    }

    fn run(d: &mut Denoiser, p: &DenoiseParams, buf: &mut [f32]) {
        for chunk in buf.chunks_mut(256) {
            d.process(p, chunk);
        }
    }

    /// The fraction of 10 ms frames with any sound in them.
    fn open_fraction(buf: &[f32]) -> f32 {
        let frames: Vec<_> = buf.chunks(FRAME).collect();
        let open = frames
            .iter()
            .filter(|f| f.iter().any(|&x| x != 0.0))
            .count();
        open as f32 / frames.len() as f32
    }

    #[test]
    fn dry_path_is_delayed_by_reported_latency() {
        let mut d = Denoiser::new();
        let p = params(0.0, 0.0);
        let mut buf = vec![0.0f32; 4 * FRAME];
        buf[7] = 0.5;
        // Odd block sizes to prove framing doesn't depend on them.
        for chunk in buf.chunks_mut(333) {
            d.process(&p, chunk);
        }
        let at = buf.iter().position(|&x| x != 0.0).unwrap();
        assert_eq!(at, 7 + d.latency_samples());
        assert!((buf[at] - 0.5).abs() < 1e-6);
    }

    /// Low-passed noise, roughly like a fan or room rumble. (RNNoise was
    /// trained on real-world noise; flat white noise and pure tones are
    /// poor stand-ins, and it scores tones as voiced.)
    fn fan_noise(len: usize, amp: f32) -> Vec<f32> {
        let mut seed = 0x1234_5678u32;
        let mut lp = 0.0f32;
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let white = seed as f32 / u32::MAX as f32 - 0.5;
                lp = 0.95 * lp + 0.05 * white * 8.0;
                lp * amp
            })
            .collect()
    }

    #[test]
    fn fan_noise_is_reduced() {
        let mut d = Denoiser::new();
        let p = params(1.0, 0.0);
        let mut buf = fan_noise(96_000, 0.05);
        let in_energy: f32 = buf[48_000..].iter().map(|x| x * x).sum();
        for chunk in buf.chunks_mut(256) {
            d.process(&p, chunk);
        }
        let out_energy: f32 = buf[48_000..].iter().map(|x| x * x).sum();
        let reduction_db = 10.0 * (in_energy / out_energy).log10();
        assert!(
            reduction_db > 20.0,
            "only {reduction_db:.1} dB of noise reduction"
        );
    }

    #[test]
    fn vad_gate_mutes_non_speech() {
        let mut d = Denoiser::new();
        let p = params(0.0, 0.5);
        let mut buf = fan_noise(96_000, 0.05);
        for chunk in buf.chunks_mut(480) {
            d.process(&p, chunk);
        }
        assert!(buf[48_000..].iter().all(|&x| x == 0.0));
    }

    /// A buzzy tone with speech-like harmonics around a 150 Hz pitch.
    fn voiced(len: usize, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|n| {
                let t = n as f32 / 48_000.0;
                let f0 = 150.0 * (1.0 + 0.03 * (2.0 * std::f32::consts::PI * 4.0 * t).sin());
                let phase = 2.0 * std::f32::consts::PI * f0 * t;
                let x: f32 = (1..=20).map(|k| (k as f32 * phase).sin() / k as f32).sum();
                x * amp * 0.5
            })
            .collect()
    }

    #[test]
    fn gate_ignores_quiet_voices() {
        let mut d = Denoiser::new();
        // About -59 dB: a voice across the room.
        let mut buf = voiced(96_000, 0.001);
        run(&mut d, &gate_params(), &mut buf);
        assert!(buf[4_800..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn gate_opens_for_loud_voices() {
        let mut d = Denoiser::new();
        let mut buf = voiced(96_000, 0.3);
        run(&mut d, &gate_params(), &mut buf);
        let open = open_fraction(&buf[4_800..]);
        assert!(open > 0.95, "open for only {:.0}% of frames", open * 100.0);
    }

    #[test]
    fn gate_stays_open_just_below_its_level() {
        // About -43 dB: below the -40 dB unmute level, above where an open
        // gate closes.
        let quiet = 0.0063;
        let p = gate_params();

        let mut d = Denoiser::new();
        let mut buf = voiced(96_000, quiet);
        run(&mut d, &p, &mut buf);
        assert!(buf[4_800..].iter().all(|&x| x == 0.0), "opened from closed");

        let mut d = Denoiser::new();
        let mut buf = voiced(96_000, 0.3);
        buf[48_000..].iter_mut().for_each(|x| *x *= quiet / 0.3);
        run(&mut d, &p, &mut buf);
        let open = open_fraction(&buf[72_000..]);
        assert!(open > 0.95, "open for only {:.0}% of frames", open * 100.0);
    }
}
