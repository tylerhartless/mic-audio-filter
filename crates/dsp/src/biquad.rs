//! Second-order IIR filters using the RBJ "Audio EQ Cookbook" formulas.
//! Coefficients and state are f64 so low-frequency filters stay stable.

use crate::settings::BandKind;
use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coeffs {
    pub const IDENTITY: Coeffs = Coeffs {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    pub fn new(kind: BandKind, sample_rate: f32, freq_hz: f32, q: f32, gain_db: f32) -> Coeffs {
        let fs = sample_rate as f64;
        let f0 = (freq_hz as f64).clamp(1.0, fs * 0.49);
        let q = (q as f64).max(0.01);
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let w0 = 2.0 * PI * f0 / fs;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

        let (b0, b1, b2, a0, a1, a2) = match kind {
            BandKind::Peaking => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            BandKind::LowShelf => (
                a * ((a + 1.0) - (a - 1.0) * cos + sqrt_a_alpha),
                2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                a * ((a + 1.0) - (a - 1.0) * cos - sqrt_a_alpha),
                (a + 1.0) + (a - 1.0) * cos + sqrt_a_alpha,
                -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                (a + 1.0) + (a - 1.0) * cos - sqrt_a_alpha,
            ),
            BandKind::HighShelf => (
                a * ((a + 1.0) + (a - 1.0) * cos + sqrt_a_alpha),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                a * ((a + 1.0) + (a - 1.0) * cos - sqrt_a_alpha),
                (a + 1.0) - (a - 1.0) * cos + sqrt_a_alpha,
                2.0 * ((a - 1.0) - (a + 1.0) * cos),
                (a + 1.0) - (a - 1.0) * cos - sqrt_a_alpha,
            ),
            BandKind::LowPass => (
                (1.0 - cos) / 2.0,
                1.0 - cos,
                (1.0 - cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BandKind::HighPass => (
                (1.0 + cos) / 2.0,
                -(1.0 + cos),
                (1.0 + cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            BandKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        };

        Coeffs {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    /// Magnitude response in dB at `freq_hz`. Used by tests and for sanity
    /// checks; not called on the audio thread.
    pub fn magnitude_db(&self, sample_rate: f32, freq_hz: f32) -> f64 {
        let w = 2.0 * PI * freq_hz as f64 / sample_rate as f64;
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        // H(e^jw) = (b0 + b1 e^-jw + b2 e^-2jw) / (1 + a1 e^-jw + a2 e^-2jw)
        let num_re = self.b0 + self.b1 * c1 + self.b2 * c2;
        let num_im = -(self.b1 * s1 + self.b2 * s2);
        let den_re = 1.0 + self.a1 * c1 + self.a2 * c2;
        let den_im = -(self.a1 * s1 + self.a2 * s2);
        let mag2 = (num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im);
        10.0 * mag2.log10()
    }
}

/// One biquad section in transposed direct form II.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    c: Coeffs,
    z1: f64,
    z2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Self {
            c: Coeffs::IDENTITY,
            z1: 0.0,
            z2: 0.0,
        }
    }
}

impl Biquad {
    /// Swap coefficients but keep the filter's memory, so live edits don't
    /// restart the filter from silence.
    pub fn set_coeffs(&mut self, c: Coeffs) {
        self.c = c;
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let c = &self.c;
        let y = c.b0 * x + self.z1;
        self.z1 = c.b1 * x - c.a1 * y + self.z2;
        self.z2 = c.b2 * x - c.a2 * y;
        y as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    #[test]
    fn peaking_hits_its_gain_at_center() {
        let c = Coeffs::new(BandKind::Peaking, FS, 1000.0, 1.0, 6.0);
        assert!((c.magnitude_db(FS, 1000.0) - 6.0).abs() < 0.01);
        assert!(c.magnitude_db(FS, 50.0).abs() < 0.2);
    }

    #[test]
    fn highpass_cuts_rumble_and_passes_voice() {
        let c = Coeffs::new(
            BandKind::HighPass,
            FS,
            80.0,
            std::f32::consts::FRAC_1_SQRT_2,
            0.0,
        );
        assert!(c.magnitude_db(FS, 20.0) < -20.0);
        assert!((c.magnitude_db(FS, 80.0) + 3.0).abs() < 0.1);
        assert!(c.magnitude_db(FS, 1000.0).abs() < 0.1);
    }

    #[test]
    fn shelves_reach_full_gain_away_from_corner() {
        let low = Coeffs::new(BandKind::LowShelf, FS, 200.0, 0.707, 4.0);
        assert!((low.magnitude_db(FS, 20.0) - 4.0).abs() < 0.1);
        assert!(low.magnitude_db(FS, 10_000.0).abs() < 0.1);
        let high = Coeffs::new(BandKind::HighShelf, FS, 6000.0, 0.707, -3.0);
        assert!((high.magnitude_db(FS, 20_000.0) + 3.0).abs() < 0.2);
        assert!(high.magnitude_db(FS, 100.0).abs() < 0.1);
    }

    #[test]
    fn processing_matches_analytic_response() {
        let c = Coeffs::new(BandKind::Peaking, FS, 1000.0, 2.0, -9.0);
        let mut f = Biquad::default();
        f.set_coeffs(c);
        let mut peak = 0.0f32;
        for n in 0..48_000 {
            let x = (2.0 * std::f32::consts::PI * 1000.0 * n as f32 / FS).sin();
            let y = f.process(x);
            if n > 24_000 {
                peak = peak.max(y.abs());
            }
        }
        let measured_db = 20.0 * peak.log10() as f64;
        assert!((measured_db - c.magnitude_db(FS, 1000.0)).abs() < 0.1);
    }
}
