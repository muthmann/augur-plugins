//! Stage-A A2: repeated optical-step latency acquisition.
//!
//! This plugin owns no hardware. It runs validated protocol points through the
//! persistent modulation and photodiode owners and the host camera recorder.
//! Scientific latency fits remain offline.

pub mod protocol;
mod resume;
mod runtime;

pub use runtime::StageAA2Plugin;
