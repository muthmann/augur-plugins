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

pub use runtime::StageAA1Plugin;
pub use types::{CameraEvent, Polarity};
