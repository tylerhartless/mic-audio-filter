//! The full voice chain:
//! input gain -> high-pass -> RNNoise -> EQ -> compressor -> output gain -> limiter.
//!
//! [`ChainParams`] is computed off the audio thread from [`ChainSettings`] and
//! is `Copy`, so handing a new one to [`Chain::set_params`] never allocates.

use crate::biquad::{Biquad, Coeffs};
use crate::denoise::{DenoiseParams, Denoiser};
use crate::dynamics::{
    db_to_lin, lin_to_db, Compressor, CompressorParams, Limiter, LimiterParams, SmoothedGain,
};
use crate::settings::{BandKind, ChainSettings, MAX_EQ_BANDS};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// RNNoise only runs at 48 kHz, so the whole chain does too. Backends must
/// deliver audio at this rate.
pub const SAMPLE_RATE: u32 = 48_000;
const FS: f32 = SAMPLE_RATE as f32;

#[derive(Debug, Clone, Copy)]
pub struct ChainParams {
    input_gain: f32,
    output_gain: f32,
    highpass: Option<Coeffs>,
    denoise: DenoiseParams,
    eq: [Coeffs; MAX_EQ_BANDS],
    eq_len: usize,
    compressor: CompressorParams,
    limiter: LimiterParams,
}

impl ChainParams {
    /// Settings should already have passed [`ChainSettings::validate`]; any
    /// EQ bands past [`MAX_EQ_BANDS`] are dropped.
    pub fn new(s: &ChainSettings) -> Self {
        let mut eq = [Coeffs::IDENTITY; MAX_EQ_BANDS];
        let mut eq_len = 0;
        if s.eq.enabled {
            for (slot, band) in eq.iter_mut().zip(&s.eq.bands) {
                *slot = Coeffs::new(band.kind, FS, band.freq_hz, band.q, band.gain_db);
                eq_len += 1;
            }
        }
        Self {
            input_gain: db_to_lin(s.gain.input_db),
            output_gain: db_to_lin(s.gain.output_db),
            highpass: s.highpass.enabled.then(|| {
                Coeffs::new(
                    BandKind::HighPass,
                    FS,
                    s.highpass.freq_hz,
                    std::f32::consts::FRAC_1_SQRT_2,
                    0.0,
                )
            }),
            denoise: DenoiseParams::new(&s.denoise, FS),
            eq,
            eq_len,
            compressor: CompressorParams::new(&s.compressor, FS),
            limiter: LimiterParams::new(&s.limiter, FS),
        }
    }
}

/// Levels published by the audio thread for display. Each reading is the
/// maximum since the last [`Meters::take`].
#[derive(Debug, Default)]
pub struct Meters {
    input_peak: AtomicU32,
    output_peak: AtomicU32,
    reduction_db: AtomicU32,
    vad: AtomicU32,
}

#[derive(Debug, Clone, Copy)]
pub struct MeterReading {
    /// Measured after the input gain, so it shows the effect of the boost.
    pub input_peak_db: f32,
    pub output_peak_db: f32,
    pub reduction_db: f32,
    pub vad: f32,
}

impl Meters {
    // Non-negative f32 bit patterns order the same as the floats, so
    // fetch_max on the bits is a lock-free float max.
    fn bump(slot: &AtomicU32, v: f32) {
        if v.is_finite() && v > 0.0 {
            slot.fetch_max(v.to_bits(), Ordering::Relaxed);
        }
    }

    fn take_one(slot: &AtomicU32) -> f32 {
        f32::from_bits(slot.swap(0, Ordering::Relaxed))
    }

    pub fn take(&self) -> MeterReading {
        MeterReading {
            input_peak_db: lin_to_db(Self::take_one(&self.input_peak)),
            output_peak_db: lin_to_db(Self::take_one(&self.output_peak)),
            reduction_db: Self::take_one(&self.reduction_db),
            vad: Self::take_one(&self.vad),
        }
    }
}

pub struct Chain {
    params: ChainParams,
    input_gain: SmoothedGain,
    output_gain: SmoothedGain,
    highpass: Biquad,
    denoiser: Denoiser,
    eq: [Biquad; MAX_EQ_BANDS],
    compressor: Compressor,
    limiter: Limiter,
    meters: Arc<Meters>,
}

impl Chain {
    /// Allocates; build the chain before handing it to the audio thread.
    pub fn new(params: ChainParams, meters: Arc<Meters>) -> Self {
        let mut chain = Self {
            params,
            input_gain: SmoothedGain::new(params.input_gain, FS),
            output_gain: SmoothedGain::new(params.output_gain, FS),
            highpass: Biquad::default(),
            denoiser: Denoiser::new(),
            eq: [Biquad::default(); MAX_EQ_BANDS],
            compressor: Compressor::default(),
            limiter: Limiter::default(),
            meters,
        };
        chain.set_params(params);
        chain
    }

    /// Real-time safe: copies parameters, keeps filter memory.
    pub fn set_params(&mut self, p: ChainParams) {
        if p.denoise.enabled && !self.params.denoise.enabled {
            self.denoiser.reset();
        }
        if p.highpass.is_some() && self.params.highpass.is_none() {
            self.highpass.reset();
        }
        if let Some(c) = p.highpass {
            self.highpass.set_coeffs(c);
        }
        for (i, band) in self.eq.iter_mut().enumerate() {
            if i >= self.params.eq_len {
                band.reset();
            }
            band.set_coeffs(p.eq[i]);
        }
        if !p.compressor.enabled {
            self.compressor.reset();
        }
        self.input_gain.set_target(p.input_gain);
        self.output_gain.set_target(p.output_gain);
        self.params = p;
    }

    /// Delay the chain adds, in samples.
    pub fn latency_samples(&self) -> usize {
        if self.params.denoise.enabled {
            self.denoiser.latency_samples()
        } else {
            0
        }
    }

    /// Processes one block of mono samples in place. Any block size works.
    pub fn process(&mut self, buf: &mut [f32]) {
        let p = self.params;
        let mut input_peak = 0.0f32;
        for s in buf.iter_mut() {
            let mut x = *s * self.input_gain.tick();
            input_peak = input_peak.max(x.abs());
            if p.highpass.is_some() {
                x = self.highpass.process(x);
            }
            *s = x;
        }

        if p.denoise.enabled {
            self.denoiser.process(&p.denoise, buf);
        }

        let mut output_peak = 0.0f32;
        let mut reduction = 0.0f32;
        let mut finite = true;
        for s in buf.iter_mut() {
            let mut x = *s;
            for band in &mut self.eq[..p.eq_len] {
                x = band.process(x);
            }
            if p.compressor.enabled {
                x = self.compressor.process(&p.compressor, x);
                reduction = reduction.max(self.compressor.reduction_db());
            }
            x *= self.output_gain.tick();
            if p.limiter.enabled {
                x = self.limiter.process(&p.limiter, x);
            }
            finite &= x.is_finite();
            let x = x.clamp(-1.0, 1.0);
            output_peak = output_peak.max(x.abs());
            *s = x;
        }

        if !finite {
            // A NaN would otherwise stick in the filter memory forever.
            buf.fill(0.0);
            self.reset_state();
        }

        Meters::bump(&self.meters.input_peak, input_peak);
        Meters::bump(&self.meters.output_peak, output_peak);
        Meters::bump(&self.meters.reduction_db, reduction);
        if p.denoise.enabled {
            Meters::bump(&self.meters.vad, self.denoiser.last_vad());
        }
    }

    fn reset_state(&mut self) {
        self.highpass.reset();
        self.eq.iter_mut().for_each(Biquad::reset);
        self.denoiser.reset();
        self.compressor.reset();
        self.limiter.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::*;

    fn bypass() -> ChainSettings {
        let mut s = ChainSettings::default();
        s.highpass.enabled = false;
        s.denoise.enabled = false;
        s.compressor.enabled = false;
        s.limiter.enabled = false;
        s
    }

    fn sine(len: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|n| amp * (2.0 * std::f32::consts::PI * freq * n as f32 / FS).sin())
            .collect()
    }

    #[test]
    fn bypass_chain_is_transparent() {
        let mut chain = Chain::new(ChainParams::new(&bypass()), Arc::default());
        let input = sine(4800, 440.0, 0.5);
        let mut buf = input.clone();
        chain.process(&mut buf);
        assert_eq!(chain.latency_samples(), 0);
        for (a, b) in input.iter().zip(&buf) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn input_gain_boosts() {
        let mut s = bypass();
        s.gain.input_db = 12.0;
        let mut chain = Chain::new(ChainParams::new(&s), Arc::default());
        let mut buf = sine(4800, 440.0, 0.1);
        chain.process(&mut buf);
        let peak = buf[2400..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((lin_to_db(peak) - lin_to_db(0.1 * db_to_lin(12.0))).abs() < 0.1);
    }

    #[test]
    fn default_chain_never_clips() {
        let mut s = ChainSettings::default();
        s.gain.input_db = 30.0;
        let mut chain = Chain::new(ChainParams::new(&s), Arc::default());
        let mut buf = sine(48_000, 300.0, 0.9);
        for chunk in buf.chunks_mut(1024) {
            chain.process(chunk);
        }
        let ceiling = db_to_lin(s.limiter.ceiling_db);
        assert!(buf.iter().all(|x| x.abs() <= ceiling + 1e-6));
    }

    #[test]
    fn meters_report_levels_and_reset() {
        let meters = Arc::new(Meters::default());
        let mut chain = Chain::new(ChainParams::new(&bypass()), meters.clone());
        let mut buf = sine(4800, 440.0, 0.5);
        chain.process(&mut buf);
        let r = meters.take();
        assert!((r.input_peak_db - lin_to_db(0.5)).abs() < 0.1);
        assert!((r.output_peak_db - lin_to_db(0.5)).abs() < 0.1);
        assert!(meters.take().input_peak_db < -150.0);
    }

    #[test]
    fn live_param_change_applies() {
        let mut chain = Chain::new(ChainParams::new(&bypass()), Arc::default());
        let mut s = bypass();
        s.gain.output_db = -6.0;
        chain.set_params(ChainParams::new(&s));
        let mut buf = sine(48_000, 440.0, 0.5);
        chain.process(&mut buf);
        let peak = buf[24_000..].iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((lin_to_db(peak) - (lin_to_db(0.5) - 6.0)).abs() < 0.1);
    }

    #[test]
    fn default_settings_validate() {
        assert!(ChainSettings::default().validate(FS).is_ok());
        let mut s = ChainSettings::default();
        s.compressor.ratio = 0.5;
        s.highpass.freq_hz = 5.0;
        assert_eq!(s.validate(FS).unwrap_err().len(), 2);
    }
}
