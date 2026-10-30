//! The cpal backend, used on macOS. macOS has no way for a program to create
//! a mic, so this one plays the processed voice into a virtual audio device
//! that apps record from, BlackHole by default.
//!
//! - The capture stream reads the real mic at whatever rate and channel count
//!   it already runs at, mixes it to mono, and hands it to a [`bridge`].
//! - The playback stream runs on the virtual device's clock at 48 kHz. Each
//!   callback pulls a block from the bridge, runs the chain on it, and writes
//!   it to every channel.
//! - The main thread checks the devices once a second and rebuilds both
//!   streams when the mic to use changes, such as when the configured mic is
//!   plugged in or the system default changes.
//!
//! Unlike the PipeWire backend, the mic stays open while mic-filter runs: an
//! app recording from BlackHole is indistinguishable from none.
//!
//! On Linux, building with the `cpal-backend` feature runs this through
//! PipeWire, to test it without a Mac.

use crate::bridge;
use crate::config::{Config, InputConfig};
use crate::devices::{Source, Sources};
use crate::ipc::{self, MeterServer, Snapshot};
use crate::service::{self, ConfigWatcher, Options};
use anyhow::{anyhow, bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, Device, ErrorKind, InterfaceType, Stream, StreamConfig, SupportedBufferSize,
};
use mic_filter_dsp::{Chain, ChainParams, Meters, SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often to look for a better mic, or for devices that came back.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// How long the mic may send pure silence before mic-filter suggests that
/// macOS is blocking it.
const SILENCE_HINT_AFTER: Duration = Duration::from_secs(3);
/// The longest block the playback callback processes at once.
const MAX_BLOCK: usize = 4096;

/// Set by SIGINT and SIGTERM.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// Runs until SIGINT/SIGTERM.
pub fn run(opts: Options) -> Result<()> {
    // SAFETY: the handler only stores to an atomic, which is
    // async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    let meters = Arc::new(Meters::default());
    let meter_server = match MeterServer::start(ipc::socket_path(&opts.config.output.name)) {
        Ok(server) => Some(server),
        Err(e) => {
            status!("GUI meters are off: {e}");
            None
        }
    };
    let mut watcher = ConfigWatcher::new(opts.config_path.clone(), opts.config.clone());
    let mut config = opts.config;
    let mut pipeline: Option<Pipeline> = None;
    let mut next_check = Instant::now();
    let mut problem = Repeats::default();
    let mut dropouts = 0;
    let mut skips = 0;
    let tick = Duration::from_millis(50);

    while !STOP.load(Ordering::Relaxed) {
        if let Some((old, new)) = watcher.poll() {
            if new.chain() != old.chain() {
                if let Some(p) = &mut pipeline {
                    if p.params_tx.push(ChainParams::new(&new.chain())).is_err() {
                        status!("config reload dropped: audio thread is not keeping up");
                    }
                }
            }
            if new.input.target() != old.input.target() {
                next_check = Instant::now();
            }
            config = new;
        }

        if let Some(e) = pipeline.as_ref().and_then(Pipeline::error) {
            status!("audio stopped ({e}); reconnecting");
            pipeline = None;
        }

        if Instant::now() >= next_check {
            next_check = Instant::now() + CHECK_INTERVAL;
            match choose(&config) {
                Ok(choice) => {
                    let current = pipeline.as_ref().map(|p| (&p.input_id, &p.output_id));
                    if current != Some((&choice.input_id, &choice.output_id)) {
                        // Release the devices before opening them again.
                        pipeline = None;
                        match Pipeline::start(&choice, &config, meters.clone()) {
                            Ok(p) => {
                                problem.clear();
                                status!("{}", choice.describe());
                                pipeline = Some(p);
                            }
                            Err(e) => problem.say(format!("could not start audio: {e:#}")),
                        }
                    }
                }
                Err(e) => {
                    pipeline = None;
                    problem.say(format!("{e:#}"));
                }
            }
        }

        let streaming = pipeline.as_mut().is_some_and(Pipeline::flowing);
        if let Some(p) = &mut pipeline {
            if p.silence_hint_due() {
                status!(
                    "the mic sends only silence. Check that it isn't muted, and that \
                     mic-filter is allowed under System Settings > Privacy & Security > \
                     Microphone"
                );
            }
            let now = p.stats.dropouts.load(Ordering::Relaxed);
            if now > dropouts {
                status!("audio dropout; buffering a little more to prevent it");
            }
            dropouts = now;
            let now = p.stats.skips.load(Ordering::Relaxed);
            if now > skips {
                status!("audio backed up; skipped ahead to keep the delay low");
            }
            skips = now;
        }

        let reading = meters.take();
        if let Some(server) = &meter_server {
            server.publish(Snapshot {
                input_db: reading.input_peak_db,
                output_db: reading.output_peak_db,
                reduction_db: reading.reduction_db,
                vad: reading.vad,
                streaming,
                gate_db: reading.gate_peak_db,
                gate_open: reading.gate_open,
            });
        }
        if opts.meter {
            service::print_meter(reading, streaming, "waiting for audio from the mic");
        }
        std::thread::sleep(tick);
    }
    service::finish_meter();
    Ok(())
}

/// Prints a problem once, rather than on every retry.
#[derive(Default)]
struct Repeats {
    last: Option<String>,
}

impl Repeats {
    fn say(&mut self, message: String) {
        if self.last.as_ref() != Some(&message) {
            status!("{message}");
            self.last = Some(message);
        }
    }

    fn clear(&mut self) {
        self.last = None;
    }
}

/// The devices one pipeline runs between.
struct Choice {
    input: Device,
    input_id: String,
    input_name: String,
    /// The configured mic, when it isn't connected and this is a stand-in.
    missing: Option<String>,
    output: Device,
    output_id: String,
    output_name: String,
}

impl Choice {
    fn describe(&self) -> String {
        let source = match &self.missing {
            Some(wanted) => format!(
                "{wanted} isn't connected; capturing from {} until it is",
                self.input_name
            ),
            None => format!("capturing from {}", self.input_name),
        };
        format!(
            "{source}; playing into {}. Select it as the mic in your apps",
            self.output_name
        )
    }
}

/// Picks the mic and the output device from the config and what's
/// connected right now.
fn choose(config: &Config) -> Result<Choice> {
    let host = cpal::default_host();
    let (output, output_id, output_name) = find_output(&host, &config.output.name)?;
    let (input, missing) = pick_input(&host, &config.input, &output_id)?;
    Ok(Choice {
        input_id: id_of(&input).unwrap_or_default(),
        input_name: name_of(&input),
        input,
        missing,
        output,
        output_id,
        output_name,
    })
}

/// The `[output] name` device: by UID, by name, or by a piece of a name that
/// only one device has.
fn find_output(host: &cpal::Host, wanted: &str) -> Result<(Device, String, String)> {
    let mut devices: Vec<(Device, String, String)> = host
        .output_devices()
        .context("listing audio devices")?
        .filter_map(|d| Some((id_of(&d)?, name_of(&d), d)))
        .map(|(id, name, d)| (d, id, name))
        .collect();
    let exact = devices
        .iter()
        .position(|(_, id, name)| id == wanted || name.eq_ignore_ascii_case(wanted));
    let needle = wanted.to_lowercase();
    let partial: Vec<usize> = (0..devices.len())
        .filter(|&i| devices[i].2.to_lowercase().contains(&needle))
        .collect();
    let index = match (exact, partial.as_slice()) {
        (Some(i), _) => i,
        (None, [i]) => *i,
        (None, []) => bail!(
            "there's no audio device named \"{wanted}\" to play into. Install BlackHole \
             (brew install blackhole-2ch), or set [output] name to another virtual device"
        ),
        (None, many) => bail!(
            "\"{wanted}\" matches {} audio devices ({}); set [output] name to the full name",
            many.len(),
            many.iter()
                .map(|&i| devices[i].2.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Ok(devices.swap_remove(index))
}

/// The configured mic if it's connected, otherwise the system default, and
/// failing that the likeliest voice mic. Never the output device, which
/// would feed mic-filter its own voice.
fn pick_input(
    host: &cpal::Host,
    input: &InputConfig,
    output_id: &str,
) -> Result<(Device, Option<String>)> {
    let mut devices: Vec<(String, Device)> = host
        .input_devices()
        .context("listing audio devices")?
        .filter_map(|d| Some((id_of(&d)?, d)))
        .filter(|(id, _)| id != output_id)
        .collect();
    let take = |devices: &mut Vec<(String, Device)>, id: &str| {
        let i = devices.iter().position(|(d, _)| d == id)?;
        Some(devices.swap_remove(i).1)
    };

    let wanted = input.target();
    if let Some(wanted) = wanted {
        if let Some(device) = take(&mut devices, wanted) {
            return Ok((device, None));
        }
    }
    let missing = wanted.map(str::to_string);
    let default = host.default_input_device().and_then(|d| id_of(&d));
    if let Some(device) = default.and_then(|id| take(&mut devices, &id)) {
        return Ok((device, missing));
    }
    let sources = sources_from(&devices, None);
    let best = sources
        .best_hardware()
        .or(sources.list.first())
        .map(|s| s.name.clone());
    match best.and_then(|id| take(&mut devices, &id)) {
        Some(device) => Ok((device, missing)),
        None => bail!("no mic found; waiting for one to be connected"),
    }
}

/// Every mic, for `list`, `use` and the window.
pub fn query() -> Result<Sources> {
    let host = cpal::default_host();
    let devices: Vec<(String, Device)> = host
        .input_devices()
        .context("listing audio devices")?
        .filter_map(|d| Some((id_of(&d)?, d)))
        .collect();
    let default = host.default_input_device().and_then(|d| id_of(&d));
    Ok(sources_from(&devices, default))
}

fn sources_from(devices: &[(String, Device)], default: Option<String>) -> Sources {
    let mut sources = Sources {
        list: devices
            .iter()
            .map(|(id, device)| {
                let description = name_of(device);
                Source {
                    hardware: !looks_virtual(device, &description),
                    name: id.clone(),
                    description,
                }
            })
            .collect(),
        default,
    };
    sources.sort();
    sources
}

/// The device's stable id: the CoreAudio UID, or the PipeWire node.name.
fn id_of(device: &Device) -> Option<String> {
    device.id().ok().map(|id| id.id().to_string())
}

fn name_of(device: &Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "an unnamed device".into())
}

/// CoreAudio reports aggregate devices to cpal but not other virtual ones,
/// so this also goes by the names of common virtual devices.
fn looks_virtual(device: &Device, name: &str) -> bool {
    const NAMES: [&str; 8] = [
        "blackhole",
        "loopback",
        "soundflower",
        "zoomaudiodevice",
        "teams audio",
        "aggregate",
        "multi-output",
        "virtual",
    ];
    let kind = device.description().map(|d| d.interface_type());
    let name = name.to_lowercase();
    matches!(kind, Ok(InterfaceType::Virtual | InterfaceType::Aggregate))
        || NAMES.iter().any(|n| name.contains(n))
}

/// What the audio callbacks report to the main thread.
#[derive(Default)]
struct Shared {
    /// Capture callbacks so far.
    captured: AtomicUsize,
    /// Set once the mic delivers anything other than digital silence.
    heard: AtomicBool,
    error: Mutex<Option<String>>,
}

impl Shared {
    fn error_handler(self: &Arc<Self>, what: &'static str) -> impl FnMut(cpal::Error) + Send {
        let shared = self.clone();
        move |e| {
            // The stream keeps running after these: a stream following the
            // default device rerouted itself, or the system was too busy to
            // run a callback on time, which the bridge rides out.
            if matches!(e.kind(), ErrorKind::DeviceChanged | ErrorKind::Xrun) {
                return;
            }
            let mut slot = shared.error.lock().unwrap();
            slot.get_or_insert_with(|| format!("{what}: {e}"));
        }
    }
}

/// The mic and output streams of one running setup.
struct Pipeline {
    input_id: String,
    output_id: String,
    params_tx: rtrb::Producer<ChainParams>,
    shared: Arc<Shared>,
    stats: Arc<bridge::Stats>,
    started: Instant,
    /// Capture count at the last check, and when it last moved.
    last_captured: usize,
    last_audio: Option<Instant>,
    silence_hinted: bool,
    _input: Stream,
    _output: Stream,
}

impl Pipeline {
    fn start(choice: &Choice, config: &Config, meters: Arc<Meters>) -> Result<Pipeline> {
        let shared = Arc::new(Shared::default());
        let (params_tx, mut params_rx) = rtrb::RingBuffer::<ChainParams>::new(16);
        let mut chain = Chain::new(ChainParams::new(&config.chain()), meters);

        let (mut reader, input) = capture(&choice.input, SAMPLE_RATE, &shared)
            .with_context(|| format!("opening {}", choice.input_name))?;
        let stats = reader.stats();
        let output = playback(
            &choice.output,
            Some(SAMPLE_RATE),
            &shared,
            move |block: &mut [f32]| {
                while let Ok(p) = params_rx.pop() {
                    chain.set_params(p);
                }
                reader.fill(block);
                chain.process(block);
            },
        )
        .with_context(|| format!("opening {}", choice.output_name))?;

        Ok(Pipeline {
            input_id: choice.input_id.clone(),
            output_id: choice.output_id.clone(),
            params_tx,
            shared,
            stats,
            started: Instant::now(),
            last_captured: 0,
            last_audio: None,
            silence_hinted: false,
            _input: input,
            _output: output,
        })
    }

    fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().take()
    }

    /// The mic delivered audio in the last half second.
    fn flowing(&mut self) -> bool {
        let captured = self.shared.captured.load(Ordering::Relaxed);
        if captured != self.last_captured {
            self.last_captured = captured;
            self.last_audio = Some(Instant::now());
        }
        self.last_audio
            .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
    }

    /// True once, when the mic has run for a while without making a sound.
    /// macOS sends silence to programs it doesn't allow to record.
    fn silence_hint_due(&mut self) -> bool {
        let due = !self.silence_hinted
            && self.last_audio.is_some()
            && self.started.elapsed() > SILENCE_HINT_AFTER
            && !self.shared.heard.load(Ordering::Relaxed);
        self.silence_hinted |= due;
        due
    }
}

/// Opens `device` for recording at its current rate and channel count, mixed
/// to mono into a bridge whose reader plays at `out_rate`. The stream is
/// running when this returns.
fn capture(
    device: &Device,
    out_rate: u32,
    shared: &Arc<Shared>,
) -> Result<(bridge::Reader, Stream)> {
    let supported = device.default_input_config()?;
    let channels = supported.channels() as usize;
    let rate = supported.sample_rate();
    let (mut feeder, reader) = bridge::bridge(rate, out_rate);
    let config = StreamConfig {
        channels: channels as u16,
        sample_rate: rate,
        buffer_size: block_size(supported.buffer_size(), rate),
    };
    let callback_shared = shared.clone();
    let scale = 1.0 / channels as f32;
    let stream = device.build_input_stream::<f32, _, _>(
        config,
        move |data: &[f32], _| {
            if !callback_shared.heard.load(Ordering::Relaxed) && data.iter().any(|&x| x != 0.0) {
                callback_shared.heard.store(true, Ordering::Relaxed);
            }
            feeder.push(
                data.chunks_exact(channels)
                    .map(|frame| frame.iter().sum::<f32>() * scale),
            );
            callback_shared.captured.fetch_add(1, Ordering::Relaxed);
        },
        shared.error_handler("mic"),
        None,
    )?;
    stream.play()?;
    Ok((reader, stream))
}

/// Opens `device` for playback, at `rate` or else its current rate, and
/// fills every channel with the mono blocks `render` produces.
fn playback(
    device: &Device,
    rate: Option<u32>,
    shared: &Arc<Shared>,
    mut render: impl FnMut(&mut [f32]) + Send + 'static,
) -> Result<Stream> {
    let supported = device.default_output_config()?;
    let channels = supported.channels() as usize;
    let rate = rate.unwrap_or(supported.sample_rate());
    let config = StreamConfig {
        channels: channels as u16,
        sample_rate: rate,
        buffer_size: block_size(supported.buffer_size(), rate),
    };
    let mut block = vec![0.0f32; MAX_BLOCK].into_boxed_slice();
    let stream = device.build_output_stream::<f32, _, _>(
        config,
        move |data: &mut [f32], _| {
            for frames in data.chunks_mut(MAX_BLOCK * channels) {
                let block = &mut block[..frames.len() / channels];
                render(block);
                for (frame, &x) in frames.chunks_exact_mut(channels).zip(block.iter()) {
                    frame.fill(x);
                }
            }
        },
        shared.error_handler("output"),
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// Asks for 5 ms callbacks, for low delay, when the device says what it
/// allows. Otherwise the device decides.
fn block_size(supported: &SupportedBufferSize, rate: u32) -> BufferSize {
    match supported {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed((rate / 200).clamp(*min, (*max).max(*min)))
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    }
}

/// "Hear myself" in the window: plays the virtual device into the speakers
/// or headphones that are the system output right now.
#[cfg_attr(not(feature = "gui"), allow(dead_code))]
pub struct Monitor {
    shared: Arc<Shared>,
    _input: Stream,
    _output: Stream,
}

#[cfg_attr(not(feature = "gui"), allow(dead_code))]
impl Monitor {
    /// `output_name` is the `[output] name` mic-filter plays into.
    pub fn start(output_name: &str) -> Result<Monitor> {
        let host = cpal::default_host();
        let (_, virtual_id, virtual_name) = find_output(&host, output_name)?;
        let source = host
            .input_devices()?
            .find(|d| id_of(d).as_deref() == Some(virtual_id.as_str()))
            .ok_or_else(|| anyhow!("{virtual_name} has no recording side to listen to"))?;
        // A fixed device, so a later change of system output can't loop.
        let speakers_id = host
            .default_output_device()
            .and_then(|d| id_of(&d))
            .ok_or_else(|| anyhow!("there's no system output to play into"))?;
        if speakers_id == virtual_id {
            bail!(
                "the system output is {virtual_name}, so this would play mic-filter into \
                 itself. Choose your speakers or headphones in System Settings > Sound"
            );
        }
        let speakers = host
            .output_devices()?
            .find(|d| id_of(d).as_deref() == Some(speakers_id.as_str()))
            .ok_or_else(|| anyhow!("the system output disappeared"))?;
        let out_rate = speakers.default_output_config()?.sample_rate();

        let shared = Arc::new(Shared::default());
        let (mut reader, input) = capture(&source, out_rate, &shared)?;
        let output = playback(&speakers, None, &shared, move |block| reader.fill(block))?;
        Ok(Monitor {
            shared,
            _input: input,
            _output: output,
        })
    }

    /// Why it stopped, if it did.
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_aim_for_five_ms() {
        let range = SupportedBufferSize::Range { min: 64, max: 4096 };
        assert_eq!(block_size(&range, 48_000), BufferSize::Fixed(240));
        let narrow = SupportedBufferSize::Range { min: 512, max: 512 };
        assert_eq!(block_size(&narrow, 48_000), BufferSize::Fixed(512));
        assert_eq!(
            block_size(&SupportedBufferSize::Unknown, 48_000),
            BufferSize::Default
        );
    }
}
