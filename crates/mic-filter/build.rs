//! Picks the audio backend, and on macOS embeds Info.plist in the binary so
//! macOS can ask for microphone access.
//!
//! - `pipewire_backend`: Linux. mic-filter publishes its own virtual mic.
//! - `cpal_backend`: macOS, or Linux with the `cpal-backend` feature. mic-filter
//!   plays into an existing virtual device such as BlackHole.
//! - `audio_backend`: either of the above.

use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(pipewire_backend, cpal_backend, audio_backend)");
    println!("cargo::rerun-if-changed=macos/Info.plist");

    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let cpal_feature = env::var_os("CARGO_FEATURE_CPAL_BACKEND").is_some();
    let backend = match os.as_str() {
        "macos" => Some("cpal_backend"),
        "linux" if cpal_feature => Some("cpal_backend"),
        "linux" => Some("pipewire_backend"),
        _ => None,
    };
    if let Some(backend) = backend {
        println!("cargo::rustc-cfg={backend}");
        println!("cargo::rustc-cfg=audio_backend");
    }

    if os == "macos" {
        embed_info_plist();
    }
}

/// A command-line binary has no bundle to carry an Info.plist, and without
/// NSMicrophoneUsageDescription macOS won't let it record. The linker can
/// put the plist in a `__TEXT,__info_plist` section, which macOS reads instead.
fn embed_info_plist() {
    let template = std::fs::read_to_string("macos/Info.plist").expect("reading macos/Info.plist");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("Info.plist");
    std::fs::write(&out, template.replace("@VERSION@", &version)).expect("writing Info.plist");
    for arg in ["-sectcreate", "__TEXT", "__info_plist"] {
        println!("cargo::rustc-link-arg-bins={arg}");
    }
    println!("cargo::rustc-link-arg-bins={}", out.display());
}
