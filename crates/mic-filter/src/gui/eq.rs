//! The EQ curve: the combined response of the high-pass filter and every EQ
//! band, with a draggable dot per band.

use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2,
};
use mic_filter_dsp::biquad::Coeffs;
use mic_filter_dsp::{BandKind, EqBand, EqSettings, HighpassSettings, MAX_EQ_BANDS, SAMPLE_RATE};

const FS: f32 = SAMPLE_RATE as f32;
const F_MIN: f32 = 20.0;
const F_MAX: f32 = 20_000.0;
/// The plot spans this many dB above and below zero. Band gains are held to
/// the same range so every dot stays on screen.
const DB_SPAN: f32 = 18.0;

const PALETTE: [Color32; 8] = [
    Color32::from_rgb(79, 195, 247),
    Color32::from_rgb(255, 183, 77),
    Color32::from_rgb(129, 199, 132),
    Color32::from_rgb(229, 115, 115),
    Color32::from_rgb(186, 104, 200),
    Color32::from_rgb(255, 213, 79),
    Color32::from_rgb(77, 182, 172),
    Color32::from_rgb(240, 98, 146),
];

const KINDS: [BandKind; 6] = [
    BandKind::Peaking,
    BandKind::LowShelf,
    BandKind::HighShelf,
    BandKind::LowPass,
    BandKind::HighPass,
    BandKind::Notch,
];

fn band_color(i: usize) -> Color32 {
    PALETTE[i % PALETTE.len()]
}

fn has_gain(kind: BandKind) -> bool {
    matches!(
        kind,
        BandKind::Peaking | BandKind::LowShelf | BandKind::HighShelf
    )
}

fn kind_label(kind: BandKind) -> &'static str {
    match kind {
        BandKind::Peaking => "Peak",
        BandKind::LowShelf => "Low shelf",
        BandKind::HighShelf => "High shelf",
        BandKind::LowPass => "Low-pass",
        BandKind::HighPass => "High-pass",
        BandKind::Notch => "Notch",
    }
}

pub fn format_freq(hz: f32) -> String {
    if hz >= 10_000.0 {
        format!("{:.1} kHz", hz / 1000.0)
    } else if hz >= 1000.0 {
        format!("{:.2} kHz", hz / 1000.0)
    } else {
        format!("{hz:.0} Hz")
    }
}

/// Rounds to a few significant figures so dragged values stay readable in
/// the config file.
fn round_sig(x: f32, digits: i32) -> f32 {
    if x == 0.0 {
        return 0.0;
    }
    let scale = 10f32.powi(digits - 1 - x.abs().log10().floor() as i32);
    (x * scale).round() / scale
}

fn round_to(x: f32, step: f32) -> f32 {
    // Dividing by the inverse keeps results like 2.3 exact in f32.
    let inverse = 1.0 / step;
    (x * inverse).round() / inverse
}

fn new_band(freq_hz: f32, gain_db: f32) -> EqBand {
    EqBand {
        kind: BandKind::Peaking,
        freq_hz,
        gain_db,
        q: 1.0,
    }
}

/// Maps between frequency/gain and screen positions inside the plot.
struct Axes {
    plot: Rect,
}

impl Axes {
    fn x(&self, hz: f32) -> f32 {
        let t = (hz / F_MIN).ln() / (F_MAX / F_MIN).ln();
        self.plot.left() + t * self.plot.width()
    }

    fn hz(&self, x: f32) -> f32 {
        let t = ((x - self.plot.left()) / self.plot.width()).clamp(0.0, 1.0);
        F_MIN * (F_MAX / F_MIN).powf(t)
    }

    fn y(&self, db: f32) -> f32 {
        self.plot.center().y - db / DB_SPAN * (self.plot.height() / 2.0)
    }

    fn db(&self, y: f32) -> f32 {
        ((self.plot.center().y - y) / (self.plot.height() / 2.0) * DB_SPAN).clamp(-DB_SPAN, DB_SPAN)
    }
}

#[derive(Default)]
pub struct EqView {
    selected: Option<usize>,
}

impl EqView {
    /// Draws the response curve and handles dragging. Returns true if any
    /// band changed.
    pub fn curve(
        &mut self,
        ui: &mut egui::Ui,
        eq: &mut EqSettings,
        highpass: &HighpassSettings,
    ) -> bool {
        let (response, painter) =
            ui.allocate_painter(Vec2::new(ui.available_width(), 270.0), Sense::click());
        let rect = response.rect;
        let axes = Axes {
            plot: Rect::from_min_max(
                rect.min + Vec2::new(40.0, 12.0),
                rect.max - Vec2::new(14.0, 24.0),
            ),
        };
        let plot = axes.plot;
        let visuals = ui.visuals().clone();
        let label_color = visuals.weak_text_color();
        let small = FontId::proportional(11.0);

        painter.rect_filled(rect, 6.0, visuals.extreme_bg_color);
        let grid = visuals.widgets.noninteractive.bg_stroke.color;
        for (hz, text) in [
            (20.0, "20"),
            (50.0, "50"),
            (100.0, "100"),
            (200.0, "200"),
            (500.0, "500"),
            (1000.0, "1k"),
            (2000.0, "2k"),
            (5000.0, "5k"),
            (10_000.0, "10k"),
            (20_000.0, "20k"),
        ] {
            let x = axes.x(hz);
            painter.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                Stroke::new(1.0, grid),
            );
            painter.text(
                Pos2::new(x, plot.bottom() + 4.0),
                Align2::CENTER_TOP,
                text,
                small.clone(),
                label_color,
            );
        }
        for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
            let y = axes.y(db);
            let width = if db == 0.0 { 1.5 } else { 1.0 };
            painter.line_segment(
                [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
                Stroke::new(width, grid),
            );
            let text = if db > 0.0 {
                format!("+{db:.0}")
            } else {
                format!("{db:.0}")
            };
            painter.text(
                Pos2::new(plot.left() - 6.0, y),
                Align2::RIGHT_CENTER,
                text,
                small.clone(),
                label_color,
            );
        }

        // Response curves. The high-pass is part of the chain, so it's
        // drawn into the total even though it's set elsewhere.
        let band_coeffs: Vec<Coeffs> = eq
            .bands
            .iter()
            .map(|b| Coeffs::new(b.kind, FS, b.freq_hz, b.q, b.gain_db))
            .collect();
        let highpass_coeffs = highpass.enabled.then(|| {
            Coeffs::new(
                BandKind::HighPass,
                FS,
                highpass.freq_hz,
                std::f32::consts::FRAC_1_SQRT_2,
                0.0,
            )
        });
        let total_db = |hz: f32| -> f32 {
            let mut db = highpass_coeffs.map_or(0.0, |c| c.magnitude_db(FS, hz));
            if eq.enabled {
                db += band_coeffs
                    .iter()
                    .map(|c| c.magnitude_db(FS, hz))
                    .sum::<f64>();
            }
            db as f32
        };
        let sample = |f: &dyn Fn(f32) -> f32| -> Vec<Pos2> {
            (0..=240)
                .map(|i| {
                    let hz = F_MIN * (F_MAX / F_MIN).powf(i as f32 / 240.0);
                    Pos2::new(
                        axes.x(hz),
                        axes.y(f(hz).clamp(-DB_SPAN * 1.5, DB_SPAN * 1.5)),
                    )
                })
                .collect()
        };
        let clipped = painter.with_clip_rect(plot);
        if let Some(i) = self
            .selected
            .filter(|&i| i < band_coeffs.len() && eq.enabled)
        {
            let c = band_coeffs[i];
            let own = sample(&|hz| c.magnitude_db(FS, hz) as f32);
            clipped.line(own, Stroke::new(1.5, band_color(i).gamma_multiply(0.6)));
        }
        let accent = if eq.enabled {
            visuals.selection.stroke.color
        } else {
            visuals.weak_text_color()
        };
        clipped.line(sample(&total_db), Stroke::new(2.5, accent));

        // Band handles, drawn and hit-tested on top of the canvas.
        let mut changed = false;
        let mut remove = None;
        for i in 0..eq.bands.len() {
            let band = &mut eq.bands[i];
            let gain_shown = if has_gain(band.kind) {
                band.gain_db
            } else {
                0.0
            };
            let center = Pos2::new(axes.x(band.freq_hz.clamp(F_MIN, F_MAX)), axes.y(gain_shown));
            let id = response.id.with(("band", i));
            let handle = ui.interact(
                Rect::from_center_size(center, Vec2::splat(22.0)),
                id,
                Sense::click_and_drag(),
            );

            if handle.clicked() || handle.drag_started() {
                self.selected = Some(i);
            }
            if handle.dragged() {
                if let Some(p) = handle.interact_pointer_pos() {
                    band.freq_hz = round_sig(axes.hz(p.x), 3);
                    if has_gain(band.kind) {
                        band.gain_db = round_to(axes.db(p.y), 0.1);
                    }
                    changed = true;
                }
            }
            if handle.hovered() {
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if scroll != 0.0 {
                    band.q = round_sig((band.q * (scroll * 0.004).exp()).clamp(0.1, 20.0), 3);
                    ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                    changed = true;
                }
            }
            handle.context_menu(|ui| {
                if ui.button("Remove band").clicked() {
                    remove = Some(i);
                    ui.close();
                }
            });
            let summary = band_summary(i, band);
            handle.on_hover_text(format!(
                "{summary}\nDrag to move. Scroll to change the width (Q). Right-click to remove."
            ));

            let color = if eq.enabled {
                band_color(i)
            } else {
                band_color(i).gamma_multiply(0.4)
            };
            let selected = self.selected == Some(i);
            let stroke = if selected {
                Stroke::new(2.5, visuals.strong_text_color())
            } else {
                Stroke::new(1.0, visuals.extreme_bg_color)
            };
            painter.circle(center, 9.0, color, stroke);
            painter.text(
                center,
                Align2::CENTER_CENTER,
                i + 1,
                FontId::proportional(11.0),
                Color32::BLACK,
            );
        }
        if let Some(i) = remove {
            self.remove(eq, i);
            changed = true;
        }

        if response.double_clicked() && eq.bands.len() < MAX_EQ_BANDS {
            if let Some(p) = response.interact_pointer_pos() {
                eq.bands.push(new_band(
                    round_sig(axes.hz(p.x), 3),
                    round_to(axes.db(p.y), 0.1),
                ));
                self.selected = Some(eq.bands.len() - 1);
                changed = true;
            }
        }

        if let Some(p) = response.hover_pos().filter(|p| plot.contains(*p)) {
            let readout = format!("{}   {:+.1} dB", format_freq(axes.hz(p.x)), axes.db(p.y));
            painter.text(
                plot.right_top() + Vec2::new(-6.0, 4.0),
                Align2::RIGHT_TOP,
                readout,
                small,
                label_color,
            );
        }
        painter.rect_stroke(
            rect,
            6.0,
            visuals.widgets.noninteractive.bg_stroke,
            StrokeKind::Inside,
        );
        changed
    }

    /// The row under the curve: one chip per band, then the selected band's
    /// controls. Returns true if any band changed.
    pub fn editor(&mut self, ui: &mut egui::Ui, eq: &mut EqSettings) -> bool {
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Bands:");
            for i in 0..eq.bands.len() {
                let text = RichText::new(format!(" {} ", i + 1))
                    .color(band_color(i))
                    .strong();
                if ui
                    .selectable_label(self.selected == Some(i), text)
                    .clicked()
                {
                    self.selected = Some(i);
                }
            }
            let can_add = eq.bands.len() < MAX_EQ_BANDS;
            if ui
                .add_enabled(can_add, egui::Button::new("+ Add band"))
                .clicked()
            {
                eq.bands.push(new_band(1000.0, 0.0));
                self.selected = Some(eq.bands.len() - 1);
                changed = true;
            }
        });

        let Some(i) = self.selected.filter(|&i| i < eq.bands.len()) else {
            ui.weak("Double-click the curve to add a band. Drag a dot to move it, scroll over it to change its width.");
            return changed;
        };
        let mut remove = false;
        let band = &mut eq.bands[i];
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().slider_width = 150.0;
            ui.label(
                RichText::new(format!("Band {}", i + 1))
                    .color(band_color(i))
                    .strong(),
            );
            egui::ComboBox::from_id_salt("band-kind")
                .selected_text(kind_label(band.kind))
                .show_ui(ui, |ui| {
                    for kind in KINDS {
                        changed |= ui
                            .selectable_value(&mut band.kind, kind, kind_label(kind))
                            .changed();
                    }
                });
            changed |= ui
                .add(
                    egui::Slider::new(&mut band.freq_hz, F_MIN..=F_MAX)
                        .logarithmic(true)
                        .custom_formatter(|v, _| format_freq(v as f32))
                        .text("Freq"),
                )
                .changed();
            changed |= ui
                .add_enabled(
                    has_gain(band.kind),
                    egui::Slider::new(&mut band.gain_db, -DB_SPAN..=DB_SPAN)
                        .step_by(0.1)
                        .suffix(" dB")
                        .text("Gain"),
                )
                .changed();
            changed |= ui
                .add(
                    egui::Slider::new(&mut band.q, 0.1..=20.0)
                        .logarithmic(true)
                        .max_decimals(2)
                        .text("Q"),
                )
                .changed();
            remove = ui.button("Remove").clicked();
        });
        if changed {
            band.freq_hz = round_sig(band.freq_hz, 3);
            band.q = round_sig(band.q, 3);
        }
        if remove {
            self.remove(eq, i);
            changed = true;
        }
        changed
    }

    fn remove(&mut self, eq: &mut EqSettings, i: usize) {
        eq.bands.remove(i);
        self.selected = match self.selected {
            Some(s) if s == i => None,
            Some(s) if s > i => Some(s - 1),
            other => other,
        };
    }
}

fn band_summary(i: usize, band: &EqBand) -> String {
    let gain = if has_gain(band.kind) {
        format!(", {:+.1} dB", band.gain_db)
    } else {
        String::new()
    };
    format!(
        "Band {}: {} at {}{gain}, Q {:.2}",
        i + 1,
        kind_label(band.kind),
        format_freq(band.freq_hz),
        band.q
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn axes_round_trip() {
        let axes = Axes {
            plot: Rect::from_min_max(Pos2::new(40.0, 10.0), Pos2::new(840.0, 250.0)),
        };
        for hz in [20.0, 100.0, 1234.0, 20_000.0] {
            assert!((axes.hz(axes.x(hz)) - hz).abs() / hz < 1e-4);
        }
        for db in [-18.0, -3.5, 0.0, 12.0] {
            assert!((axes.db(axes.y(db)) - db).abs() < 1e-4);
        }
    }

    #[test]
    fn rounding_keeps_values_tidy() {
        assert_eq!(round_sig(1234.5, 3), 1230.0);
        assert_eq!(round_sig(87.64, 3), 87.6);
        assert_eq!(round_to(2.349, 0.1), 2.3);
        assert_eq!(format_freq(250.0), "250 Hz");
        assert_eq!(format_freq(3000.0), "3.00 kHz");
        assert_eq!(format_freq(12_500.0), "12.5 kHz");
    }
}
