//! Pure scientific and workflow core for the Stage-A A1 experiment.
//!
//! This crate intentionally contains no serial transport, Teensy client, or
//! PDQ writer. Hardware ownership remains with the Stage-A modulation and
//! photodiode plugins; this code only validates and analyses immutable inputs.

pub mod phase;
pub mod protocol;
pub mod rates;
pub mod response_curve;
mod runtime;
pub mod types;

/// Host sensor-telemetry compaction. Shared with A4 through the contract
/// crate, because both workflows gather the same host-written CSV and a second
/// copy would drift the moment the host adds a column.
pub use stage_a_plugin_contract::{csv, telemetry as sensor};

pub use runtime::StageAA1Plugin;
pub use types::{CameraEvent, Polarity};
