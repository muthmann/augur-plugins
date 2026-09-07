//! Stage-A A4: reproducible contrast-threshold measurements on the IMX636.
//!
//! At one fixed optical condition, A4 walks a protocol of `diff_on`/`diff_off`
//! bias pairs, confirms each against the sensor's own readback before it
//! records, and writes a RAW file per point with the provenance needed to read
//! an event rate against a threshold setting months later.
//!
//! This crate owns no hardware. Biases are changed through the host's generic
//! camera-configuration session (augur-rs ADR 037), the only way a plugin can
//! touch the sensor. That session carries a whole configuration, so keeping the
//! survey to two registers is A4's own job: it clones the configuration the
//! host confirmed when the run opened, and changes exactly two fields.

pub mod protocol;
pub mod qc;
mod runtime;
mod sidecar;

pub use runtime::StageAA4Plugin;
