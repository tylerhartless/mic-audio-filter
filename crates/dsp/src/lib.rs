//! Platform-independent voice processing for mic-filter.
//!
//! Nothing here touches audio devices: a backend feeds mono 48 kHz `f32`
//! blocks to [`Chain::process`] from its audio callback.

pub mod biquad;
pub mod chain;
pub mod deesser;
pub mod denoise;
pub mod dynamics;
pub mod settings;

pub use chain::{Chain, ChainParams, MeterReading, Meters, SAMPLE_RATE};
pub use settings::*;
