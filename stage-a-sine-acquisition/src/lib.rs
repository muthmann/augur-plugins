//! Shared A1/A3 acquisition coordinator and A1 scientific helpers.
//!
//! This crate intentionally contains no serial transport, Teensy client, or
//! PDQ writer. Hardware ownership remains with the Stage-A modulation and
//! photodiode plugins; the coordinator uses their routed service contracts.

pub mod phase;
pub mod rates;
pub mod response_curve;
mod runtime;
pub mod types;

/// Shared protocol and telemetry contracts.
pub use stage_a_plugin_contract::{csv, protocol, telemetry as sensor};

pub use runtime::{StageAA1Plugin, StageAA3Plugin};
pub use types::{CameraEvent, Polarity};
