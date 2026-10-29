//! The TOML config file: which mic to read, what to call the virtual mic,
//! and the processing chain settings.

use anyhow::{bail, Context, Result};
use mic_filter_dsp::{
    ChainSettings, CompressorSettings, DenoiseSettings, EqSettings, GainSettings, HighpassSettings,
    LimiterSettings, SAMPLE_RATE,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub input: InputConfig,
    pub output: OutputConfig,
    pub gain: GainSettings,
    pub highpass: HighpassSettings,
    pub denoise: DenoiseSettings,
    pub eq: EqSettings,
    pub compressor: CompressorSettings,
    pub limiter: LimiterSettings,
}

/// The `[input] device` value that follows the system default mic.
pub const DEFAULT_DEVICE: &str = "default";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputConfig {
    /// PipeWire node.name of the mic, or [`DEFAULT_DEVICE`].
    pub device: String,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            device: DEFAULT_DEVICE.into(),
        }
    }
}

impl InputConfig {
    /// The node to capture from. `None` follows the system default mic,
    /// which PipeWire never resolves to this program's own output.
    pub fn target(&self) -> Option<&str> {
        let device = self.device.trim();
        (!device.is_empty() && device != DEFAULT_DEVICE).then_some(device)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// PipeWire node.name of the virtual mic.
    pub name: String,
    /// The name apps show in their mic dropdowns.
    pub description: String,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: "mic_filter".into(),
            description: "Mic Filter".into(),
        }
    }
}

impl Config {
    pub fn chain(&self) -> ChainSettings {
        ChainSettings {
            gain: self.gain.clone(),
            highpass: self.highpass.clone(),
            denoise: self.denoise.clone(),
            eq: self.eq.clone(),
            compressor: self.compressor.clone(),
            limiter: self.limiter.clone(),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        let config: Config = toml::from_str(text)?;
        if let Err(errs) = config.chain().validate(SAMPLE_RATE as f32) {
            bail!("invalid settings:\n  {}", errs.join("\n  "));
        }
        if config.output.name.trim().is_empty() {
            bail!("output.name must not be empty");
        }
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("in {}", path.display()))
    }
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("mic-filter").join("config.toml")
}

/// Sets `[input] device` in the config file, leaving everything else in it,
/// comments included, as it was. Creates the file if it doesn't exist.
pub fn set_input_device(path: &Path, device: &str) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => {
            with_input_device(&text, device).with_context(|| format!("in {}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => template(Some(device)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    // Never write a file that the running instance would then reject.
    Config::parse(&text).with_context(|| format!("in {}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn with_input_device(text: &str, device: &str) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    let input = doc
        .entry("input")
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .context("[input] is not a table")?;
    match input.get_mut("device").and_then(|item| item.as_value_mut()) {
        Some(value) => {
            // Keep any comment written after the old value.
            let decor = value.decor().clone();
            *value = device.into();
            *value.decor_mut() = decor;
        }
        None => {
            input.insert("device", toml_edit::value(device));
        }
    }
    Ok(doc.to_string())
}

/// A commented starter config. `device` is written in as the input mic;
/// `None` follows the system default.
pub fn template(device: Option<&str>) -> String {
    let device = toml::Value::String(device.unwrap_or(DEFAULT_DEVICE).to_string());
    TEMPLATE.replace("{DEVICE}", &device.to_string())
}

const TEMPLATE: &str = r#"# mic-filter config. Edits apply live while mic-filter is running,
# except [output], which needs a restart.
# Signal chain: gain -> highpass -> denoise -> eq -> compressor -> output gain -> limiter

[input]
# The mic to process. Switch it with `mic-filter use`, or paste a name from
# `mic-filter list`. "default" follows the system default mic.
device = {DEVICE}

[output]
# The virtual mic other apps record from.
name = "mic_filter"
description = "Mic Filter"

[gain]
# Boost for a quiet mic. Watch `mic-filter run --meter`: speech should
# peak around -18 to -10 dB on the "in" side after this boost.
input_db = 0.0
# Final trim before the limiter.
output_db = 0.0

[highpass]
# Removes rumble, desk thumps and breath pops below the voice range.
enabled = true
freq_hz = 80.0

[denoise]
# RNNoise neural noise suppression (fans, keyboards, hum, room noise).
enabled = true
# 1.0 = full suppression. Lower (e.g. 0.85) blends some original back in
# if your voice starts to sound watery.
strength = 1.0
# Voice gate: mute when RNNoise is less sure than this that you're talking.
# 0.0 = off. Try 0.5 to 0.8 to kill residual noise between sentences.
vad_threshold = 0.0
vad_hold_ms = 300.0

[eq]
enabled = true
# Types: peaking, low_shelf, high_shelf, low_pass, high_pass, notch.
# gain_db is ignored by low_pass, high_pass and notch. q defaults to 0.707.
bands = [
    # Headset mics tend to sound boxy around 250 Hz.
    { type = "peaking", freq_hz = 250.0, gain_db = -2.0, q = 1.0 },
    # A little presence helps intelligibility.
    { type = "peaking", freq_hz = 3000.0, gain_db = 2.0, q = 1.0 },
    # { type = "high_shelf", freq_hz = 8000.0, gain_db = 1.5 },
    # { type = "notch", freq_hz = 60.0, q = 10.0 },   # mains hum
]

[compressor]
# Evens out loud and quiet speech so you sit at a consistent level.
enabled = true
threshold_db = -24.0
ratio = 3.0
knee_db = 6.0
attack_ms = 5.0
release_ms = 150.0
makeup_db = 6.0

[limiter]
# Safety net so nothing ever clips.
enabled = true
ceiling_db = -1.0
release_ms = 60.0
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_names_the_device() {
        let c = Config::parse(&template(Some("alsa_input.test"))).unwrap();
        assert_eq!(c.input.target(), Some("alsa_input.test"));
        assert_eq!(c.eq.bands.len(), 2);
        let mut defaults = c.chain();
        defaults.eq = EqSettings::default();
        assert_eq!(
            defaults,
            ChainSettings::default(),
            "template drifted from the code defaults"
        );
    }

    #[test]
    fn template_without_device_follows_default() {
        let c = Config::parse(&template(None)).unwrap();
        assert_eq!(c.input.device, DEFAULT_DEVICE);
        assert_eq!(c.input.target(), None);
    }

    #[test]
    fn switching_device_changes_only_that_value() {
        let before = template(Some("alsa_input.a"));
        assert_eq!(
            with_input_device(&before, "alsa_input.b").unwrap(),
            template(Some("alsa_input.b"))
        );
        assert_eq!(
            with_input_device(&before, DEFAULT_DEVICE).unwrap(),
            template(None)
        );
    }

    #[test]
    fn switching_device_keeps_trailing_comment() {
        let text = "[input]\ndevice = \"a\"  # my headset\n\n[gain]\ninput_db = 6.0\n";
        let after = with_input_device(text, "b").unwrap();
        assert_eq!(
            after,
            "[input]\ndevice = \"b\"  # my headset\n\n[gain]\ninput_db = 6.0\n"
        );
    }

    #[test]
    fn switching_device_adds_missing_section() {
        let after = with_input_device("[gain]\ninput_db = 6.0\n", "alsa_input.x").unwrap();
        let c = Config::parse(&after).unwrap();
        assert_eq!(c.input.target(), Some("alsa_input.x"));
        assert_eq!(c.gain.input_db, 6.0);
    }

    #[test]
    fn set_input_device_creates_and_updates_file() {
        let dir = std::env::temp_dir().join(format!("mic-filter-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        set_input_device(&path, "alsa_input.a").unwrap();
        assert_eq!(
            Config::load(&path).unwrap().input.target(),
            Some("alsa_input.a")
        );
        set_input_device(&path, DEFAULT_DEVICE).unwrap();
        assert_eq!(Config::load(&path).unwrap().input.target(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn typos_are_rejected() {
        let err = Config::parse("[compressor]\nthreshhold_db = -20.0\n").unwrap_err();
        assert!(format!("{err:#}").contains("threshhold_db"));
    }

    #[test]
    fn out_of_range_values_are_rejected() {
        let err = Config::parse("[compressor]\nratio = 0.5\n").unwrap_err();
        assert!(format!("{err:#}").contains("compressor.ratio"));
    }
}
