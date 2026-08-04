//! Internal cluster-tracking bookkeeping.
//!
//! Not part of the cross-plugin contract — the shared eveSMLM types live in
//! the `evesmlm-types` crate.

use evesmlm_types::EveCluster;

#[derive(Debug, Clone)]
pub(crate) struct TrackedCluster {
    pub id: u64,
    pub centroid_x: f64,
    pub centroid_y: f64,
    pub event_count: usize,
    pub last_seen_frame: u64,
    pub last_grown_frame: u64,
    pub frames_since_growth: usize,
    pub complete: bool,
    pub emitted: bool,
    pub cluster: EveCluster,
}
