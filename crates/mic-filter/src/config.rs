//! The TOML config file: which mic to read, what to call the virtual mic,
//! and the processing chain settings.

use anyhow::{bail, Context, Result};
use mic_filter_dsp::{
    ChainSettings, CompressorSettings, DeEsserSettings, DenoiseSettings, EqSettings, GainSettings,
    HighpassSettings, LimiterSettings, SAMPLE_RATE,
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
    pub deesser: DeEsserSettings,
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
    /// On Linux, the PipeWire node.name of the virtual mic mic-filter
    /// publishes. On macOS, the name or UID of the virtual device it plays
    /// into.
    pub name: String,
    /// The name apps show in their mic dropdowns. Linux only: on macOS the
    /// virtual device names itself.
    #[serde(skip_serializing_if = "unused_description")]
    pub description: String,
}

const DEFAULT_DESCRIPTION: &str = "Mic Filter";

/// macOS has no use for `description`, so saving leaves it out of the file.
fn unused_description(description: &str) -> bool {
    cfg!(cpal_backend) && description == DEFAULT_DESCRIPTION
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            name: if cfg!(cpal_backend) {
                "BlackHole 2ch"
            } else {
                "mic_filter"
            }
            .into(),
            description: DEFAULT_DESCRIPTION.into(),
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
            deesser: self.deesser.clone(),
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
    write_checked(path, &text).map(drop)
}

/// Writes every setting in `config` to the file, changing only the values
/// that differ so the file's comments and layout survive. Creates the file
/// from the template if it doesn't exist.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub fn save(path: &Path, config: &Config) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => template(Some(&config.input.device)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let text = merged(&existing, config).with_context(|| format!("in {}", path.display()))?;
    if write_checked(path, &text)? != *config {
        bail!("saved config did not read back the same; please report this");
    }
    Ok(())
}

/// Validates `text`, then replaces the file atomically, so the running
/// service never reads a half-written or invalid config.
fn write_checked(path: &Path, text: &str) -> Result<Config> {
    let config = Config::parse(text).with_context(|| format!("in {}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(config)
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn merged(existing: &str, config: &Config) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = existing.parse()?;
    let fresh: toml_edit::DocumentMut = toml::to_string(config)?.parse()?;
    merge_table(doc.as_table_mut(), fresh.as_table());
    Ok(doc.to_string())
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn merge_table(dst: &mut dyn toml_edit::TableLike, src: &dyn toml_edit::TableLike) {
    use toml_edit::{Item, Value};
    for (key, new) in src.iter() {
        // serde writes lists of structs as [[tables]]; the config file keeps
        // them as an inline list, like the template.
        let new = match new {
            Item::ArrayOfTables(tables) => Item::Value(Value::Array(inline_array(tables))),
            other => other.clone(),
        };
        match dst.get_mut(key) {
            Some(old) if old.is_table_like() && new.is_table_like() => merge_table(
                old.as_table_like_mut().unwrap(),
                new.as_table_like().unwrap(),
            ),
            Some(Item::Value(old)) => {
                if let Item::Value(new) = new {
                    replace_value(old, new);
                }
            }
            Some(old) => *old = new,
            None => {
                dst.insert(key, new);
            }
        }
    }
}

/// Swaps in `new` unless it's equal to `old`, keeping the comments around
/// the old value.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn replace_value(old: &mut toml_edit::Value, mut new: toml_edit::Value) {
    if normalized(old) == normalized(&new) {
        return;
    }
    if let (Some(old_array), Some(new_array)) = (old.as_array(), new.as_array_mut()) {
        // Keep commented-out entries written after the last item.
        let trailing = old_array.trailing().as_str().unwrap_or("\n").to_string();
        new_array.set_trailing(if trailing.contains('\n') {
            trailing
        } else {
            "\n".into()
        });
    }
    *new.decor_mut() = old.decor().clone();
    *old = new;
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn normalized(value: &toml_edit::Value) -> String {
    let mut value = value.clone();
    value.decor_mut().clear();
    if let Some(array) = value.as_array_mut() {
        array.fmt();
        array.iter_mut().for_each(|v| {
            if let Some(t) = v.as_inline_table_mut() {
                t.fmt();
            }
        });
    }
    value.to_string()
}

/// One inline table per line, matching the template's layout.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
fn inline_array(tables: &toml_edit::ArrayOfTables) -> toml_edit::Array {
    let mut array: toml_edit::Array = tables
        .iter()
        .map(|t| {
            let mut inline = t.clone().into_inline_table();
            inline.fmt();
            toml_edit::Value::InlineTable(inline)
        })
        .collect();
    for item in array.iter_mut() {
        item.decor_mut().set_prefix("\n    ");
    }
    array.set_trailing_comma(true);
    array.set_trailing("\n");
    array
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
    let output = if cfg!(cpal_backend) {
        OUTPUT_CPAL
    } else {
        OUTPUT_PIPEWIRE
    };
    TEMPLATE
        .replace("{DEVICE}", &device.to_string())
        .replace("{OUTPUT}\n", output)
}

const OUTPUT_PIPEWIRE: &str = r#"# The virtual mic other apps record from.
name = "mic_filter"
description = "Mic Filter"
"#;

const OUTPUT_CPAL: &str = r#"# The virtual audio device mic-filter plays into. Pick it as the mic in
# your apps. Get BlackHole with `brew install blackhole-2ch`.
name = "BlackHole 2ch"
"#;

const TEMPLATE: &str = r#"# mic-filter config. Edits apply live while mic-filter is running,
# except [output], which needs a restart.
# Signal chain: gain -> highpass -> denoise -> eq -> compressor -> deesser -> output gain -> limiter

[input]
# The mic to process. Switch it with `mic-filter use`, or paste a name from
# `mic-filter list`. "default" follows the system default mic.
device = {DEVICE}

[output]
{OUTPUT}

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
# The gate also stays shut for sounds with peaks quieter than this (dBFS,
# after noise removal). Raise it if voices across the room unmute you.
gate_level_db = -50.0

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

[deesser]
# Turns down harsh "s" and "sh" sounds. The band around freq_hz is cut,
# only while it's louder than threshold_db.
enabled = false
freq_hz = 6500.0
threshold_db = -34.0
max_reduction_db = 9.0

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
    fn template_output_matches_the_code_default() {
        let c = Config::parse(&template(None)).unwrap();
        assert_eq!(c.output, OutputConfig::default());
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

    fn changed_lines(a: &str, b: &str) -> Vec<String> {
        a.lines()
            .zip(b.lines())
            .filter(|(x, y)| x != y)
            .map(|(_, y)| y.to_string())
            .collect()
    }

    #[test]
    fn saving_unchanged_config_is_a_no_op() {
        let text = template(Some("alsa_input.a"));
        let config = Config::parse(&text).unwrap();
        assert_eq!(merged(&text, &config).unwrap(), text);
    }

    #[test]
    fn saving_changes_only_edited_lines() {
        let text = template(Some("alsa_input.a"));
        let mut config = Config::parse(&text).unwrap();
        config.compressor.ratio = 4.5;
        config.denoise.strength = 0.85;
        let after = merged(&text, &config).unwrap();
        assert_eq!(text.lines().count(), after.lines().count());
        assert_eq!(
            changed_lines(&text, &after),
            ["strength = 0.85", "ratio = 4.5"]
        );
    }

    #[test]
    fn saving_new_bands_keeps_commented_examples() {
        let text = template(Some("alsa_input.a"));
        let mut config = Config::parse(&text).unwrap();
        config.eq.bands[1].gain_db = 3.5;
        config.eq.bands.push(mic_filter_dsp::EqBand {
            kind: mic_filter_dsp::BandKind::HighShelf,
            freq_hz: 9000.0,
            gain_db: 2.0,
            q: 0.7,
        });
        let after = merged(&text, &config).unwrap();
        assert_eq!(Config::parse(&after).unwrap(), config);
        assert!(
            after.contains(
                "    { type = \"peaking\", freq_hz = 3000.0, gain_db = 3.5, q = 1.0 },\n"
            ),
            "{after}"
        );
        assert!(
            after.contains(
                "    { type = \"high_shelf\", freq_hz = 9000.0, gain_db = 2.0, q = 0.7 },\n"
            ),
            "{after}"
        );
        assert!(
            after.contains("# { type = \"notch\", freq_hz = 60.0, q = 10.0 },   # mains hum\n]"),
            "{after}"
        );
        assert!(after.contains("# mic-filter config."), "top comment lost");
    }

    #[test]
    fn saving_fills_in_a_sparse_file() {
        let mut config = Config::parse("[gain]\ninput_db = 6.0\n").unwrap();
        config.limiter.ceiling_db = -2.0;
        let after = merged("[gain]\ninput_db = 6.0\n", &config).unwrap();
        assert_eq!(Config::parse(&after).unwrap(), config);
    }

    #[test]
    fn save_writes_file_that_loads_back() {
        let dir = std::env::temp_dir().join(format!("mic-filter-save-{}", std::process::id()));
        let path = dir.join("config.toml");
        let mut config = Config::default();
        config.gain.input_db = 9.5;
        save(&path, &config).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
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
