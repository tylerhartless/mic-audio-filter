//! `mic-filter gui`: a window for picking the mic and tuning every setting
//! while you listen. It edits the same config file the service watches, so
//! changes apply live and persist, and it shows live levels from the
//! service's meter socket.

mod eq;
mod gate;
mod listen;
mod meters;
mod simple;

use crate::config::{self, Config, DEFAULT_DEVICE};
use crate::{devices, ipc};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Minimum time between config writes while a control is being dragged.
const SAVE_INTERVAL: Duration = Duration::from_millis(80);
/// How often to look for new mics and outside edits to the config file.
const MIC_REFRESH: Duration = Duration::from_secs(5);
const FILE_CHECK: Duration = Duration::from_secs(1);

/// Window sizes for the two views. Switching views resizes the window.
const SIMPLE_SIZE: [f32; 2] = [600.0, 760.0];
const SIMPLE_MIN: [f32; 2] = [480.0, 560.0];
const ADVANCED_SIZE: [f32; 2] = [1100.0, 900.0];
const ADVANCED_MIN: [f32; 2] = [720.0, 600.0];

/// The launchd agent that runs the service on macOS. The install script
/// writes it.
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "io.github.tylerhartless.mic-filter";

const OK_COLOR: Color32 = Color32::from_rgb(80, 190, 110);
const BAD_COLOR: Color32 = Color32::from_rgb(230, 90, 80);

pub fn run(config_path: PathBuf) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Mic Filter")
            .with_app_id("mic-filter")
            .with_inner_size(SIMPLE_SIZE)
            .with_min_inner_size(SIMPLE_MIN),
        ..Default::default()
    };
    eframe::run_native(
        "Mic Filter",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc, config_path)))),
    )
    .map_err(|e| anyhow::anyhow!("could not open the window: {e}"))
}

/// What the meter socket has delivered since the UI last looked.
#[derive(Default)]
struct Feed {
    connected: bool,
    streaming: bool,
    /// The loudest values seen since the last frame.
    pending: Option<ipc::Snapshot>,
}

#[derive(Default)]
struct MicList {
    sources: devices::Sources,
    loading: bool,
    error: Option<String>,
    fetched: Option<Instant>,
}

struct Notice {
    text: String,
    error: bool,
    at: Instant,
}

impl Notice {
    fn info(text: impl Into<String>) -> Option<Notice> {
        Some(Notice {
            text: text.into(),
            error: false,
            at: Instant::now(),
        })
    }

    fn error(text: impl Into<String>) -> Option<Notice> {
        Some(Notice {
            text: text.into(),
            error: true,
            at: Instant::now(),
        })
    }
}

struct GuiApp {
    path: PathBuf,
    /// What the window shows and edits.
    config: Config,
    /// What was last written to (or read from) the file.
    saved: Config,
    file_modified: Option<SystemTime>,
    last_save: Instant,
    last_file_check: Instant,
    notice: Arc<Mutex<Option<Notice>>>,
    mics: Arc<Mutex<MicList>>,
    feed: Arc<Mutex<Feed>>,
    meters: meters::Meters,
    last_frame: Instant,
    eq: eq::EqView,
    listen: listen::Listen,
    want_preview: bool,
    want_monitor: bool,
    /// Showing every setting, with the simple controls hidden.
    advanced: bool,
    /// "Set from my voice" is listening.
    calibration: Option<gate::Calibration>,
}

impl GuiApp {
    fn new(cc: &eframe::CreationContext<'_>, path: PathBuf) -> Self {
        cc.egui_ctx.all_styles_mut(|style| {
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.slider_width = 240.0;
        });

        let (config, notice) = match Config::load(&path) {
            Ok(config) => (config, None),
            Err(_) if !path.exists() => (
                Config::default(),
                Notice::info(format!(
                    "No config yet. Your first change creates {}.",
                    path.display()
                )),
            ),
            Err(e) => (
                Config::default(),
                Notice::error(format!(
                    "The config file has a problem, so defaults are shown: {e:#}"
                )),
            ),
        };

        let feed = Arc::new(Mutex::new(Feed::default()));
        let ctx = cc.egui_ctx.clone();
        let feed_writer = feed.clone();
        ipc::spawn_client(ipc::socket_path(&config.output.name), move |snapshot| {
            let mut feed = feed_writer.lock().unwrap();
            match snapshot {
                Some(s) => {
                    feed.connected = true;
                    feed.streaming = s.streaming;
                    feed.pending = Some(match feed.pending {
                        Some(p) => ipc::Snapshot {
                            input_db: p.input_db.max(s.input_db),
                            output_db: p.output_db.max(s.output_db),
                            reduction_db: p.reduction_db.max(s.reduction_db),
                            vad: p.vad.max(s.vad),
                            streaming: s.streaming,
                            gate_db: p.gate_db.max(s.gate_db),
                            gate_open: p.gate_open || s.gate_open,
                        },
                        None => s,
                    });
                }
                None => *feed = Feed::default(),
            }
            ctx.request_repaint();
        });

        let now = Instant::now();
        GuiApp {
            file_modified: modified_time(&path),
            path,
            saved: config.clone(),
            config,
            last_save: now,
            last_file_check: now,
            notice: Arc::new(Mutex::new(notice)),
            mics: Arc::default(),
            feed,
            meters: meters::Meters::default(),
            last_frame: now,
            eq: eq::EqView::default(),
            listen: listen::Listen::default(),
            want_preview: true,
            want_monitor: false,
            advanced: false,
            calibration: None,
        }
    }

    fn set_notice(&self, notice: Option<Notice>) {
        *self.notice.lock().unwrap() = notice;
    }

    fn refresh_mics(&self, ctx: &egui::Context) {
        let mut list = self.mics.lock().unwrap();
        if list.loading {
            return;
        }
        list.loading = true;
        let mics = self.mics.clone();
        let own_output = self.config.output.name.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = devices::query();
            let mut list = mics.lock().unwrap();
            match result {
                Ok(sources) => {
                    list.sources = sources.without(&own_output);
                    list.error = None;
                }
                Err(e) => list.error = Some(format!("{e:#}")),
            }
            list.loading = false;
            list.fetched = Some(Instant::now());
            ctx.request_repaint();
        });
    }

    /// Runs a short command off the UI thread and reports how it went.
    fn run_command(
        &self,
        ctx: &egui::Context,
        program: &'static str,
        args: Vec<String>,
        success: &'static str,
    ) {
        let notice = self.notice.clone();
        let mics = self.mics.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = match Command::new(program).args(&args).output() {
                Ok(out) if out.status.success() => Notice::info(success),
                Ok(out) => Notice::error(format!(
                    "{program} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )),
                Err(e) => Notice::error(format!("could not run {program}: {e}")),
            };
            *notice.lock().unwrap() = result;
            // The command may have changed the default mic.
            mics.lock().unwrap().fetched = None;
            ctx.request_repaint();
        });
    }

    /// Writes the config when it changed, at most every SAVE_INTERVAL.
    fn save_if_changed(&mut self) {
        if self.config == self.saved || self.last_save.elapsed() < SAVE_INTERVAL {
            return;
        }
        self.last_save = Instant::now();
        // Recorded either way, so a failing value isn't retried every frame.
        self.saved = self.config.clone();
        match config::save(&self.path, &self.config) {
            Ok(()) => self.file_modified = modified_time(&self.path),
            Err(e) => self.set_notice(Notice::error(format!("Could not save: {e:#}"))),
        }
    }

    /// Picks up edits made to the config file outside this window.
    fn check_file(&mut self) {
        if self.last_file_check.elapsed() < FILE_CHECK {
            return;
        }
        self.last_file_check = Instant::now();
        let modified = modified_time(&self.path);
        if modified == self.file_modified || self.config != self.saved {
            return;
        }
        self.file_modified = modified;
        match Config::load(&self.path) {
            Ok(loaded) if loaded != self.config => {
                self.config = loaded.clone();
                self.saved = loaded;
                self.set_notice(Notice::info("Loaded changes made to the config file"));
            }
            Ok(_) => {}
            Err(e) => self.set_notice(Notice::error(format!(
                "The config file has a problem: {e:#}"
            ))),
        }
    }

    fn sync_children(&mut self, connected: bool) {
        let want_preview = (self.want_preview || !self.advanced) && connected;
        let problems = self.listen.sync(
            &self.config.output.name,
            want_preview,
            self.want_monitor && connected,
        );
        if let Some(e) = problems.preview {
            self.want_preview = false;
            self.set_notice(Notice::error(format!("Live preview stopped: {e}")));
        }
        if let Some(e) = problems.monitor {
            self.want_monitor = false;
            self.set_notice(Notice::error(format!("Hear myself stopped: {e}")));
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, connected: bool) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            if connected {
                ui.label(RichText::new("●").color(OK_COLOR));
                ui.label("Running");
            } else {
                ui.label(RichText::new("●").color(BAD_COLOR));
                ui.label("Not running");
                if ui.button("Start").clicked() {
                    let (program, args) = start_service_command();
                    self.run_command(&ctx, program, args, "Started the mic-filter service");
                }
            }
            ui.separator();

            ui.label("Mic");
            self.mic_picker(ui);
            if self.advanced {
                if ui.button("Refresh").on_hover_text("Look for newly connected mics").clicked() {
                    self.refresh_mics(&ctx);
                }
                ui.separator();
                if listen::NEEDS_PREVIEW {
                    ui.toggle_value(&mut self.want_preview, "Live preview").on_hover_text(
                        "Keeps the mic running while this window is open, so the meters move even when no app is recording.",
                    );
                }
                ui.toggle_value(&mut self.want_monitor, "Hear myself")
                    .on_hover_text("Plays Mic Filter into your output. Use headphones to avoid feedback.");
            }
            ui.separator();

            self.default_mic_status(ui, &ctx);
        });
    }

    /// Whether apps get the processed mic without picking it themselves.
    #[cfg(pipewire_backend)]
    fn default_mic_status(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let default = self.mics.lock().unwrap().sources.default.clone();
        if default.as_deref() == Some(self.config.output.name.as_str()) {
            ui.label(RichText::new("Default mic").color(OK_COLOR))
                .on_hover_text("Apps that use the system default mic get Mic Filter.");
        } else if default.is_some()
            && ui
                .button("Make default mic")
                .on_hover_text("Make Mic Filter the system default, so apps use it automatically.")
                .clicked()
        {
            self.run_command(
                ctx,
                "pactl",
                vec!["set-default-source".into(), self.config.output.name.clone()],
                "Mic Filter is now the default mic",
            );
        }
    }

    /// The default mic has to stay your real mic, which mic-filter may be
    /// following, so this only names the device apps should pick.
    #[cfg(cpal_backend)]
    fn default_mic_status(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        ui.label(format!("Apps use: {}", self.config.output.name))
            .on_hover_text("Pick this as the mic in Discord, Zoom, your browser and other apps.");
    }

    fn mic_picker(&mut self, ui: &mut egui::Ui) {
        let list = self.mics.lock().unwrap();
        let current = self.config.input.device.clone();
        let label = if current == DEFAULT_DEVICE {
            "Follow the system default".to_string()
        } else {
            list.sources
                .list
                .iter()
                .find(|m| m.name == current)
                .map(|m| m.description.clone())
                .unwrap_or_else(|| format!("{current} (not connected)"))
        };
        let mut chosen = current.clone();
        egui::ComboBox::from_id_salt("mic")
            .width(if self.advanced { 290.0 } else { 230.0 })
            .selected_text(label)
            .show_ui(ui, |ui| {
                if list.loading && list.sources.list.is_empty() {
                    ui.weak("Looking for mics...");
                }
                if let Some(e) = &list.error {
                    ui.colored_label(BAD_COLOR, e);
                }
                for mic in &list.sources.list {
                    let text = if mic.hardware {
                        mic.description.clone()
                    } else {
                        format!("{}  (virtual)", mic.description)
                    };
                    ui.selectable_value(&mut chosen, mic.name.clone(), text)
                        .on_hover_text(&mic.name);
                }
                ui.separator();
                ui.selectable_value(
                    &mut chosen,
                    DEFAULT_DEVICE.to_string(),
                    "Follow the system default",
                );
            });
        drop(list);
        if chosen != current {
            self.config.input.device = chosen;
        }
    }

    fn meter_panel(&mut self, ui: &mut egui::Ui, connected: bool, streaming: bool) {
        ui.add_space(6.0);
        ui.heading("Levels");
        ui.add_space(4.0);
        self.meters.show(ui);
        ui.add_space(10.0);
        if !connected {
            ui.colored_label(
                BAD_COLOR,
                "The mic-filter service isn't running, so nothing is processed. Press Start at the top.",
            );
        } else if !streaming {
            ui.weak(if listen::NEEDS_PREVIEW {
                "Idle: nothing is recording from Mic Filter. Turn on Live preview to see levels."
            } else {
                "Waiting for audio from your mic."
            });
        } else {
            ui.label("Talk normally and aim for the shaded zones.");
        }
        ui.add_space(8.0);
        ui.weak("In: your voice after the input gain.");
        ui.weak("Out: what apps hear.");
        ui.weak("Comp: how much the compressor turns loud parts down.");
        ui.weak("Voice: how sure noise removal is that you're talking.");
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        let c = &mut self.config;

        ui.horizontal(|ui| {
            ui.heading("Equalizer");
            ui.checkbox(&mut c.eq.enabled, "On");
            if c.highpass.enabled {
                ui.weak(format!(
                    "The curve includes the {:.0} Hz high-pass.",
                    c.highpass.freq_hz
                ));
            }
        });
        self.eq.curve(ui, &mut c.eq, &c.highpass);
        self.eq.editor(ui, &mut c.eq);
        ui.add_space(14.0);

        // Two columns of cards when there's room, one otherwise.
        if ui.available_width() >= 760.0 {
            ui.columns(2, |cols| {
                source_cards(&mut cols[0], c);
                dynamics_cards(&mut cols[1], c);
            });
        } else {
            source_cards(ui, c);
            dynamics_cards(ui, c);
        }
    }

    fn set_advanced(&mut self, ctx: &egui::Context, advanced: bool) {
        self.advanced = advanced;
        let (size, min) = if advanced {
            (ADVANCED_SIZE, ADVANCED_MIN)
        } else {
            (SIMPLE_SIZE, SIMPLE_MIN)
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::MinInnerSize(min.into()));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size.into()));
    }

    /// The default view: a level check, mic boost, and four plain controls.
    fn simple_view(&mut self, ui: &mut egui::Ui, connected: bool, streaming: bool) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            section_title(ui, "Your level");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let hear = egui::Button::new("Hear myself").selected(self.want_monitor);
                if ui
                    .add(hear)
                    .on_hover_text("Play your processed mic into your headphones while you adjust.")
                    .clicked()
                {
                    self.want_monitor = !self.want_monitor;
                }
            });
        });
        self.meters.show_simple(ui, connected, streaming);
        ui.add_space(18.0);

        section_title(ui, "Mic boost");
        ui.spacing_mut().slider_width = (ui.available_width() - 90.0).max(150.0);
        ui.add(
            egui::Slider::new(&mut self.config.gain.input_db, 0.0..=30.0)
                .step_by(0.5)
                .max_decimals(1)
                .suffix(" dB"),
        );
        ui.weak("Turn this up if you're too quiet.");
        ui.add_space(18.0);

        for (i, control) in simple::CONTROLS.iter().enumerate() {
            section_title(ui, control.title);
            let current = control.current(&self.config);
            ui.horizontal(|ui| {
                for level in simple::Level::ALL {
                    let button = egui::Button::new(level.label()).selected(current == Some(level))
                        .min_size(egui::vec2(90.0, 30.0));
                    if ui.add(button).clicked() {
                        (control.apply)(&mut self.config, level);
                    }
                }
                if current.is_none() {
                    ui.weak("Custom").on_hover_text(
                        "These settings were tuned by hand under Advanced settings. Pick a level to replace them.",
                    );
                }
            });
            ui.weak(control.help);
            // The first control is background noise, whose Strong level turns
            // on the voice gate.
            let d = &self.config.denoise;
            if i == 0 && d.enabled && d.vad_threshold > 0.0 {
                self.gate_section(ui, streaming);
            }
            ui.add_space(16.0);
        }
    }

    /// The voice gate's unmute level, shown while the gate is on.
    fn gate_section(&mut self, ui: &mut egui::Ui, streaming: bool) {
        ui.add_space(8.0);
        ui.label("Unmute level");
        if self
            .meters
            .show_gate(ui, &mut self.config.denoise.gate_level_db)
        {
            self.calibration = None;
        }
        ui.weak(format!(
            "Sounds below the line ({:.0} dB) never unmute you. Drag it above noise from across the room.",
            self.config.denoise.gate_level_db
        ));
        let label = match &self.calibration {
            Some(c) => format!("Listening, keep talking... {}", c.seconds_left()),
            None => "Set from my voice".into(),
        };
        let button = egui::Button::new(label);
        if ui
            .add_enabled(streaming && self.calibration.is_none(), button)
            .on_hover_text("Talk normally for a few seconds, and the line moves a safe distance below your voice.")
            .clicked()
        {
            self.calibration = Some(gate::Calibration::new());
        }
    }

    fn finish_calibration(&mut self) {
        let Some(calibration) = self.calibration.take() else {
            return;
        };
        let notice = match calibration.level() {
            Some(db) => {
                self.config.denoise.gate_level_db = db;
                format!("Unmute level set to {db:.0} dB.")
            }
            None => {
                "Didn't hear enough talking. Press Set from my voice and talk until it finishes."
                    .into()
            }
        };
        self.set_notice(Notice::info(notice));
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let mut notice = self.notice.lock().unwrap();
            if notice
                .as_ref()
                .is_some_and(|n| !n.error && n.at.elapsed() > Duration::from_secs(6))
            {
                *notice = None;
            }
            match notice.as_ref() {
                Some(n) if n.error => {
                    ui.colored_label(BAD_COLOR, &n.text);
                }
                Some(n) => {
                    ui.label(&n.text);
                }
                None => {
                    ui.weak("Changes save automatically and apply live.");
                }
            }
            drop(notice);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.advanced {
                    if ui.button("Advanced settings").clicked() {
                        self.set_advanced(&ctx, true);
                    }
                    return;
                }
                if ui.button("Simple view").clicked() {
                    self.set_advanced(&ctx, false);
                }
                if ui.button("Open config file").clicked() {
                    let opener = if cfg!(target_os = "macos") {
                        "open"
                    } else {
                        "xdg-open"
                    };
                    self.run_command(
                        &ctx,
                        opener,
                        vec![self.path.display().to_string()],
                        "Opened the config file",
                    );
                }
                ui.weak(self.path.display().to_string());
            });
        });
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.25);
        self.last_frame = now;

        let (connected, streaming, reading) = {
            let mut feed = self.feed.lock().unwrap();
            (feed.connected, feed.streaming, feed.pending.take())
        };
        let animating = self.meters.update(dt, reading, self.config.denoise.enabled);
        if let (Some(calibration), Some(r)) = (&mut self.calibration, &reading) {
            calibration.add(r);
        }
        if self
            .calibration
            .as_ref()
            .is_some_and(gate::Calibration::done)
        {
            self.finish_calibration();
        }
        self.sync_children(connected);
        self.check_file();
        let stale = {
            let list = self.mics.lock().unwrap();
            !list.loading && list.fetched.is_none_or(|t| t.elapsed() > MIC_REFRESH)
        };
        if stale {
            self.refresh_mics(&ctx);
        }

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui, connected);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.advanced {
            egui::Panel::right("meters")
                .resizable(false)
                .exact_size(250.0)
                .show(ui, |ui| self.meter_panel(ui, connected, streaming));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Keep the floating scroll bar off the cards' right edges.
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        right: 14,
                        ..Default::default()
                    })
                    .show(ui, |ui| {
                        if self.advanced {
                            self.settings(ui);
                        } else {
                            self.simple_view(ui, connected, streaming);
                        }
                    });
            });
        });

        self.save_if_changed();
        let next = if animating {
            Duration::from_millis(16)
        } else if self.calibration.is_some() {
            Duration::from_millis(100)
        } else if self.config != self.saved {
            SAVE_INTERVAL
        } else {
            Duration::from_millis(500)
        };
        ctx.request_repaint_after(next);
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        if self.config != self.saved {
            let _ = config::save(&self.path, &self.config);
        }
    }
}

/// Input gain, high-pass and noise removal.
fn source_cards(ui: &mut egui::Ui, c: &mut Config) {
    card(ui, "Input", None, INPUT_HINT, |ui, _| {
        slider_row(
            ui,
            "Input gain",
            true,
            egui::Slider::new(&mut c.gain.input_db, -20.0..=30.0)
                .step_by(0.5)
                .suffix(" dB"),
        );
        toggle_row(ui, "High-pass", &mut c.highpass.enabled);
        slider_row(
            ui,
            "Cutoff",
            c.highpass.enabled,
            egui::Slider::new(&mut c.highpass.freq_hz, 20.0..=300.0)
                .logarithmic(true)
                .step_by(1.0)
                .suffix(" Hz"),
        );
    });

    let d = &mut c.denoise;
    card(
        ui,
        "Noise removal",
        Some(&mut d.enabled),
        NOISE_HINT,
        |ui, on| {
            percent_row(ui, "Strength", on, &mut d.strength, 100.0, |v| {
                format!("{v:.0}%")
            });
            percent_row(ui, "Voice gate", on, &mut d.vad_threshold, 95.0, |v| {
                if v == 0.0 {
                    "Off".into()
                } else {
                    format!("{v:.0}%")
                }
            });
            let gate_on = on && d.vad_threshold > 0.0;
            slider_row(
                ui,
                "Gate hold",
                gate_on,
                egui::Slider::new(&mut d.vad_hold_ms, 0.0..=2000.0)
                    .step_by(10.0)
                    .suffix(" ms"),
            );
            slider_row(
                ui,
                "Gate level",
                gate_on,
                egui::Slider::new(&mut d.gate_level_db, -80.0..=0.0)
                    .step_by(1.0)
                    .suffix(" dB"),
            );
        },
    );
}

/// Compressor, output gain and limiter.
fn dynamics_cards(ui: &mut egui::Ui, c: &mut Config) {
    let k = &mut c.compressor;
    card(
        ui,
        "Compressor",
        Some(&mut k.enabled),
        COMPRESSOR_HINT,
        |ui, on| {
            slider_row(
                ui,
                "Threshold",
                on,
                egui::Slider::new(&mut k.threshold_db, -60.0..=0.0)
                    .step_by(0.5)
                    .suffix(" dB"),
            );
            slider_row(
                ui,
                "Ratio",
                on,
                egui::Slider::new(&mut k.ratio, 1.0..=20.0)
                    .logarithmic(true)
                    .step_by(0.1)
                    .custom_formatter(|v, _| format!("{v:.1}:1")),
            );
            slider_row(
                ui,
                "Knee",
                on,
                egui::Slider::new(&mut k.knee_db, 0.0..=24.0)
                    .step_by(0.5)
                    .suffix(" dB"),
            );
            slider_row(
                ui,
                "Attack",
                on,
                egui::Slider::new(&mut k.attack_ms, 0.1..=100.0)
                    .logarithmic(true)
                    .step_by(0.1)
                    .suffix(" ms"),
            );
            slider_row(
                ui,
                "Release",
                on,
                egui::Slider::new(&mut k.release_ms, 10.0..=1000.0)
                    .logarithmic(true)
                    .step_by(1.0)
                    .suffix(" ms"),
            );
            slider_row(
                ui,
                "Makeup gain",
                on,
                egui::Slider::new(&mut k.makeup_db, 0.0..=24.0)
                    .step_by(0.5)
                    .suffix(" dB"),
            );
        },
    );

    let s = &mut c.deesser;
    card(
        ui,
        "De-esser",
        Some(&mut s.enabled),
        DEESSER_HINT,
        |ui, on| {
            slider_row(
                ui,
                "Frequency",
                on,
                egui::Slider::new(&mut s.freq_hz, 3000.0..=10000.0)
                    .logarithmic(true)
                    .step_by(50.0)
                    .suffix(" Hz"),
            );
            slider_row(
                ui,
                "Threshold",
                on,
                egui::Slider::new(&mut s.threshold_db, -60.0..=0.0)
                    .step_by(0.5)
                    .suffix(" dB"),
            );
            slider_row(
                ui,
                "Max reduction",
                on,
                egui::Slider::new(&mut s.max_reduction_db, 0.0..=24.0)
                    .step_by(0.5)
                    .suffix(" dB"),
            );
        },
    );

    let (g, l) = (&mut c.gain, &mut c.limiter);
    card(ui, "Output", None, OUTPUT_HINT, |ui, _| {
        slider_row(
            ui,
            "Output gain",
            true,
            egui::Slider::new(&mut g.output_db, -24.0..=12.0)
                .step_by(0.5)
                .suffix(" dB"),
        );
        toggle_row(ui, "Limiter", &mut l.enabled);
        slider_row(
            ui,
            "Ceiling",
            l.enabled,
            egui::Slider::new(&mut l.ceiling_db, -12.0..=0.0)
                .step_by(0.1)
                .suffix(" dB"),
        );
        slider_row(
            ui,
            "Release",
            l.enabled,
            egui::Slider::new(&mut l.release_ms, 10.0..=500.0)
                .logarithmic(true)
                .step_by(1.0)
                .suffix(" ms"),
        );
    });
}

const INPUT_HINT: &str = "Raise the input gain until In sits in its shaded zone while you talk. The high-pass removes rumble and desk thumps.";
const NOISE_HINT: &str = "Lower the strength if your voice sounds watery. The voice gate mutes you between sentences; start around 50%. Raise the gate level if sounds across the room unmute you.";
const COMPRESSOR_HINT: &str = "Aim for 3 to 8 dB on the Comp meter while you talk. A lower threshold or a higher ratio compresses more.";
const DEESSER_HINT: &str = "Turns down harsh \"s\" sounds. A lower threshold catches more; move the frequency to where your S sounds are sharpest.";
const OUTPUT_HINT: &str =
    "Out should peak between -9 and -3 dB. The limiter keeps it from ever clipping.";

/// A titled box holding a two-column grid of controls, with a hint below.
/// With `toggle`, the title row gets an On switch, and `add_rows` learns
/// whether it's on so it can grey out its controls.
fn card(
    ui: &mut egui::Ui,
    title: &str,
    toggle: Option<&mut bool>,
    hint: &str,
    add_rows: impl FnOnce(&mut egui::Ui, bool),
) {
    let margin = 12.0;
    let frame = egui::Frame::group(ui.style()).inner_margin(margin);
    let inner_width = ui.available_width() - 2.0 * (margin + frame.stroke.width);
    frame.show(ui, |ui| {
        ui.set_width(inner_width);
        // Leave room for the label column and each slider's value box.
        ui.spacing_mut().slider_width = (inner_width - 200.0).clamp(90.0, 280.0);
        let on = ui
            .horizontal(|ui| {
                ui.label(RichText::new(title).strong().size(16.0));
                match toggle {
                    Some(on) => {
                        ui.checkbox(on, "On");
                        *on
                    }
                    None => true,
                }
            })
            .inner;
        ui.add_space(2.0);
        egui::Grid::new(title)
            .num_columns(2)
            .spacing([14.0, 8.0])
            .show(ui, |ui| add_rows(ui, on));
        ui.add_space(2.0);
        ui.weak(hint);
    });
    ui.add_space(10.0);
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).strong().size(16.0));
}

fn slider_row(ui: &mut egui::Ui, label: &str, enabled: bool, slider: egui::Slider<'_>) {
    ui.add_enabled(enabled, egui::Label::new(label));
    ui.add_enabled(enabled, slider);
    ui.end_row();
}

fn toggle_row(ui: &mut egui::Ui, label: &str, value: &mut bool) {
    ui.label(label);
    ui.checkbox(value, "On");
    ui.end_row();
}

/// A slider showing a 0..=1 setting as whole percents, which also keeps the
/// config file tidy (0.85, never 0.8500001).
fn percent_row(
    ui: &mut egui::Ui,
    label: &str,
    enabled: bool,
    value: &mut f32,
    max_percent: f32,
    format: fn(f64) -> String,
) {
    let mut percent = (*value * 100.0).round();
    let slider = egui::Slider::new(&mut percent, 0.0..=max_percent)
        .step_by(1.0)
        .custom_formatter(move |v, _| format(v));
    ui.add_enabled(enabled, egui::Label::new(label));
    if ui.add_enabled(enabled, slider).changed() {
        *value = percent / 100.0;
    }
    ui.end_row();
}

fn modified_time(path: &std::path::Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(target_os = "macos")]
fn start_service_command() -> (&'static str, Vec<String>) {
    // SAFETY: getuid has no preconditions and can't fail.
    let uid = unsafe { libc::getuid() };
    (
        "launchctl",
        vec!["kickstart".into(), format!("gui/{uid}/{LAUNCHD_LABEL}")],
    )
}

#[cfg(not(target_os = "macos"))]
fn start_service_command() -> (&'static str, Vec<String>) {
    (
        "systemctl",
        vec!["--user".into(), "start".into(), "mic-filter.service".into()],
    )
}
