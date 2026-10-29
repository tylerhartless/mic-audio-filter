//! The Linux backend: a PipeWire capture stream on the real mic, feeding a
//! second stream published as an `Audio/Source` node, the virtual mic.
//!
//! This follows the same pattern as PipeWire's own filter-chain module:
//! - Both streams share a `node.group` and `node.link-group`. The group keeps
//!   them on one clock. The link-group stops WirePlumber from linking our
//!   capture to our own output, which would loop.
//! - The capture side is passive, so the real mic only runs while some app is
//!   recording from the virtual mic.
//! - The output stream uses `TRIGGER`: each capture cycle triggers it, and its
//!   process callback pulls the captured buffer, runs the chain, and emits the
//!   result in the same graph cycle, so the streams add no buffering.
//! - Switching mics reconnects only the capture stream. The virtual mic stays
//!   up, so apps keep it selected and hear a short gap.

use crate::config::{self, Config, InputConfig};
use anyhow::{Context, Result};
use mic_filter_dsp::{Chain, ChainParams, MeterReading, Meters, SAMPLE_RATE};
use pipewire as pw;
use pw::properties::PropertiesBox;
use pw::spa;
use pw::stream::{Stream, StreamBox, StreamFlags, StreamRc, StreamState};
use std::cell::{Cell, RefCell};
use std::ffi::CString;
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Set while `--meter` has a live line drawn on stderr.
static METER_LINE: AtomicBool = AtomicBool::new(false);

/// eprintln! that first clears the meter line, so messages don't get glued
/// onto it. The meter redraws below on its next tick.
macro_rules! status {
    ($($arg:tt)*) => {{
        if METER_LINE.swap(false, Ordering::Relaxed) {
            eprint!("\r\x1b[2K");
        }
        eprintln!($($arg)*);
    }};
}

pub struct Options {
    pub config_path: PathBuf,
    pub config: Config,
    pub meter: bool,
}

/// Runs until SIGINT/SIGTERM.
pub fn run(opts: Options) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .context("connecting to PipeWire; is it running?")?;

    // If PipeWire goes away, exit with an error so systemd restarts us
    // against the new daemon. Dead streams would otherwise sit idle.
    let connection_lost = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .error({
            let mainloop = mainloop.clone();
            let connection_lost = connection_lost.clone();
            move |id, _, _, message| {
                if id == pw::core::PW_ID_CORE {
                    status!("PipeWire error: {message}");
                    connection_lost.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();

    let cfg = &opts.config;
    let group = format!("mic-filter-{}", std::process::id());

    let mut capture_props = PropertiesBox::new();
    capture_props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    capture_props.insert(*pw::keys::MEDIA_CATEGORY, "Capture");
    capture_props.insert(*pw::keys::APP_NAME, "mic-filter");
    capture_props.insert(*pw::keys::NODE_NAME, format!("{}.capture", cfg.output.name));
    capture_props.insert(
        *pw::keys::NODE_DESCRIPTION,
        format!("{} input", cfg.output.description),
    );
    capture_props.insert(*pw::keys::NODE_PASSIVE, "true");
    capture_props.insert(*pw::keys::NODE_GROUP, group.as_str());
    capture_props.insert(*pw::keys::NODE_LINK_GROUP, group.as_str());

    let mut output_props = PropertiesBox::new();
    output_props.insert(*pw::keys::MEDIA_CLASS, "Audio/Source");
    output_props.insert(*pw::keys::APP_NAME, "mic-filter");
    output_props.insert(*pw::keys::NODE_NAME, cfg.output.name.as_str());
    output_props.insert(*pw::keys::NODE_DESCRIPTION, cfg.output.description.as_str());
    output_props.insert(*pw::keys::NODE_GROUP, group.as_str());
    output_props.insert(*pw::keys::NODE_LINK_GROUP, group.as_str());

    let capture = StreamRc::new(core.clone(), "mic-filter capture", capture_props)?;
    let output = StreamBox::new(&core, "mic-filter output", output_props)?;

    let meters = Arc::new(Meters::default());
    let (params_tx, params_rx) = rtrb::RingBuffer::<ChainParams>::new(16);
    let chain = Chain::new(ChainParams::new(&cfg.chain()), meters.clone());

    // The two process callbacks reach each other's stream through raw
    // pointers, exactly as filter-chain does in C. Both streams outlive both
    // listeners: the listeners are dropped first at the end of this function.
    let capture_ptr: *const Stream = &*capture;
    let output_ptr: *const Stream = &*output;

    let device_label = Rc::new(RefCell::new(input_label(&cfg.input)));
    let _capture_listener = capture
        .add_local_listener_with_user_data(())
        .process(move |capture, _| {
            // SAFETY: see above; the output stream outlives this listener.
            let output = unsafe { &*output_ptr };
            if output.trigger_process().is_err() {
                // The output side isn't ready: recycle the input buffers.
                while capture.dequeue_buffer().is_some() {}
            }
        })
        .state_changed({
            let device_label = device_label.clone();
            move |_, _, _, new| match new {
                StreamState::Streaming => status!("capturing from {}", device_label.borrow()),
                StreamState::Error(e) => status!("capture error: {e}"),
                _ => {}
            }
        })
        .register()?;

    let streaming = Rc::new(Cell::new(false));
    let description = cfg.output.description.clone();
    let _output_listener = output
        .add_local_listener_with_user_data(Processor {
            chain,
            params_rx,
            capture: capture_ptr,
            last_len: 0,
        })
        .process(|output, p| p.process(output))
        .state_changed({
            let streaming = streaming.clone();
            move |_, _, old, new| {
                streaming.set(matches!(new, StreamState::Streaming));
                match new {
                    StreamState::Paused if matches!(old, StreamState::Connecting) => {
                        status!("virtual mic \"{description}\" is ready; select it in any app")
                    }
                    StreamState::Error(e) => status!("virtual mic error: {e}"),
                    _ => {}
                }
            }
        })
        .register()?;

    let format = format_pod()?;
    connect_capture(&capture, &cfg.input, &format)?;
    output.connect(
        spa::utils::Direction::Output,
        None,
        StreamFlags::AUTOCONNECT
            | StreamFlags::MAP_BUFFERS
            | StreamFlags::RT_PROCESS
            | StreamFlags::TRIGGER,
        &mut [spa::pod::Pod::from_bytes(&format).unwrap()],
    )?;

    // Config hot-reload and the level meter run on the main loop.
    let tick = if opts.meter {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(500)
    };
    let watcher = RefCell::new(ConfigWatcher::new(
        opts.config_path.clone(),
        opts.config.clone(),
    ));
    let params_tx = RefCell::new(params_tx);
    let reload_capture = capture.clone();
    let timer = mainloop.loop_().add_timer(move |_| {
        if let Some((old, new)) = watcher.borrow_mut().poll() {
            if new.chain() != old.chain()
                && params_tx
                    .borrow_mut()
                    .push(ChainParams::new(&new.chain()))
                    .is_err()
            {
                status!("config reload dropped: audio thread is not keeping up");
            }
            if new.input.target() != old.input.target() {
                *device_label.borrow_mut() = input_label(&new.input);
                status!("switching input to {}", device_label.borrow());
                let _ = reload_capture.disconnect();
                if let Err(e) = connect_capture(&reload_capture, &new.input, &format) {
                    status!("could not switch input: {e:#}");
                }
            }
        }
        if opts.meter {
            print_meter(meters.take(), streaming.get());
        }
    });
    timer.update_timer(Some(tick), Some(tick)).into_result()?;

    let _sigint = mainloop.loop_().add_signal_local(pw::loop_::Signal::INT, {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    });
    let _sigterm = mainloop.loop_().add_signal_local(pw::loop_::Signal::TERM, {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    });

    mainloop.run();
    if METER_LINE.swap(false, Ordering::Relaxed) {
        eprintln!();
    }

    // Disconnect before the listeners holding cross-stream pointers go away.
    let _ = output.disconnect();
    let _ = capture.disconnect();
    drop(_output_listener);
    drop(_capture_listener);
    if connection_lost.get() {
        anyhow::bail!("lost the connection to PipeWire");
    }
    Ok(())
}

fn input_label(input: &InputConfig) -> String {
    input
        .target()
        .unwrap_or("the system default mic")
        .to_string()
}

/// Connects the capture stream to the configured mic. WirePlumber does the
/// linking: it follows `target.object` when set, and the system default mic
/// when not.
fn connect_capture(capture: &Stream, input: &InputConfig, format: &[u8]) -> Result<()> {
    set_target(capture, input.target())?;
    capture.connect(
        spa::utils::Direction::Input,
        None,
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::RT_PROCESS,
        &mut [spa::pod::Pod::from_bytes(format).unwrap()],
    )?;
    Ok(())
}

/// Sets or clears `target.object` on a disconnected stream. The bindings
/// don't wrap `pw_stream_update_properties`, so this calls it directly; a
/// NULL value removes the key.
fn set_target(stream: &Stream, target: Option<&str>) -> Result<()> {
    let key = CString::new("target.object")?;
    let value = target
        .map(CString::new)
        .transpose()
        .context("mic name contains a NUL byte")?;
    let item = spa::sys::spa_dict_item {
        key: key.as_ptr(),
        value: value.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
    };
    let dict = spa::sys::spa_dict {
        flags: 0,
        n_items: 1,
        items: &item,
    };
    // SAFETY: the dict and its strings live until the call returns, and
    // PipeWire copies what it keeps.
    unsafe { pw::sys::pw_stream_update_properties(stream.as_raw_ptr(), &dict) };
    Ok(())
}

/// Lives on PipeWire's real-time data thread. Must not allocate or block.
struct Processor {
    chain: Chain,
    params_rx: rtrb::Consumer<ChainParams>,
    capture: *const Stream,
    /// Block size of the previous cycle, reused if a cycle has no input.
    last_len: usize,
}

impl Processor {
    fn process(&mut self, output: &Stream) {
        // SAFETY: the capture stream outlives the output listener.
        let capture = unsafe { &*self.capture };

        // Keep only the newest captured buffer; older ones go back to the pool.
        let mut input = None;
        while let Some(b) = capture.dequeue_buffer() {
            input = Some(b);
        }
        let Some(mut out_buf) = output.dequeue_buffer() else {
            return;
        };

        while let Ok(p) = self.params_rx.pop() {
            self.chain.set_params(p);
        }

        let requested = out_buf.requested() as usize;
        let out_data = &mut out_buf.datas_mut()[0];
        let len = {
            let Some(bytes) = out_data.data() else { return };
            // SAFETY: f32 has no invalid bit patterns; misalignment is checked.
            let (head, out, _) = unsafe { bytes.align_to_mut::<f32>() };
            if !head.is_empty() {
                return;
            }
            let mut len = out.len();
            if requested > 0 {
                len = len.min(requested);
            }

            let copied = input.as_mut().and_then(|b| {
                let d = &mut b.datas_mut()[0];
                let (offset, size) = (d.chunk().offset() as usize, d.chunk().size() as usize);
                let bytes = d.data()?;
                let end = (offset + size).min(bytes.len());
                let (head, samples, _) = unsafe { bytes[offset.min(end)..end].align_to::<f32>() };
                if !head.is_empty() {
                    return None;
                }
                let n = len.min(samples.len());
                out[..n].copy_from_slice(&samples[..n]);
                Some(n)
            });
            let len = match copied {
                Some(n) => n,
                None => {
                    let n = len.min(self.last_len);
                    out[..n].fill(0.0);
                    n
                }
            };
            self.chain.process(&mut out[..len]);
            len
        };
        self.last_len = len;

        let chunk = out_data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = std::mem::size_of::<f32>() as i32;
        *chunk.size_mut() = (len * std::mem::size_of::<f32>()) as u32;
    }
}

/// Mono f32 at 48 kHz, which RNNoise needs. PipeWire converts the mic's
/// native format and rate to this, and converts our output for each app.
fn format_pod() -> Result<Vec<u8>> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(1);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| anyhow::anyhow!("building audio format: {e:?}"))?
    .0
    .into_inner();
    Ok(bytes)
}

/// Reloads the config file when its modification time changes.
struct ConfigWatcher {
    path: PathBuf,
    modified: Option<SystemTime>,
    current: Config,
}

impl ConfigWatcher {
    fn new(path: PathBuf, current: Config) -> Self {
        let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        Self {
            path,
            modified,
            current,
        }
    }

    /// Returns `(previous, new)` if the file changed and is valid.
    fn poll(&mut self) -> Option<(Config, Config)> {
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified == self.modified {
            return None;
        }
        self.modified = modified;
        match config::Config::load(&self.path) {
            Ok(new) if new == self.current => None,
            Ok(new) => {
                if new.output != self.current.output {
                    status!("config reloaded; restart mic-filter to apply [output] changes");
                } else {
                    status!("config reloaded");
                }
                let old = std::mem::replace(&mut self.current, new.clone());
                Some((old, new))
            }
            Err(e) => {
                status!("config not reloaded: {e:#}");
                None
            }
        }
    }
}

fn print_meter(r: MeterReading, streaming: bool) {
    let mut out = std::io::stderr().lock();
    METER_LINE.store(true, Ordering::Relaxed);
    if !streaming {
        let _ = write!(
            out,
            "\r\x1b[2Kidle: no app is recording from the virtual mic"
        );
    } else {
        let _ = write!(
            out,
            "\r\x1b[2Kin {:>6.1} dB {}  out {:>6.1} dB {}  comp -{:>4.1} dB  voice {:>3.0}%",
            r.input_peak_db.max(-99.9),
            bar(r.input_peak_db),
            r.output_peak_db.max(-99.9),
            bar(r.output_peak_db),
            r.reduction_db,
            r.vad * 100.0,
        );
    }
    let _ = out.flush();
}

/// A 20-cell bar spanning -60..0 dBFS.
fn bar(db: f32) -> String {
    let cells = (((db + 60.0) / 3.0).clamp(0.0, 20.0)) as usize;
    format!("[{}{}]", "#".repeat(cells), " ".repeat(20 - cells))
}
