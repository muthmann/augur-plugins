//! A4 constant-light background references and legacy threshold surveys.
//!
//! The bright reference preserves the confirmed camera configuration and uses
//! the existing modulation and photodiode owners for paired RAW/PDQ capture.
//! Legacy surveys clone that configuration and vary only threshold offsets.
//! Camera commands always use the host-owned configuration session.

mod devices;
pub mod protocol;
pub mod qc;
mod runtime;
mod sidecar;

pub use runtime::StageAA4Plugin;
