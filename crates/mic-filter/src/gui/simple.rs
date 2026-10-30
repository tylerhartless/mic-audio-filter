//! The simple view's controls. Each one is a few named levels that set the
//! underlying settings, which the advanced view edits directly.

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Off,
    Light,
    Medium,
    Strong,
}

impl Level {
    pub const ALL: [Level; 4] = [Level::Off, Level::Light, Level::Medium, Level::Strong];

    pub fn label(self) -> &'static str {
        match self {
            Level::Off => "Off",
            Level::Light => "Light",
            Level::Medium => "Medium",
            Level::Strong => "Strong",
        }
    }
}

/// One simple control: its name, a one-line explanation, and how each level
/// maps onto the settings.
pub struct Control {
    pub title: &'static str,
    pub help: &'static str,
    pub apply: fn(&mut Config, Level),
}

impl Control {
    /// The level the settings match, or `None` when they were tuned by hand
    /// in the advanced view.
    pub fn current(&self, config: &Config) -> Option<Level> {
        Level::ALL.into_iter().find(|&level| {
            let mut probe = config.clone();
            (self.apply)(&mut probe, level);
            probe == *config
        })
    }
}

pub const CONTROLS: [Control; 4] = [
    Control {
        title: "Remove background noise",
        help: "Fans, keyboards, hum and room noise. Strong also mutes the mic between sentences.",
        apply: noise,
    },
    Control {
        title: "Even out my volume",
        help: "Brings quiet words up and loud ones down, so you stay at a steady level.",
        apply: leveling,
    },
    Control {
        title: "Soften harsh S sounds",
        help: "Turns down sharp \"s\" and \"sh\" sounds.",
        apply: sibilance,
    },
    Control {
        title: "Reduce pops and rumble",
        help: "Cuts breath pops, desk thumps and low hum.",
        apply: pops,
    },
];

// Each level sets only the fields that define it, so finer settings made
// under Advanced (gate hold time, de-esser frequency, knee) survive a
// change of level.

fn noise(c: &mut Config, level: Level) {
    let d = &mut c.denoise;
    d.enabled = level != Level::Off;
    let (strength, gate) = match level {
        Level::Off => return,
        Level::Light => (0.7, 0.0),
        Level::Medium => (1.0, 0.0),
        Level::Strong => (1.0, 0.6),
    };
    d.strength = strength;
    d.vad_threshold = gate;
}

fn leveling(c: &mut Config, level: Level) {
    let k = &mut c.compressor;
    k.enabled = level != Level::Off;
    // Peaks come out near -12 dBFS at every level; stronger levels lift
    // quiet speech more.
    let (threshold, ratio, attack, release, makeup) = match level {
        Level::Off => return,
        Level::Light => (-18.0, 2.0, 10.0, 200.0, 3.0),
        Level::Medium => (-24.0, 3.0, 5.0, 150.0, 6.0),
        Level::Strong => (-30.0, 6.0, 3.0, 100.0, 12.0),
    };
    k.threshold_db = threshold;
    k.ratio = ratio;
    k.attack_ms = attack;
    k.release_ms = release;
    k.makeup_db = makeup;
}

fn sibilance(c: &mut Config, level: Level) {
    let s = &mut c.deesser;
    s.enabled = level != Level::Off;
    let (threshold, max_reduction) = match level {
        Level::Off => return,
        Level::Light => (-28.0, 6.0),
        Level::Medium => (-34.0, 9.0),
        Level::Strong => (-40.0, 12.0),
    };
    s.threshold_db = threshold;
    s.max_reduction_db = max_reduction;
}

fn pops(c: &mut Config, level: Level) {
    let h = &mut c.highpass;
    h.enabled = level != Level::Off;
    h.freq_hz = match level {
        Level::Off => return,
        Level::Light => 60.0,
        Level::Medium => 80.0,
        Level::Strong => 120.0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(c: &Config) -> Vec<Option<Level>> {
        CONTROLS.iter().map(|control| control.current(c)).collect()
    }

    #[test]
    fn defaults_map_to_levels() {
        use Level::*;
        assert_eq!(
            levels(&Config::default()),
            [Some(Medium), Some(Medium), Some(Off), Some(Medium)]
        );
    }

    #[test]
    fn every_level_reads_back() {
        for control in &CONTROLS {
            for level in Level::ALL {
                let mut c = Config::default();
                (control.apply)(&mut c, level);
                assert_eq!(
                    control.current(&c),
                    Some(level),
                    "{} {:?}",
                    control.title,
                    level
                );
                assert!(
                    Config::parse(&toml::to_string(&c).unwrap()).is_ok(),
                    "{} {level:?} is invalid",
                    control.title
                );
            }
        }
    }

    #[test]
    fn controls_are_independent() {
        let mut c = Config::default();
        (CONTROLS[2].apply)(&mut c, Level::Strong);
        (CONTROLS[0].apply)(&mut c, Level::Light);
        use Level::*;
        assert_eq!(
            levels(&c),
            [Some(Light), Some(Medium), Some(Strong), Some(Medium)]
        );
    }

    #[test]
    fn hand_tuned_settings_show_as_custom() {
        let mut c = Config::default();
        c.compressor.ratio = 3.7;
        assert_eq!(CONTROLS[1].current(&c), None);
        c.compressor.enabled = false;
        assert_eq!(CONTROLS[1].current(&c), Some(Level::Off));
    }
}
