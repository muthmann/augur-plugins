//! Shared eveSMLM contract.
//!
//! The candidate, fitting and post-processing plugins form a chain: fitting
//! consumes what candidates publishes, and post-processing consumes what
//! fitting publishes. Expressing that by having one plugin crate depend on
//! another looks natural, but plugin crates are `cdylib`s that each export
//! `augur_plugin_vtable` — and a plugin that links another plugin's rlib pulls
//! that symbol in twice. The Apple linker tolerates the duplicate; `rust-lld`
//! and MSVC's `link.exe` do not, so the chain built on macOS and failed to link
//! on Linux and Windows.
//!
//! Everything that crosses a plugin boundary therefore lives here, in a plain
//! library crate that exports no vtable. Plugins depend on this crate, never on
//! each other.

pub mod candidates;
pub mod datasets;
pub mod localization;

pub use candidates::{
    CandidateFindingMethod, ClusterBoundary, EveCandidates, EveCluster, EveEvent,
    ACCEPTED_CANDIDATE_EVENTS_DATASET_ID, CTX_EVE_CANDIDATES,
};
pub use datasets::{
    current_localizations_dataset, current_localizations_registry,
    current_localizations_registry_for_results, current_localizations_schema,
    current_localizations_schema_for_results, localization_row_id, localization_time_bounds,
    localization_xy_bounds, to_localization_results, CURRENT_LOCALIZATIONS_3D_VIEW_ID,
    CURRENT_LOCALIZATIONS_DATASET_ID, CURRENT_LOCALIZATIONS_LAYER_ID,
    CURRENT_LOCALIZATIONS_VIEW_ID,
};
pub use localization::{
    EveLocalization, EveLocalizationResults, FitMethod, RejectedFitRow, RejectionReason,
    CTX_EVE_LOCALIZATION_RESULTS,
};
