//! User-facing settings for the processing chain. These are what the config
//! file deserializes into; [`crate::ChainParams`] is the real-time-safe form.

use serde::{Deserialize, Serialize};

/// Most EQ bands a chain will accept. Keeps [`crate::ChainParams`] fixed-size
/// so it can cross into the audio thread without allocating.
pub const MAX_EQ_BANDS: usize = 16;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChainSettings {
    pub gain: GainSettings,
    pub highpass: HighpassSettings,
    pub denoise: DenoiseSettings,
    pub eq: EqSettings,
    pub compressor: CompressorSettings,
    pub deesser: DeEsserSettings,
    pub limiter: LimiterSettings,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GainSettings {
    /// Boost applied to the raw mic before anything else.
    pub input_db: f32,
    /// Trim applied after the compressor, before the limiter.
    pub output_db: f32,
}

impl Default for GainSettings {
    fn default() -> Self {
        Self {
            input_db: 0.0,
            output_db: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HighpassSettings {
    pub enabled: bool,
    pub freq_hz: f32,
}

impl Default for HighpassSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            freq_hz: 80.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DenoiseSettings {
    pub enabled: bool,
    /// 1.0 = fully denoised; lower values blend some of the original back in,
    /// which softens RNNoise's artifacts at the cost of leaving more noise.
    pub strength: f32,
    /// Mute when RNNoise's voice probability is below this (0.0 to 1.0).
    /// 0.0 turns the voice gate off.
    pub vad_threshold: f32,
    /// How long the voice gate stays open after speech stops.
    pub vad_hold_ms: f32,
}

impl Default for DenoiseSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            strength: 1.0,
            vad_threshold: 0.0,
            vad_hold_ms: 300.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EqSettings {
    pub enabled: bool,
    pub bands: Vec<EqBand>,
}

impl Default for EqSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            bands: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqBand {
    #[serde(rename = "type")]
    pub kind: BandKind,
    pub freq_hz: f32,
    /// Ignored by low_pass, high_pass and notch.
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default = "default_q")]
    pub q: f32,
}

fn default_q() -> f32 {
    std::f32::consts::FRAC_1_SQRT_2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BandKind {
    Peaking,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    Notch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CompressorSettings {
    pub enabled: bool,
    pub threshold_db: f32,
    pub ratio: f32,
    pub knee_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

impl Default for CompressorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_db: -24.0,
            ratio: 3.0,
            knee_db: 6.0,
            attack_ms: 5.0,
            release_ms: 150.0,
            makeup_db: 6.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimiterSettings {
    pub enabled: bool,
    pub ceiling_db: f32,
    pub release_ms: f32,
}

/// Turns down harsh "s" and "sh" sounds: a band around `freq_hz` is cut,
/// only while it's louder than `threshold_db`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeEsserSettings {
    pub enabled: bool,
    /// Center of the band to watch. Sibilance sits around 5 to 8 kHz.
    pub freq_hz: f32,
    pub threshold_db: f32,
    /// The most the band is ever turned down.
    pub max_reduction_db: f32,
}

impl Default for DeEsserSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            freq_hz: 6500.0,
            threshold_db: -34.0,
            max_reduction_db: 9.0,
        }
    }
}

impl Default for LimiterSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            ceiling_db: -1.0,
            release_ms: 60.0,
        }
    }
}

impl ChainSettings {
    /// Rejects values that would produce unstable filters or nonsense gain.
    /// Returns every problem found in one pass.
    pub fn validate(&self, sample_rate: f32) -> Result<(), Vec<String>> {
        let mut errs = Vec::new();
        let nyquist = sample_rate / 2.0;
        let mut check = |ok: bool, msg: String| {
            if !ok {
                errs.push(msg);
            }
        };

        for (name, db) in [
            ("gain.input_db", self.gain.input_db),
            ("gain.output_db", self.gain.output_db),
        ] {
            check(
                (-60.0..=40.0).contains(&db),
                format!("{name} = {db} is outside -60..40 dB"),
            );
        }

        let hp = self.highpass.freq_hz;
        check(
            (10.0..=1000.0).contains(&hp),
            format!("highpass.freq_hz = {hp} is outside 10..1000 Hz"),
        );

        let d = &self.denoise;
        check(
            (0.0..=1.0).contains(&d.strength),
            format!("denoise.strength = {} is outside 0..1", d.strength),
        );
        check(
            (0.0..=1.0).contains(&d.vad_threshold),
            format!(
                "denoise.vad_threshold = {} is outside 0..1",
                d.vad_threshold
            ),
        );
        check(
            (0.0..=5000.0).contains(&d.vad_hold_ms),
            format!("denoise.vad_hold_ms = {} is outside 0..5000", d.vad_hold_ms),
        );

        check(
            self.eq.bands.len() <= MAX_EQ_BANDS,
            format!(
                "eq has {} bands; the limit is {MAX_EQ_BANDS}",
                self.eq.bands.len()
            ),
        );
        for (i, b) in self.eq.bands.iter().enumerate() {
            check(
                b.freq_hz >= 10.0 && b.freq_hz < nyquist * 0.98,
                format!(
                    "eq band {} freq_hz = {} is outside 10..{:.0} Hz",
                    i + 1,
                    b.freq_hz,
                    nyquist * 0.98
                ),
            );
            check(
                (0.1..=20.0).contains(&b.q),
                format!("eq band {} q = {} is outside 0.1..20", i + 1, b.q),
            );
            check(
                (-30.0..=30.0).contains(&b.gain_db),
                format!(
                    "eq band {} gain_db = {} is outside -30..30",
                    i + 1,
                    b.gain_db
                ),
            );
        }

        let c = &self.compressor;
        check(
            (-80.0..=0.0).contains(&c.threshold_db),
            format!(
                "compressor.threshold_db = {} is outside -80..0",
                c.threshold_db
            ),
        );
        check(
            (1.0..=50.0).contains(&c.ratio),
            format!("compressor.ratio = {} is outside 1..50", c.ratio),
        );
        check(
            (0.0..=24.0).contains(&c.knee_db),
            format!("compressor.knee_db = {} is outside 0..24", c.knee_db),
        );
        check(
            (0.1..=500.0).contains(&c.attack_ms),
            format!("compressor.attack_ms = {} is outside 0.1..500", c.attack_ms),
        );
        check(
            (1.0..=5000.0).contains(&c.release_ms),
            format!(
                "compressor.release_ms = {} is outside 1..5000",
                c.release_ms
            ),
        );
        check(
            (0.0..=40.0).contains(&c.makeup_db),
            format!("compressor.makeup_db = {} is outside 0..40", c.makeup_db),
        );

        let s = &self.deesser;
        check(
            (2000.0..=12000.0).contains(&s.freq_hz),
            format!("deesser.freq_hz = {} is outside 2000..12000", s.freq_hz),
        );
        check(
            (-60.0..=0.0).contains(&s.threshold_db),
            format!(
                "deesser.threshold_db = {} is outside -60..0",
                s.threshold_db
            ),
        );
        check(
            (0.0..=24.0).contains(&s.max_reduction_db),
            format!(
                "deesser.max_reduction_db = {} is outside 0..24",
                s.max_reduction_db
            ),
        );

        let l = &self.limiter;
        check(
            (-30.0..=0.0).contains(&l.ceiling_db),
            format!("limiter.ceiling_db = {} is outside -30..0", l.ceiling_db),
        );
        check(
            (1.0..=2000.0).contains(&l.release_ms),
            format!("limiter.release_ms = {} is outside 1..2000", l.release_ms),
        );

        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }
}
