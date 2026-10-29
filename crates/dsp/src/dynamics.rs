//! Compressor, limiter and gain smoothing.

use crate::settings::{CompressorSettings, LimiterSettings};

/// One-pole smoothing coefficient for a time constant in milliseconds.
pub fn time_coef(ms: f32, sample_rate: f32) -> f32 {
    (-1.0 / (ms.max(0.01) * 0.001 * sample_rate)).exp()
}

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-9).log10()
}

/// A gain that glides smoothly to each new target, so live config
/// edits don't click.
#[derive(Debug, Clone, Copy)]
pub struct SmoothedGain {
    current: f32,
    target: f32,
    coef: f32,
}

impl SmoothedGain {
    pub fn new(lin: f32, sample_rate: f32) -> Self {
        Self {
            current: lin,
            target: lin,
            coef: time_coef(20.0, sample_rate),
        }
    }

    pub fn set_target(&mut self, lin: f32) {
        self.target = lin;
    }

    #[inline]
    pub fn tick(&mut self) -> f32 {
        self.current = self.target + (self.current - self.target) * self.coef;
        self.current
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CompressorParams {
    pub enabled: bool,
    threshold_db: f32,
    slope: f32,
    knee_db: f32,
    makeup_db: f32,
    attack: f32,
    release: f32,
}

impl CompressorParams {
    pub fn new(s: &CompressorSettings, sample_rate: f32) -> Self {
        Self {
            enabled: s.enabled,
            threshold_db: s.threshold_db,
            slope: 1.0 / s.ratio.max(1.0) - 1.0,
            knee_db: s.knee_db.max(0.0),
            makeup_db: s.makeup_db,
            attack: time_coef(s.attack_ms, sample_rate),
            release: time_coef(s.release_ms, sample_rate),
        }
    }

    /// Static soft-knee curve: gain change in dB (zero or negative) for an
    /// input level in dB.
    #[inline]
    fn gain_change_db(&self, x_db: f32) -> f32 {
        let over = x_db - self.threshold_db;
        let half_knee = self.knee_db / 2.0;
        if over <= -half_knee {
            0.0
        } else if over < half_knee {
            let k = over + half_knee;
            self.slope * k * k / (2.0 * self.knee_db)
        } else {
            self.slope * over
        }
    }
}

/// Feed-forward compressor with attack/release smoothing in the dB domain.
#[derive(Debug, Clone, Copy, Default)]
pub struct Compressor {
    gr_db: f32,
}

impl Compressor {
    #[inline]
    pub fn process(&mut self, p: &CompressorParams, x: f32) -> f32 {
        let target = p.gain_change_db(lin_to_db(x.abs()));
        let coef = if target < self.gr_db {
            p.attack
        } else {
            p.release
        };
        self.gr_db = target + (self.gr_db - target) * coef;
        x * db_to_lin(self.gr_db + p.makeup_db)
    }

    /// Current gain reduction in dB, as a positive number.
    pub fn reduction_db(&self) -> f32 {
        -self.gr_db
    }

    pub fn reset(&mut self) {
        self.gr_db = 0.0;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct LimiterParams {
    pub enabled: bool,
    ceiling: f32,
    release: f32,
}

impl LimiterParams {
    pub fn new(s: &LimiterSettings, sample_rate: f32) -> Self {
        Self {
            enabled: s.enabled,
            ceiling: db_to_lin(s.ceiling_db),
            release: time_coef(s.release_ms, sample_rate),
        }
    }
}

/// Peak limiter with instant attack: output never exceeds the ceiling.
/// It sits after the compressor as a safety net, so it should rarely act.
#[derive(Debug, Clone, Copy)]
pub struct Limiter {
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self { gain: 1.0 }
    }
}

impl Limiter {
    #[inline]
    pub fn process(&mut self, p: &LimiterParams, x: f32) -> f32 {
        let level = x.abs();
        let needed = if level > p.ceiling {
            p.ceiling / level
        } else {
            1.0
        };
        self.gain = if needed < self.gain {
            needed
        } else {
            needed + (self.gain - needed) * p.release
        };
        x * self.gain
    }

    pub fn reset(&mut self) {
        self.gain = 1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: f32 = 48_000.0;

    fn comp(threshold_db: f32, ratio: f32, knee_db: f32) -> CompressorParams {
        CompressorParams::new(
            &CompressorSettings {
                enabled: true,
                threshold_db,
                ratio,
                knee_db,
                attack_ms: 1.0,
                release_ms: 100.0,
                makeup_db: 0.0,
            },
            FS,
        )
    }

    #[test]
    fn static_curve_follows_ratio_above_threshold() {
        let p = comp(-20.0, 4.0, 0.0);
        assert_eq!(p.gain_change_db(-30.0), 0.0);
        // 12 dB over a 4:1 threshold should come out 3 dB over: -9 dB change.
        assert!((p.gain_change_db(-8.0) + 9.0).abs() < 1e-4);
    }

    #[test]
    fn soft_knee_is_continuous() {
        let p = comp(-20.0, 4.0, 10.0);
        let below = p.gain_change_db(-25.0 - 1e-4);
        let at_lower_edge = p.gain_change_db(-25.0 + 1e-4);
        let at_upper_edge = p.gain_change_db(-15.0 - 1e-4);
        let above = p.gain_change_db(-15.0 + 1e-4);
        assert!((below - at_lower_edge).abs() < 1e-3);
        assert!((at_upper_edge - above).abs() < 1e-3);
    }

    #[test]
    fn compressor_settles_to_static_curve() {
        let p = comp(-20.0, 4.0, 0.0);
        let mut c = Compressor::default();
        let x = db_to_lin(-8.0);
        let mut y = 0.0;
        for _ in 0..48_000 {
            y = c.process(&p, x);
        }
        assert!((lin_to_db(y) - (-17.0)).abs() < 0.05);
        assert!((c.reduction_db() - 9.0).abs() < 0.05);
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let p = LimiterParams::new(
            &LimiterSettings {
                enabled: true,
                ceiling_db: -1.0,
                release_ms: 50.0,
            },
            FS,
        );
        let mut l = Limiter::default();
        let ceiling = db_to_lin(-1.0);
        for n in 0..4800 {
            let x = 3.0 * (n as f32 * 0.07).sin();
            assert!(l.process(&p, x).abs() <= ceiling + 1e-6);
        }
    }
}
