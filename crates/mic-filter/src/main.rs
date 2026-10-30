//! mic-filter: one processed virtual mic for every app.

mod config;
#[cfg(target_os = "linux")]
mod devices;
#[cfg(target_os = "linux")]
mod engine;
#[cfg(all(target_os = "linux", feature = "gui"))]
mod gui;
#[cfg(target_os = "linux")]
mod ipc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Clean up your mic once, for every app: gain, EQ, compression and noise removal on a virtual mic"
)]
struct Cli {
    /// Config file [default: ~/.config/mic-filter/config.toml]
    #[arg(long, short, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the virtual mic (the default command)
    Run {
        /// Show live input/output levels, compression and voice detection
        #[arg(long, short)]
        meter: bool,
    },
    /// List the mics mic-filter can use
    List,
    /// Choose which mic to process. A running mic-filter switches to it live
    Use {
        /// A number from `list`, part of a mic's name (e.g. "aula"), a full
        /// node name, or "default". Shows a picker if omitted.
        mic: Option<String>,
    },
    /// Write a starter config file
    Init {
        /// Mic to process (anything `use` accepts); picks the best real mic if omitted
        #[arg(long, short)]
        device: Option<String>,
        /// Overwrite an existing config
        #[arg(long)]
        force: bool,
    },
    /// Validate the config file and exit
    Check,
    /// Open the window for choosing the mic and tuning the sound
    #[cfg(feature = "gui")]
    Gui,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(config::default_path);

    match cli.command.unwrap_or(Command::Run { meter: false }) {
        Command::Run { meter } => run(config_path, meter),
        Command::List => list(&config_path),
        Command::Use { mic } => use_mic(&config_path, mic),
        Command::Init { device, force } => init(&config_path, device, force),
        #[cfg(feature = "gui")]
        Command::Gui => open_gui(config_path),
        Command::Check => {
            config::Config::load(&config_path)?;
            println!("{} is valid", config_path.display());
            Ok(())
        }
    }
}

/// The config if there is a valid one, otherwise the defaults. Good enough
/// for commands that only need the device and output names.
fn config_or_default(path: &Path) -> config::Config {
    config::Config::load(path).unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn run(config_path: PathBuf, meter: bool) -> Result<()> {
    let config = if config_path.exists() {
        config::Config::load(&config_path)?
    } else {
        eprintln!(
            "no config at {}; using defaults (run `mic-filter init` to create one)",
            config_path.display()
        );
        config::Config::default()
    };
    engine::run(engine::Options {
        config_path,
        config,
        meter,
    })
}

/// Every mic except mic-filter's own output.
#[cfg(target_os = "linux")]
fn selectable_mics(config: &config::Config) -> Result<devices::Sources> {
    Ok(devices::query()?.without(&config.output.name))
}

#[cfg(target_os = "linux")]
fn print_mics(mics: &devices::Sources, config: &config::Config) {
    let in_use = config.input.target();
    if mics.list.is_empty() {
        println!("no mics found");
    }
    for (i, mic) in mics.list.iter().enumerate() {
        let mut tags = Vec::new();
        if Some(mic.name.as_str()) == in_use {
            tags.push("in use");
        }
        if Some(&mic.name) == mics.default.as_ref() {
            tags.push("system default");
        }
        if !mic.hardware {
            tags.push("virtual");
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!("  ({})", tags.join(", "))
        };
        println!("{:>3}. {}{tags}\n     {}", i + 1, mic.description, mic.name);
    }
    let follow = if in_use.is_none() { "  (in use)" } else { "" };
    println!("  d. Follow the system default mic{follow}");
    if let Some(name) = in_use.filter(|n| !mics.list.iter().any(|m| m.name == *n)) {
        println!("\nThe configured mic, {name}, is not connected right now.");
    }
}

#[cfg(target_os = "linux")]
fn list(config_path: &Path) -> Result<()> {
    let config = config_or_default(config_path);
    print_mics(&selectable_mics(&config)?, &config);
    println!("\nSwitch with `mic-filter use <number>`.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn use_mic(config_path: &Path, choice: Option<String>) -> Result<()> {
    use std::io::{IsTerminal, Write};

    let config = config_or_default(config_path);
    let mics = selectable_mics(&config)?;
    let choice = match choice {
        Some(choice) => choice,
        None => {
            if !std::io::stdin().is_terminal() {
                bail!("say which mic: mic-filter use <number|name|default>");
            }
            print_mics(&mics, &config);
            print!("\nUse which mic? [1-{}, d] ", mics.list.len());
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            if line.trim().is_empty() {
                println!("unchanged");
                return Ok(());
            }
            line
        }
    };

    let device = mics.resolve(&choice)?;
    config::set_input_device(config_path, &device)?;
    match mics.list.iter().find(|m| m.name == device) {
        Some(mic) => println!("mic-filter now processes {}", mic.description),
        None if device == config::DEFAULT_DEVICE => {
            println!("mic-filter now follows the system default mic")
        }
        None => println!("mic-filter now processes {device} (not connected right now)"),
    }
    println!(
        "Saved to {}. A running mic-filter switches within a second.",
        config_path.display()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn init(path: &Path, device: Option<String>, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    let mics = selectable_mics(&config::Config::default())?;
    let device = match device {
        Some(choice) => mics.resolve(&choice)?,
        None => match mics.best_hardware() {
            Some(mic) => {
                eprintln!(
                    "using mic: {}\n(switch any time with `mic-filter use`)",
                    mic.description
                );
                mic.name.clone()
            }
            None => {
                eprintln!("no hardware mic found; the config will follow the system default");
                config::DEFAULT_DEVICE.to_string()
            }
        },
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, config::template(Some(&device)))
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

#[cfg(all(target_os = "linux", feature = "gui"))]
fn open_gui(config_path: PathBuf) -> Result<()> {
    gui::run(config_path)
}

#[cfg(all(not(target_os = "linux"), feature = "gui"))]
fn open_gui(_: PathBuf) -> Result<()> {
    bail!("only Linux (PipeWire) is supported so far")
}

#[cfg(not(target_os = "linux"))]
fn run(_: PathBuf, _: bool) -> Result<()> {
    bail!("only Linux (PipeWire) is supported so far")
}

#[cfg(not(target_os = "linux"))]
fn list(_: &Path) -> Result<()> {
    bail!("only Linux (PipeWire) is supported so far")
}

#[cfg(not(target_os = "linux"))]
fn use_mic(_: &Path, _: Option<String>) -> Result<()> {
    bail!("only Linux (PipeWire) is supported so far")
}

#[cfg(not(target_os = "linux"))]
fn init(path: &Path, _device: Option<String>, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            path.display()
        );
    }
    std::fs::write(path, config::template(None))?;
    Ok(())
}
