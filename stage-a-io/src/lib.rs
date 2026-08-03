//! # stage-a-io
//!
//! Shared research-owned I/O library for the Stage-A bench plugins
//! (currently `stage-a-modulation`; the future A1–A3 experiment plugins
//! build on it too — see ADR 006).
//!
//! Scope, per the Stage-A control-software specification:
//! - the v1 ASCII command grammar and PDA1 binary frame format (wire-
//!   compatible with `stage-a-controller/include/wire_protocol.h`),
//! - a typed serial client with idempotent sequence retries and stream-
//!   integrity accounting (CRC failures, resync skips, sequence gaps,
//!   ADC overruns — any of which invalidates a measurement point),
//! - a bounded background I/O worker so plugin `process_frame()` never
//!   blocks on serial,
//! - streaming `.pdq` write/replay with CRC32, SHA-256, byte/frame counts,
//!   contiguous sample-range receipts, plus the JSON run sidecar,
//! - the calibrated optical log-contrast estimator (`a` is measured light,
//!   never the commanded DAC excursion),
//! - a mock controller for tests and hardware-free development.
//!
//! This crate deliberately contains **no** experiment policy (sweeps,
//! bisection, fits live in the protocol plugins) and **no** augur types —
//! it is plain I/O + numerics, testable without a host.

pub mod client;
pub mod estimator;
pub mod mock;
pub mod pdq;
pub mod protocol;
mod sha256;
pub mod sidecar;
pub mod transport;
pub mod wire;

pub use client::{ClientError, DeviceEvent, StageAClient, StreamIntegrity};
pub use estimator::{
    estimate_contrast, near_rail_margin, AdcCalibration, ContrastEstimate, ContrastGeometry,
    EstimateError,
};
pub use mock::{MockController, MockState, MockWave};
pub use pdq::{
    inspect_pdq, PdqReadEvent, PdqReadSummary, PdqReader, PdqSampleRange, PdqSummary, PdqWriter,
};
pub use protocol::{Command, ControlMessage, ProtocolError};
pub use sha256::Sha256Digest;
pub use sidecar::{DetectorLoad, IntegrityRecord, RunSidecar, TriggerSource};
#[cfg(feature = "hardware")]
pub use transport::SerialTransport;
pub use transport::{MockLink, MockTransport, Transport};
pub use wire::{
    Frame, FrameHeader, FrameParser, FrameType, MarkerPayload, ParseEvent, SummaryPayload,
    MARKER_SOURCE_PHASE0,
};
pub use worker::{IoWorker, WorkerOutput, WorkerRequest};

pub mod worker;
