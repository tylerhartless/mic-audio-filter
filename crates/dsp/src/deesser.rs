//! De-esser: turns down harsh "s" and "sh" sounds.
//!
//! A band-pass filter picks out the sibilance band. When that band is louder
//! than the threshold, a scaled copy of it is subtracted from the signal,
//! which cuts just that band (a band-pass subtracted from its input is a
//! notch, so a partial subtraction is a partial cut). Everything outside the
//! band passes untouched, and nothing happens below the threshold.

use crate::biquad::{Biquad, Coeffs};
use crate::dynamics::{db_to_lin, lin_to_db, time_coef};
use crate::settings::DeEsserSettings;

/// Width of the watched band. At 6.5 kHz this spans roughly 4.5 to 9 kHz.
const BAND_Q: f32 = 1.4;
/// Above the threshold, the band is turned down by this fraction of the
/// overshoot (a 4:1 ratio).
const SLOPE: f32 = 0.75;
const ATTACK_MS: f32 = 1.0;
const RELEASE_MS: f32 = 60.0;

#[derive(Debug, Clone, Copy)]
pub struct DeEsserParams {
    pub enabled: bool,
    band: Coeffs,
    threshold_db: f32,
    max_reduction_db: f32,
    attack: f32,
    release: f32,
}

impl DeEsserParams {
    pub fn new(s: &DeEsserSettings, sample_rate: f32) -> Self {
        Self {
            enabled: s.enabled,
            band: Coeffs::band_pass(sample_rate, s.freq_hz, BAND_Q),
            threshold_db: s.threshold_db,
            max_reduction_db: s.max_reduction_db.max(0.0),
            attack: time_coef(ATTACK_MS, sample_rate),
            release: time_coef(RELEASE_MS, sample_rate),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DeEsser {
    band: Biquad,
    /// Current cut in dB, zero or negative.
    gain_db: f32,
}

impl DeEsser {
    /// Loads new parameters, keeping the filter's memory.
    pub fn configure(&mut self, p: &DeEsserParams) {
        self.band.set_coeffs(p.band);
    }

    #[inline]
    pub fn process(&mut self, p: &DeEsserParams, x: f32) -> f32 {
        let band = self.band.process(x);
        let over = lin_to_db(band.abs()) - p.threshold_db;
        let target = if over > 0.0 {
            -(over * SLOPE).min(p.max_reduction_db)
        } else {
            0.0
        };
        let coef = if target < self.gain_db {
            p.attack
        } else {
            p.release
        };
        self.gain_db = target + (self.gain_db - target) * coef;
        x - (1.0 - db_to_lin(self.gain_db)) * band
    }

    /// Current cut in dB, as a positive number.
    pub fn reduction_db(&self) -> f32 {
        -self.gain_db
    }

    pub fn reset(&mut self) {
        self.band.reset();
        self.gain_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    fn params(threshold_db: f32, max_reduction_db: f32) -> DeEsserParams {
        DeEsserParams::new(
            &DeEsserSettings {
                enabled: true,
                freq_hz: 6500.0,
                threshold_db,
                max_reduction_db,
            },
            FS,
        )
    }

    /// Peak level in dB of a sine after it has run through the de-esser.
    fn output_db(p: &DeEsserParams, freq: f32, amp: f32) -> f32 {
        let mut d = DeEsser::default();
        d.configure(p);
        let mut peak = 0.0f32;
        for n in 0..48_000 {
            let x = amp * (2.0 * std::f32::consts::PI * freq * n as f32 / FS).sin();
            let y = d.process(p, x);
            if n > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        lin_to_db(peak)
    }

    #[test]
    fn loud_sibilance_is_cut_up_to_the_limit() {
        let input_db = lin_to_db(0.3);
        let cut = input_db - output_db(&params(-34.0, 9.0), 6500.0, 0.3);
        assert!((cut - 9.0).abs() < 0.5, "cut {cut:.1} dB");
    }

    #[test]
    fn voice_band_passes_untouched() {
        let input_db = lin_to_db(0.3);
        let out = output_db(&params(-34.0, 9.0), 300.0, 0.3);
        assert!(
            (out - input_db).abs() < 0.3,
            "300 Hz changed by {:.1} dB",
            out - input_db
        );
    }

    #[test]
    fn quiet_sibilance_is_left_alone() {
        let input_db = lin_to_db(0.005);
        let out = output_db(&params(-34.0, 9.0), 6500.0, 0.005);
        assert!((out - input_db).abs() < 0.1);
    }
}
