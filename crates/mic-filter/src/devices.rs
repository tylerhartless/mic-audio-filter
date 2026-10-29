//! One-shot query of PipeWire for the available mics and the default one.

use crate::config::DEFAULT_DEVICE;
use anyhow::{bail, Result};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::types::ObjectType;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub description: String,
    /// Backed by a real device (ALSA, Bluetooth...). Virtual nodes are false.
    pub hardware: bool,
}

impl Source {
    /// How likely this is the mic you talk into. Registry globals don't carry
    /// the device form factor, so this goes by name: headsets are usually USB
    /// or Bluetooth, and webcam mics are usually the worst choice.
    fn voice_score(&self) -> i32 {
        let text = format!("{} {}", self.name, self.description).to_lowercase();
        let mut score = 0;
        if text.contains(".usb-") || text.starts_with("bluez_input") {
            score += 2;
        }
        if text.contains("camera") || text.contains("webcam") {
            score -= 3;
        }
        score
    }
}

#[derive(Debug, Default)]
pub struct Sources {
    pub list: Vec<Source>,
    /// node.name of the system default source.
    pub default: Option<String>,
}

impl Sources {
    /// The mic `init` should pick: the default if it's real hardware,
    /// otherwise the hardware mic that looks most like a headset.
    pub fn best_hardware(&self) -> Option<&Source> {
        let default = self
            .list
            .iter()
            .find(|s| Some(&s.name) == self.default.as_ref() && s.hardware);
        default.or_else(|| {
            self.list
                .iter()
                .filter(|s| s.hardware)
                .max_by_key(|s| s.voice_score())
        })
    }

    /// Drops the virtual mic that mic-filter publishes, which can't be its
    /// own input.
    pub fn without(mut self, own_output: &str) -> Self {
        self.list.retain(|s| s.name != own_output);
        self
    }

    /// Turns what the user typed into an `[input] device` value. Accepts a
    /// number from the list, "default" (or "d"), an exact node name, or a
    /// piece of one mic's name or description, such as "aula" or "usb".
    pub fn resolve(&self, choice: &str) -> Result<String> {
        let choice = choice.trim();
        if choice.is_empty() {
            bail!("no mic chosen");
        }
        if choice.eq_ignore_ascii_case(DEFAULT_DEVICE) || choice.eq_ignore_ascii_case("d") {
            return Ok(DEFAULT_DEVICE.into());
        }
        if let Ok(n) = choice.parse::<usize>() {
            return match n.checked_sub(1).and_then(|i| self.list.get(i)) {
                Some(s) => Ok(s.name.clone()),
                None => bail!("there is no mic {n}; pick 1 to {}", self.list.len()),
            };
        }
        if let Some(s) = self.list.iter().find(|s| s.name == choice) {
            return Ok(s.name.clone());
        }
        let needle = choice.to_lowercase();
        let matches: Vec<&Source> = self
            .list
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&needle)
                    || s.description.to_lowercase().contains(&needle)
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok(one.name.clone()),
            // A full node name for a mic that isn't plugged in right now.
            [] if choice.contains('.') && !choice.contains(char::is_whitespace) => {
                Ok(choice.to_string())
            }
            [] => bail!("no mic matches \"{choice}\"; run `mic-filter list` to see them"),
            many => bail!(
                "\"{choice}\" matches {} mics ({}); be more specific or use the number",
                many.len(),
                many.iter()
                    .map(|s| s.description.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

pub fn query() -> Result<Sources> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let sources = Rc::new(RefCell::new(Sources::default()));
    // Keeps the bound metadata proxy and its listener alive until we're done.
    let metadata: Rc<RefCell<Vec<(Metadata, MetadataListener)>>> = Rc::default();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let sources = sources.clone();
            let metadata = metadata.clone();
            let registry = registry.clone();
            move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    ObjectType::Node if props.get("media.class") == Some("Audio/Source") => {
                        let name = props.get("node.name").unwrap_or_default().to_string();
                        sources.borrow_mut().list.push(Source {
                            description: props.get("node.description").unwrap_or(&name).to_string(),
                            hardware: props.get("device.id").is_some(),
                            name,
                        });
                    }
                    ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                        let Ok(md) = registry.bind::<Metadata, _>(global) else {
                            return;
                        };
                        let sources = sources.clone();
                        let listener = md
                            .add_listener_local()
                            .property(move |_, key, _, value| {
                                if key == Some("default.audio.source") {
                                    sources.borrow_mut().default = value.and_then(json_name);
                                }
                                0
                            })
                            .register();
                        metadata.borrow_mut().push((md, listener));
                    }
                    _ => {}
                }
            }
        })
        .register();

    // Two round trips: the first delivers the globals, the second delivers
    // the properties of the metadata object we bound during the first.
    let rounds_left = Rc::new(Cell::new(2));
    let pending = Rc::new(Cell::new(core.sync(0)?));
    let _core_listener = core
        .add_listener_local()
        .done({
            let core = core.clone();
            let mainloop = mainloop.clone();
            let pending = pending.clone();
            let rounds_left = rounds_left.clone();
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || seq != pending.get() {
                    return;
                }
                rounds_left.set(rounds_left.get() - 1);
                match core.sync(0) {
                    Ok(next) if rounds_left.get() > 0 => pending.set(next),
                    _ => mainloop.quit(),
                }
            }
        })
        .register();
    mainloop.run();

    drop(metadata);
    let mut sources = sources.take();
    sources.list.sort_by(|a, b| {
        b.hardware
            .cmp(&a.hardware)
            .then(b.voice_score().cmp(&a.voice_score()))
    });
    Ok(sources)
}

/// Pulls "name" out of a metadata value like `{ "name": "alsa_input.foo" }`.
fn json_name(value: &str) -> Option<String> {
    let after_key = &value[value.find("\"name\"")? + 6..];
    let start = after_key.find('"')? + 1;
    let len = after_key[start..].find('"')?;
    Some(after_key[start..start + len].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(name: &str, description: &str) -> Source {
        Source {
            name: name.into(),
            description: description.into(),
            hardware: true,
        }
    }

    #[test]
    fn prefers_headset_over_builtin_and_webcam() {
        let sources = Sources {
            list: vec![
                hw(
                    "alsa_input.pci-0000_00_1f.3.analog-stereo",
                    "Built-in Audio Analog Stereo",
                ),
                hw(
                    "alsa_input.usb-CVTE_Camera_000000-02.analog-stereo",
                    "Camera Analog Stereo",
                ),
                hw(
                    "alsa_input.usb-C-Media_AULA-G7Pro-01.mono-fallback",
                    "AULA-G7Pro Mono",
                ),
                Source {
                    name: "effect_output.micboost".into(),
                    description: "Mic Boosted".into(),
                    hardware: false,
                },
            ],
            default: Some("effect_output.micboost".into()),
        };
        assert_eq!(
            sources.best_hardware().unwrap().description,
            "AULA-G7Pro Mono"
        );
    }

    #[test]
    fn hardware_default_wins() {
        let sources = Sources {
            list: vec![
                hw("alsa_input.pci-x", "Built-in"),
                hw("alsa_input.usb-headset", "Headset"),
            ],
            default: Some("alsa_input.pci-x".into()),
        };
        assert_eq!(sources.best_hardware().unwrap().description, "Built-in");
    }

    fn sample() -> Sources {
        Sources {
            list: vec![
                hw(
                    "alsa_input.usb-C-Media_AULA-G7Pro-01.mono-fallback",
                    "AULA-G7Pro Mono",
                ),
                hw(
                    "alsa_input.pci-0000_00_1f.3.analog-stereo",
                    "Built-in Audio Analog Stereo",
                ),
                hw(
                    "alsa_input.usb-CVTE_Camera-02.analog-stereo",
                    "Camera Analog Stereo",
                ),
                Source {
                    name: "mic_filter".into(),
                    description: "Mic Filter".into(),
                    hardware: false,
                },
            ],
            default: None,
        }
        .without("mic_filter")
    }

    #[test]
    fn own_output_is_not_selectable() {
        assert!(sample().list.iter().all(|s| s.name != "mic_filter"));
        assert!(sample().resolve("Mic Filter").is_err());
    }

    #[test]
    fn resolves_numbers_names_and_fragments() {
        let s = sample();
        assert_eq!(
            s.resolve("2").unwrap(),
            "alsa_input.pci-0000_00_1f.3.analog-stereo"
        );
        assert_eq!(
            s.resolve(" aula \n").unwrap(),
            "alsa_input.usb-C-Media_AULA-G7Pro-01.mono-fallback"
        );
        assert_eq!(
            s.resolve("Built-in").unwrap(),
            "alsa_input.pci-0000_00_1f.3.analog-stereo"
        );
        assert_eq!(
            s.resolve("alsa_input.usb-CVTE_Camera-02.analog-stereo")
                .unwrap(),
            "alsa_input.usb-CVTE_Camera-02.analog-stereo"
        );
        assert_eq!(s.resolve("default").unwrap(), DEFAULT_DEVICE);
        assert_eq!(s.resolve("D").unwrap(), DEFAULT_DEVICE);
    }

    #[test]
    fn rejects_bad_choices() {
        let s = sample();
        assert!(s.resolve("0").is_err());
        assert!(s.resolve("4").is_err());
        assert!(s.resolve("").is_err());
        assert!(s.resolve("nonexistent").is_err());
        let err = s.resolve("usb").unwrap_err().to_string();
        assert!(err.contains("matches 2 mics"), "{err}");
    }

    #[test]
    fn accepts_full_name_of_unplugged_mic() {
        assert_eq!(
            sample().resolve("bluez_input.00_11_22.0").unwrap(),
            "bluez_input.00_11_22.0"
        );
    }

    #[test]
    fn parses_default_source_metadata() {
        assert_eq!(
            json_name(r#"{ "name": "alsa_input.usb-foo.mono" }"#).as_deref(),
            Some("alsa_input.usb-foo.mono")
        );
        assert_eq!(json_name(r#"{"name":"x"}"#).as_deref(), Some("x"));
        assert_eq!(json_name("{}"), None);
    }
}
