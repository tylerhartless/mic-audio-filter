//! Level meters fed by the service's meter socket.

use crate::ipc::Snapshot;
use eframe::egui::{self, Align2, Color32, FontId, Rect, Sense, Stroke, StrokeKind, Vec2};
use std::time::{Duration, Instant};

/// How fast a displayed peak falls back once the level drops.
const FALL_DB_PER_SEC: f32 = 30.0;
const FLOOR_DB: f32 = -60.0;
/// Where speech peaks should land in what apps hear. The simple view calls
/// this range good.
const SPEECH_TARGET_DB: (f32, f32) = (-18.0, -3.0);
/// How long the simple view keeps judging your level after you stop talking.
const SPEECH_MEMORY: Duration = Duration::from_secs(3);
/// The highest unmute level the gate bar's line can be dragged to.
const GATE_MAX_DB: f32 = -5.0;

const QUIET: Color32 = Color32::from_rgb(90, 140, 205);
const GOOD: Color32 = Color32::from_rgb(80, 190, 110);
const HOT: Color32 = Color32::from_rgb(230, 190, 70);
const CLIP: Color32 = Color32::from_rgb(230, 90, 80);
const COMP: Color32 = Color32::from_rgb(235, 150, 60);
const VOICE: Color32 = Color32::from_rgb(160, 125, 235);

pub struct Meters {
    input_db: f32,
    output_db: f32,
    reduction_db: f32,
    vad: f32,
    /// Recent output peak while you were talking, for the simple view.
    speech_db: f32,
    last_speech: Option<Instant>,
    /// The level the voice gate measures, after noise removal.
    gate_db: f32,
    /// Seconds left to show the gate as open.
    gate_open_left: f32,
}

impl Default for Meters {
    fn default() -> Self {
        Self {
            input_db: FLOOR_DB,
            output_db: FLOOR_DB,
            reduction_db: 0.0,
            vad: 0.0,
            speech_db: FLOOR_DB,
            last_speech: None,
            gate_db: FLOOR_DB,
            gate_open_left: 0.0,
        }
    }
}

impl Meters {
    /// Lets the displayed peaks fall for `dt` seconds, then folds in any new
    /// reading. `voice_detection` says whether noise removal is on and so
    /// reporting when you talk; without it, any clear signal counts as
    /// talking. Returns true while something is still moving.
    pub fn update(&mut self, dt: f32, reading: Option<Snapshot>, voice_detection: bool) -> bool {
        let fall = FALL_DB_PER_SEC * dt;
        self.input_db = (self.input_db - fall).max(FLOOR_DB);
        self.output_db = (self.output_db - fall).max(FLOOR_DB);
        self.reduction_db = (self.reduction_db - fall).max(0.0);
        self.vad = (self.vad - 2.0 * dt).max(0.0);
        self.gate_db = (self.gate_db - fall).max(FLOOR_DB);
        self.gate_open_left = (self.gate_open_left - dt).max(0.0);
        // Falls slowly, so the verdict doesn't flicker between words.
        self.speech_db = (self.speech_db - 4.0 * dt).max(FLOOR_DB);
        if let Some(r) = reading {
            self.input_db = self.input_db.max(r.input_db);
            self.output_db = self.output_db.max(r.output_db);
            self.reduction_db = self.reduction_db.max(r.reduction_db);
            self.vad = self.vad.max(r.vad);
            self.gate_db = self.gate_db.max(r.gate_db);
            if r.gate_open {
                self.gate_open_left = 0.1;
            }
            let talking = if voice_detection {
                r.vad > 0.6
            } else {
                r.output_db > -35.0
            };
            if talking {
                self.speech_db = self.speech_db.max(r.output_db);
                self.last_speech = Some(Instant::now());
            }
        }
        self.input_db > FLOOR_DB
            || self.output_db > FLOOR_DB
            || self.reduction_db > 0.0
            || self.vad > 0.0
            || self.gate_db > FLOOR_DB
            || self.gate_open_left > 0.0
    }

    /// One bar for what apps hear, and a plain-words verdict on your level.
    pub fn show_simple(&self, ui: &mut egui::Ui, connected: bool, streaming: bool) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
        let painter = ui.painter_at(rect);
        let visuals = ui.visuals();
        let x_at = |db: f32| rect.left() + db_fraction(db) * rect.width();
        painter.rect_filled(rect, 5.0, visuals.extreme_bg_color);
        let (lo, hi) = SPEECH_TARGET_DB;
        painter.rect_filled(
            Rect::from_x_y_ranges(x_at(lo)..=x_at(hi), rect.y_range()),
            0.0,
            GOOD.gamma_multiply(0.18),
        );
        let level = Rect::from_x_y_ranges(rect.left()..=x_at(self.output_db), rect.y_range());
        if self.output_db > FLOOR_DB {
            painter.rect_filled(
                level.shrink(4.0),
                3.0,
                level_color(self.output_db, SPEECH_TARGET_DB),
            );
        }
        painter.rect_stroke(
            rect,
            5.0,
            visuals.widgets.noninteractive.bg_stroke,
            StrokeKind::Inside,
        );

        let talked_recently = self
            .last_speech
            .is_some_and(|t| t.elapsed() < SPEECH_MEMORY);
        let (color, text) = if !connected {
            (CLIP, "mic-filter isn't running. Press Start at the top.")
        } else if !streaming {
            (
                visuals.weak_text_color(),
                "Waiting for audio from your mic...",
            )
        } else if !talked_recently {
            (
                visuals.weak_text_color(),
                "Say a few words to check your level.",
            )
        } else if self.speech_db < lo {
            (QUIET, "A bit quiet. Turn up Mic boost.")
        } else if self.speech_db > hi {
            (HOT, "A bit loud. Turn down Mic boost.")
        } else {
            (GOOD, "Sounds good.")
        };
        ui.label(egui::RichText::new(text).color(color));
    }

    /// The level the voice gate hears, with its unmute level as a line you
    /// drag. The bar turns green while the gate lets sound through. Returns
    /// true when the line moved.
    pub fn show_gate(&self, ui: &mut egui::Ui, level_db: &mut f32) -> bool {
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 28.0),
            Sense::click_and_drag(),
        );
        let response = response
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
            .on_hover_text("Drag to set how loud a sound must be to unmute you.");
        let mut changed = false;
        if let Some(pos) = response.interact_pointer_pos() {
            let fraction = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            let db = (FLOOR_DB * (1.0 - fraction)).round().min(GATE_MAX_DB);
            changed = db != *level_db;
            *level_db = db;
        }

        let painter = ui.painter_at(rect);
        let visuals = ui.visuals();
        let x_at = |db: f32| rect.left() + db_fraction(db) * rect.width();
        let line_x = x_at(*level_db);
        painter.rect_filled(rect, 5.0, visuals.extreme_bg_color);
        painter.rect_filled(
            Rect::from_x_y_ranges(rect.left()..=line_x, rect.y_range()),
            0.0,
            CLIP.gamma_multiply(0.12),
        );
        if self.gate_db > FLOOR_DB {
            let level = Rect::from_x_y_ranges(rect.left()..=x_at(self.gate_db), rect.y_range());
            let color = if self.gate_open_left > 0.0 {
                GOOD
            } else {
                visuals.weak_text_color()
            };
            painter.rect_filled(level.shrink(4.0), 3.0, color);
        }
        painter.rect_stroke(
            rect,
            5.0,
            visuals.widgets.noninteractive.bg_stroke,
            StrokeKind::Inside,
        );
        painter.vline(
            line_x,
            rect.y_range(),
            Stroke::new(3.0, visuals.strong_text_color()),
        );
        changed
    }

    pub fn show(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            level_bar(ui, "In", self.input_db, (-18.0, -10.0));
            level_bar(ui, "Out", self.output_db, (-9.0, -3.0));
            bar(
                ui,
                "Comp",
                self.reduction_db / 24.0,
                Some((3.0 / 24.0, 8.0 / 24.0)),
                COMP,
                if self.reduction_db < 0.05 {
                    "0.0".into()
                } else {
                    format!("-{:.1}", self.reduction_db)
                },
            );
            bar(
                ui,
                "Voice",
                self.vad,
                None,
                VOICE,
                format!("{:.0}%", self.vad * 100.0),
            );
        });
    }
}

fn db_fraction(db: f32) -> f32 {
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// A dBFS meter with its target zone shaded.
fn level_color(db: f32, zone: (f32, f32)) -> Color32 {
    if db < zone.0 {
        QUIET
    } else if db <= zone.1 {
        GOOD
    } else if db <= -3.0 {
        HOT
    } else {
        CLIP
    }
}

fn level_bar(ui: &mut egui::Ui, label: &str, db: f32, zone: (f32, f32)) {
    let color = level_color(db, zone);
    let text = if db <= FLOOR_DB {
        "-inf".to_string()
    } else {
        format!("{db:.1}")
    };
    bar(
        ui,
        label,
        db_fraction(db),
        Some((db_fraction(zone.0), db_fraction(zone.1))),
        color,
        text,
    );
}

fn bar(
    ui: &mut egui::Ui,
    label: &str,
    fill: f32,
    zone: Option<(f32, f32)>,
    color: Color32,
    value: String,
) {
    let size = Vec2::new(46.0, 250.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let text_color = visuals.text_color();
    let font = FontId::proportional(13.0);

    let track = Rect::from_min_max(
        rect.min + Vec2::new(8.0, 20.0),
        rect.max - Vec2::new(8.0, 22.0),
    );
    painter.rect_filled(track, 4.0, visuals.extreme_bg_color);
    let y_at = |f: f32| track.bottom() - f.clamp(0.0, 1.0) * track.height();
    if let Some((lo, hi)) = zone {
        let zone_rect = Rect::from_x_y_ranges(track.x_range(), y_at(hi)..=y_at(lo));
        painter.rect_filled(zone_rect, 0.0, GOOD.gamma_multiply(0.18));
    }
    let filled = Rect::from_x_y_ranges(track.x_range(), y_at(fill)..=track.bottom());
    painter.rect_filled(filled.shrink(3.0), 3.0, color);
    painter.rect_stroke(
        track,
        4.0,
        visuals.widgets.noninteractive.bg_stroke,
        StrokeKind::Inside,
    );

    painter.text(
        rect.center_top(),
        Align2::CENTER_TOP,
        label,
        font.clone(),
        text_color,
    );
    painter.text(
        rect.center_bottom(),
        Align2::CENTER_BOTTOM,
        value,
        font,
        text_color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peaks_jump_up_and_fall_slowly() {
        let mut m = Meters::default();
        let loud = Snapshot {
            input_db: -12.0,
            ..Snapshot::default()
        };
        m.update(0.0, Some(loud), true);
        assert_eq!(m.input_db, -12.0);
        m.update(0.1, None, true);
        assert!((m.input_db - (-15.0)).abs() < 1e-4);
        assert!(
            !m.update(10.0, None, true),
            "everything should settle at the floor"
        );
    }
}
