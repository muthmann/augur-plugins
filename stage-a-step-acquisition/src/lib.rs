//! Shared A2/A5 optical step-latency recorder.
//!
//! The recorder owns no hardware. It runs validated protocol points through
//! the persistent modulation and photodiode owners and the host camera
//! recorder. Scientific latency fits remain offline.
//!
//! This crate exports no plugin vtable. The A2 runtime plugin wraps
//! `StageAA2Plugin` directly; A5 embeds it as its transport layer. Each
//! runtime crate exports its own vtable, so neither links the other.

pub mod protocol;
mod resume;
mod runtime;

pub use runtime::StageAA2Plugin;
