//! Pure scientific and workflow core for the Stage-A A1 experiment.
//!
//! This crate intentionally contains no serial transport, Teensy client, or
//! PDQ writer. Hardware ownership remains with the Stage-A modulation and
//! photodiode plugins; this code only validates and analyses immutable inputs.

pub mod phase;
pub mod rates;
pub mod response_curve;
mod runtime;
pub mod types;

/// Host sensor-telemetry compaction, the CSV splitter and the declarative
/// recording protocol. All three live in the contract crate: A4 gathers the
/// same host-written CSV, and the modulation and photodiode owners validate
/// the shipped protocols against their own limits. A plugin crate must never
/// depend on another plugin crate — every one of them exports
/// `augur_plugin_vtable`, and two of those in one binary do not link on
/// Windows or Linux (ADR 031).
pub use stage_a_plugin_contract::{csv, protocol, telemetry as sensor};

pub use runtime::StageAA1Plugin;
pub use types::{CameraEvent, Polarity};
