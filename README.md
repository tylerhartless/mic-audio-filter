# mic-filter

mic-filter processes your microphone once for every app. It reads any mic you choose, runs it through gain, a high-pass filter, RNNoise noise suppression, EQ, a compressor and a limiter, and publishes the result as a virtual mic named **Mic Filter**. Every app that selects Mic Filter hears the same processed voice, so per-app noise suppression and auto gain can stay off.

Linux with PipeWire is supported today. macOS and Windows are planned.[*](#speculation)

## How it works

```
real mic --> mic-filter --> "Mic Filter" virtual mic --> Discord, Zoom, OBS, browsers, ...

inside mic-filter:
  input gain -> high-pass -> RNNoise -> EQ -> compressor -> output gain -> limiter
```

- mic-filter is a separate process. If it crashes, PipeWire and the rest of your audio keep running.
- The real mic only runs while an app is recording from Mic Filter. With no app recording, mic-filter uses no CPU.
- Processing runs inside the same PipeWire cycle that captures the audio. The added delay is 20 ms with noise suppression on (RNNoise works in 10 ms frames and lags one frame) and 0 ms with it off.
- On the development machine the chain takes about 18 microseconds per 5.3 ms audio cycle, which is about 0.3% of one CPU core.
- Config edits apply live while it runs, including switching to a different mic. An invalid edit is rejected with an error and the previous settings stay in effect.
- The capture side and the virtual mic share a PipeWire link group. WirePlumber refuses to link a node to its own link group, so mic-filter can never record its own output, even when Mic Filter is the system default.

## Requirements

- Linux with PipeWire 1.0 or newer and WirePlumber.
- Rust 1.80 or newer. Install it with [rustup](https://rustup.rs).
- PipeWire development headers and clang:

  | Distro | Command |
  | --- | --- |
  | Ubuntu, Debian, Zorin, Mint, Pop!_OS | `sudo apt install libpipewire-0.3-dev libspa-0.2-dev clang pkg-config` |
  | Fedora | `sudo dnf install pipewire-devel clang` |
  | Arch | `sudo pacman -S pipewire clang` |

## Install

```bash
git clone https://github.com/tylerhartless/mic-audio-filter.git
cd mic-audio-filter
./scripts/install.sh
```

The script:

1. Builds mic-filter and installs it to `~/.cargo/bin/mic-filter`.
2. Writes `~/.config/mic-filter/config.toml` if no config exists yet. The starting mic is the system default when that is a real mic, otherwise the one that looks most like a headset. Change it any time with `mic-filter use`.
3. Installs and starts the `mic-filter` systemd user service, which starts with PipeWire at login.

Then pick **Mic Filter** as the input in each app, or make it the system default:

```bash
pactl set-default-source mic_filter
```

In apps that have their own processing (Discord, Zoom, Teams, browsers), turn off noise suppression and automatic gain control so they don't fight mic-filter.

## Choosing a mic

mic-filter works with any input PipeWire has: USB and Bluetooth headsets, analog jacks, webcams, audio interfaces, and other virtual sources. Pick one with `mic-filter use`:

```bash
mic-filter use           # numbered picker
mic-filter use 2         # by number from `mic-filter list`
mic-filter use headset   # by part of its name
mic-filter use default   # follow the system default mic
```

`mic-filter list` shows the numbers:

```
  1. USB Headset Mono  (in use)
     alsa_input.usb-Headset_Maker-00.mono-fallback
  2. Built-in Audio Analog Stereo  (system default)
     alsa_input.pci-0000_00_1f.3.analog-stereo
  3. Webcam Analog Stereo
     alsa_input.usb-Webcam-02.analog-stereo
  d. Follow the system default mic
```

The choice is saved as `[input] device` in the config. A running mic-filter switches within a second. The virtual mic stays up during the switch, so apps that have Mic Filter selected keep it and hear a short gap.

When the chosen mic is unplugged, PipeWire moves the capture to another available mic, or leaves it waiting when there is none. When the mic is plugged back in, mic-filter captures from it again.[*](#speculation)

`default` follows the system default mic. When Mic Filter itself is the default, PipeWire picks the highest-priority other mic that has something plugged in, and that can be a webcam or the built-in jack. Naming the mic with `use` is more predictable.

## Usage

| Command | Effect |
| --- | --- |
| `mic-filter run` | Runs the virtual mic. This is the default when no command is given. |
| `mic-filter run --meter` | Same, with a live level meter. |
| `mic-filter list` | Lists the mics, numbered, with the one in use and the system default marked. |
| `mic-filter use [mic]` | Switches to a mic: a number, part of a name, a full node name, or `default`. Asks when no mic is given. |
| `mic-filter init` | Writes a starter config. `--device <mic>` picks the mic, `--force` overwrites. |
| `mic-filter check` | Validates the config file and exits. |

Every command takes `--config <path>` to use a config other than `~/.config/mic-filter/config.toml`.

## Tuning

The service and a manual `run` would both create a Mic Filter node, so stop the service while tuning:

```bash
systemctl --user stop mic-filter
mic-filter run --meter
```

Record from Mic Filter in any app (or `pw-record --target mic_filter /dev/null`) and talk normally. The meter shows:

```
in  -16.2 dB [#############       ]  out  -6.8 dB [##################  ]  comp -5.1 dB  voice  97%
```

| Reading | Meaning | Target while speaking |
| --- | --- | --- |
| `in` | Peak level after `gain.input_db` | -18 to -10 dB. Raise `input_db` if lower. |
| `out` | Peak level of what apps receive | -9 to -3 dB. Adjust `compressor.makeup_db` or `gain.output_db`. |
| `comp` | Gain reduction from the compressor | 3 to 8 dB. More means `threshold_db` is too low. |
| `voice` | RNNoise's confidence that you are talking | High while talking, near 0 while silent. |

Edit `~/.config/mic-filter/config.toml` in another window and save. Changes apply within half a second. When you're done, press Ctrl+C and run `systemctl --user start mic-filter`.

Common adjustments:

- **Background noise between sentences:** set `denoise.vad_threshold` to 0.5 to 0.8. This mutes the mic when RNNoise doesn't detect speech. Raise `vad_hold_ms` if word endings get cut off.
- **Voice sounds watery or robotic:** lower `denoise.strength` to 0.8 or 0.9.
- **Boomy or muddy:** cut more around 200 to 300 Hz in `[eq]`, or raise `highpass.freq_hz` to 100.
- **Mains hum:** add `{ type = "notch", freq_hz = 60.0, q = 10.0 }` to the EQ bands (50 Hz in Europe and most of Asia).

## Configuration

`mic-filter init` writes a commented config that documents every option. The sections are:

| Section | Purpose | Live reload |
| --- | --- | --- |
| `[input]` | `device`: the mic's node name, or `"default"`. `mic-filter use` sets it for you. | Yes |
| `[output]` | `name` and `description` of the virtual mic. | Restart needed |
| `[gain]` | `input_db` boost before processing, `output_db` trim before the limiter. | Yes |
| `[highpass]` | Removes rumble below `freq_hz`. | Yes |
| `[denoise]` | RNNoise suppression with `strength`, plus the optional voice gate. | Yes |
| `[eq]` | Up to 16 bands: `peaking`, `low_shelf`, `high_shelf`, `low_pass`, `high_pass`, `notch`. | Yes |
| `[compressor]` | Soft-knee compressor: `threshold_db`, `ratio`, `knee_db`, `attack_ms`, `release_ms`, `makeup_db`. | Yes |
| `[limiter]` | Peak limiter at `ceiling_db`, so the output never clips. | Yes |

Unknown keys and out-of-range values are errors, so mic-filter reports typos.

## Service

```bash
systemctl --user status mic-filter     # is it running?
systemctl --user restart mic-filter    # apply [output] changes
journalctl --user -u mic-filter -f     # logs
systemctl --user disable --now mic-filter
```

If PipeWire restarts, mic-filter exits and systemd starts it again against the new PipeWire.

## Troubleshooting

- **Mic Filter is missing from apps:** check `systemctl --user status mic-filter` and the journal.
- **Wrong mic:** run `mic-filter use` and pick the right one.
- **`capture error: no target node available` in the log:** no usable mic is connected. PipeWire skips inputs with nothing plugged into them, such as an empty analog jack. Capture resumes once a mic appears.
- **Another filter records from Mic Filter:** a filter whose own mic is unplugged, such as a PipeWire filter-chain with a `target.object`, can fall back to recording from Mic Filter. WirePlumber then refuses to let mic-filter use that filter as its input because it would form a loop, and mic-filter logs `no target node available`. Point mic-filter at the real mic, or remove the other filter.
- **Echo of other people:** mic-filter has no echo cancellation. Use headphones.

## Project layout

| Path | Contents |
| --- | --- |
| `crates/dsp` | The processing chain. Pure Rust with no OS or audio-device code, so it carries over to other platforms. |
| `crates/mic-filter` | The program: CLI, config, and the PipeWire backend (`engine.rs`). |
| `packaging/mic-filter.service` | systemd user unit. |
| `scripts/install.sh` | Build, install and enable. |

## Development

```bash
cargo test
cargo clippy --all-targets
cargo run -- run --meter
```

The DSP crate builds and tests on any OS. The `mic-filter` crate needs the PipeWire headers on Linux.

## Speculation

The following statements are untested plans or are based on reading WirePlumber's source code without a live test:

- **Unplug behavior:** a simulated hotplug, where a virtual mic with the configured name was created and removed while an app recorded from Mic Filter, confirmed that capture starts when the mic appears and stops cleanly when it goes away. Moving back from a fallback mic to the returning mic comes from reading WirePlumber 0.4's policy code. Neither has been tested with a real USB or Bluetooth headset unplugged mid-call.
- **macOS and Windows:** the plan is a second backend that captures with the `cpal` crate and writes to a virtual audio device such as BlackHole on macOS or VB-CABLE on Windows. Those systems need a driver to create a virtual mic, and those projects provide one. The `dsp` crate would be reused unchanged.
