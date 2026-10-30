//! Carries audio from one device to another when each runs on its own clock,
//! as a mic and BlackHole do on macOS.
//!
//! Two clocks never agree exactly. A mic that runs 100 ppm fast delivers 4.8
//! extra samples a second at 48 kHz, so a plain buffer between the devices
//! slowly fills up (adding delay) or drains (causing dropouts). The reader
//! resamples instead: it converts the input rate to the output rate, and
//! trims the ratio by a fraction of a percent to hold the buffer at a small,
//! steady level. Trims that small are inaudible.
//!
//! The capture callback writes into a [`Feeder`] and the playback callback
//! reads from a [`Reader`]. Neither allocates or blocks.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// How much audio the buffer can hold before the feeder drops samples.
const CAPACITY_SECS: f64 = 0.5;
/// Spare audio kept beyond one input block, against callback jitter. Each
/// dropout adds `MARGIN_STEP_SECS`, up to `MAX_MARGIN_SECS`.
const MARGIN_SECS: f64 = 0.002;
const MARGIN_STEP_SECS: f64 = 0.002;
const MAX_MARGIN_SECS: f64 = 0.04;
/// How often the trim is updated, and how long it takes to work off a
/// difference from the target level.
const WINDOW_SECS: f64 = 0.5;
const HORIZON_SECS: f64 = 2.0;
/// The largest trim: 0.5% is about 9 cents of pitch.
const MAX_TRIM: f64 = 0.005;
/// A backlog this far above the target is skipped at once instead of
/// trimmed away, such as after the output device stalled.
const MAX_EXCESS_SECS: f64 = 0.1;

/// Counts of the bridge's corrective jumps, for the log.
#[derive(Default)]
pub struct Stats {
    pub dropouts: AtomicUsize,
    pub skips: AtomicUsize,
}

/// Creates a bridge from a device at `in_rate` to one at `out_rate`.
pub fn bridge(in_rate: u32, out_rate: u32) -> (Feeder, Reader) {
    let capacity = (in_rate as f64 * CAPACITY_SECS) as usize;
    let (tx, rx) = rtrb::RingBuffer::new(capacity);
    let block = Arc::new(AtomicUsize::new(0));
    let in_rate = in_rate as f64;
    let window = (out_rate as f64 * WINDOW_SECS) as usize;
    let feeder = Feeder {
        tx,
        block: block.clone(),
    };
    let reader = Reader {
        rx,
        block,
        in_rate,
        nominal: in_rate / out_rate as f64,
        trim: 0.0,
        frac: 0.0,
        hist: [0.0; 4],
        running: false,
        margin: (in_rate * MARGIN_SECS) as usize,
        window,
        window_left: window,
        window_min: usize::MAX,
        stats: Arc::default(),
    };
    (feeder, reader)
}

/// The capture side.
pub struct Feeder {
    tx: rtrb::Producer<f32>,
    /// The largest block written so far.
    block: Arc<AtomicUsize>,
}

impl Feeder {
    /// Adds captured mono samples. If the reader has stopped and the buffer
    /// is full, the newest samples are dropped.
    pub fn push(&mut self, samples: impl ExactSizeIterator<Item = f32>) {
        let n = samples.len();
        self.block.fetch_max(n, Ordering::Relaxed);
        let n = n.min(self.tx.slots());
        if let Ok(chunk) = self.tx.write_chunk_uninit(n) {
            chunk.fill_from_iter(samples);
        }
    }
}

/// The playback side.
pub struct Reader {
    rx: rtrb::Consumer<f32>,
    block: Arc<AtomicUsize>,
    in_rate: f64,
    /// Input samples per output sample when both clocks are exact.
    nominal: f64,
    /// The correction on top of `nominal` that holds the buffer level.
    trim: f64,
    /// The read position, between `hist[1]` and `hist[2]`.
    frac: f64,
    /// The four input samples around the read position.
    hist: [f32; 4],
    /// False until the buffer has filled to the target level.
    running: bool,
    margin: usize,
    window: usize,
    window_left: usize,
    /// The least audio left in the buffer after a callback, this window.
    window_min: usize,
    stats: Arc<Stats>,
}

impl Reader {
    pub fn stats(&self) -> Arc<Stats> {
        self.stats.clone()
    }

    /// How much input audio the reader keeps buffered once it settles.
    fn target(&self) -> usize {
        self.block.load(Ordering::Relaxed) + self.margin
    }

    /// Fills `out` with mono audio at the output rate. Plays silence until
    /// enough input has arrived.
    pub fn fill(&mut self, out: &mut [f32]) {
        if out.is_empty() {
            return;
        }
        let step = self.nominal * (1.0 + self.trim);
        let need = (self.frac + (out.len() - 1) as f64 * step) as usize;
        let avail = self.rx.slots();
        let target = self.target();

        if !self.running {
            if avail < need + target {
                out.fill(0.0);
                return;
            }
            // Start with no more delay than the target.
            self.skip(avail - need - target);
            self.running = true;
            self.trim = 0.0;
            self.window_left = self.window;
            self.window_min = usize::MAX;
        } else if avail < need {
            // The input stalled or the clocks slipped further than the
            // margin allowed for. Wait for a full buffer again, with more
            // margin this time.
            out.fill(0.0);
            self.running = false;
            self.hist = [0.0; 4];
            self.margin = (self.margin + (self.in_rate * MARGIN_STEP_SECS) as usize)
                .min((self.in_rate * MAX_MARGIN_SECS) as usize);
            self.stats.dropouts.fetch_add(1, Ordering::Relaxed);
            return;
        } else if avail - need > target + (self.in_rate * MAX_EXCESS_SECS) as usize {
            self.skip(avail - need - target);
            self.stats.skips.fetch_add(1, Ordering::Relaxed);
        }

        let slack = self.rx.slots().saturating_sub(need);
        self.window_min = self.window_min.min(slack);
        self.window_left = self.window_left.saturating_sub(out.len());
        if self.window_left == 0 {
            let error = self.window_min as f64 - target as f64;
            self.trim = (error / (HORIZON_SECS * self.in_rate)).clamp(-MAX_TRIM, MAX_TRIM);
            self.window_left = self.window;
            self.window_min = usize::MAX;
        }

        for o in out.iter_mut() {
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                self.hist.rotate_left(1);
                // Holds the last sample if the arithmetic needed one more
                // than `need` counted.
                if let Ok(x) = self.rx.pop() {
                    self.hist[3] = x;
                }
            }
            *o = cubic(&self.hist, self.frac as f32);
            self.frac += step;
        }
    }

    fn skip(&mut self, n: usize) {
        if let Ok(chunk) = self.rx.read_chunk(n.min(self.rx.slots())) {
            chunk.commit_all();
        }
    }
}

/// Catmull-Rom interpolation at `t` between `h[1]` and `h[2]`.
fn cubic(h: &[f32; 4], t: f32) -> f32 {
    let a = -0.5 * h[0] + 1.5 * h[1] - 1.5 * h[2] + 0.5 * h[3];
    let b = h[0] - 2.5 * h[1] + 2.0 * h[2] - 0.5 * h[3];
    let c = 0.5 * (h[2] - h[0]);
    ((a * t + b) * t + c) * t + h[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    struct Run {
        out: Vec<f32>,
        /// Buffered input after each read, in seconds.
        levels: Vec<f64>,
        stats: Arc<Stats>,
    }

    /// Plays a sine through a bridge for `secs`, with each device calling
    /// back on its own clock. `in_ppm` is how fast the input clock runs
    /// relative to the output clock.
    fn simulate(
        in_rate: u32,
        in_block: usize,
        out_rate: u32,
        out_block: usize,
        in_ppm: f64,
        secs: f64,
    ) -> Run {
        let (mut feeder, mut reader) = bridge(in_rate, out_rate);
        let stats = reader.stats();
        let in_period = in_block as f64 / (in_rate as f64 * (1.0 + in_ppm * 1e-6));
        let out_period = out_block as f64 / out_rate as f64;
        // Start the output mid-way through an input period.
        let (mut t_in, mut t_out) = (0.0, in_period * 0.37);
        let mut phase = 0.0f64;
        let freq = 440.0;
        let mut run = Run {
            out: Vec::new(),
            levels: Vec::new(),
            stats,
        };
        let mut block = vec![0.0; out_block];
        while t_out < secs {
            if t_in <= t_out {
                let samples: Vec<f32> = (0..in_block)
                    .map(|_| {
                        phase += TAU * freq / in_rate as f64;
                        (0.5 * phase.sin()) as f32
                    })
                    .collect();
                feeder.push(samples.into_iter());
                t_in += in_period;
            } else {
                reader.fill(&mut block);
                run.out.extend_from_slice(&block);
                run.levels.push(reader.rx.slots() as f64 / in_rate as f64);
                t_out += out_period;
            }
        }
        run
    }

    /// The largest jump away from a smooth sine: a dropout or skip shows up
    /// as a big residual, interpolation error as a tiny one.
    fn max_glitch(out: &[f32], out_rate: u32, freq: f64) -> f32 {
        let k = 2.0 * (TAU * freq / out_rate as f64).cos() as f32;
        out.windows(3)
            .map(|w| (w[2] - k * w[1] + w[0]).abs())
            .fold(0.0, f32::max)
    }

    fn settled(run: &Run, out_rate: u32, skip_secs: f64) -> &[f32] {
        &run.out[(out_rate as f64 * skip_secs) as usize..]
    }

    #[test]
    fn fast_input_clock_stays_glitch_free() {
        let run = simulate(48_000, 512, 48_000, 512, 150.0, 300.0);
        assert_eq!(run.stats.dropouts.load(Ordering::Relaxed), 0);
        assert_eq!(run.stats.skips.load(Ordering::Relaxed), 0);
        assert!(max_glitch(settled(&run, 48_000, 0.1), 48_000, 440.0) < 1e-3);
        let late = &run.levels[run.levels.len() / 2..];
        let max = late.iter().cloned().fold(0.0, f64::max);
        assert!(max < 0.03, "buffer grew to {max} s");
    }

    #[test]
    fn slow_input_clock_stays_glitch_free() {
        let run = simulate(48_000, 512, 48_000, 512, -400.0, 300.0);
        assert_eq!(run.stats.dropouts.load(Ordering::Relaxed), 0);
        assert_eq!(run.stats.skips.load(Ordering::Relaxed), 0);
        assert!(max_glitch(settled(&run, 48_000, 0.1), 48_000, 440.0) < 1e-3);
    }

    #[test]
    fn converts_sample_rates() {
        // A 44.1 kHz mic into a 48 kHz device, with mismatched block sizes.
        let run = simulate(44_100, 441, 48_000, 256, 80.0, 60.0);
        assert_eq!(run.stats.dropouts.load(Ordering::Relaxed), 0);
        let out = settled(&run, 48_000, 0.1);
        assert!(max_glitch(out, 48_000, 440.0) < 1e-3);
        // Still a 440 Hz sine at the new rate: count rising zero crossings.
        let crossings = out.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let seconds = out.len() as f64 / 48_000.0;
        let freq = crossings as f64 / seconds;
        assert!((freq - 440.0).abs() < 1.0, "came out at {freq} Hz");
    }

    #[test]
    fn delay_stays_small() {
        let run = simulate(48_000, 512, 48_000, 512, 0.0, 10.0);
        let late = &run.levels[run.levels.len() / 2..];
        let mean = late.iter().sum::<f64>() / late.len() as f64;
        // One 10.7 ms block plus the 2 ms margin, give or take a block.
        assert!(mean < 0.025, "{mean} s buffered");
    }

    #[test]
    fn silent_until_primed() {
        let (mut feeder, mut reader) = bridge(48_000, 48_000);
        let mut out = [1.0; 64];
        reader.fill(&mut out);
        assert!(out.iter().all(|&x| x == 0.0));
        for _ in 0..8 {
            feeder.push(std::iter::repeat_n(0.25, 64));
        }
        reader.fill(&mut out);
        assert!(out[10..].iter().all(|&x| (x - 0.25).abs() < 1e-6));
    }

    #[test]
    fn backlog_is_skipped_on_start() {
        let (mut feeder, mut reader) = bridge(48_000, 48_000);
        // The output device starts late, with 300 ms already buffered.
        for _ in 0..(48_000 * 3 / 10 / 480) {
            feeder.push(std::iter::repeat_n(0.0, 480));
        }
        let mut out = [0.0; 480];
        reader.fill(&mut out);
        assert!(reader.rx.slots() < 480 * 2);
    }

    #[test]
    fn recovers_from_a_stalled_input() {
        let (mut feeder, mut reader) = bridge(48_000, 48_000);
        let mut out = [0.0; 480];
        for i in 0..400 {
            // The mic delivers nothing for 100 ms in the middle.
            if !(100..110).contains(&i) {
                feeder.push(std::iter::repeat_n(0.1, 480));
            }
            reader.fill(&mut out);
        }
        assert_eq!(reader.stats.dropouts.load(Ordering::Relaxed), 1);
        assert!(reader.running);
        assert!(out.iter().all(|&x| (x - 0.1).abs() < 1e-6));
    }
}
