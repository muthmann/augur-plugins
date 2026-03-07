use augur_plugin_api::FfiCdEvent;
use serde::{Deserialize, Serialize};

pub const CTX_EVE_CANDIDATES: &str = "augur.evesmlm.candidates";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateFindingMethod {
    #[default]
    Dbscan,
    Eigenfeature,
    FrameBased,
}

impl CandidateFindingMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dbscan => "DBSCAN",
            Self::Eigenfeature => "Eigenfeature",
            Self::FrameBased => "Frame-based",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EveEvent {
    pub timestamp: u64,
    pub x: u16,
    pub y: u16,
    pub polarity: bool,
}

impl From<FfiCdEvent> for EveEvent {
    fn from(value: FfiCdEvent) -> Self {
        Self {
            timestamp: value.timestamp,
            x: value.x,
            y: value.y,
            polarity: value.polarity != 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EveCluster {
    /// Per-pixel event histogram: (x, y, n_positive, n_negative)
    pub pixel_histogram: Vec<(u16, u16, u32, u32)>,
    /// All raw events assigned to this cluster.
    pub events: Vec<EveEvent>,
    /// Unweighted cluster centroid in pixel coordinates.
    pub centroid_x: f64,
    pub centroid_y: f64,
    /// Inclusive bounding box.
    pub x_min: u16,
    pub x_max: u16,
    pub y_min: u16,
    pub y_max: u16,
}

impl EveCluster {
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    pub fn positive_event_count(&self) -> usize {
        self.pixel_histogram
            .iter()
            .map(|(_, _, positive, _)| *positive as usize)
            .sum()
    }

    pub fn negative_event_count(&self) -> usize {
        self.pixel_histogram
            .iter()
            .map(|(_, _, _, negative)| *negative as usize)
            .sum()
    }

    pub fn polarity_balance(&self) -> f64 {
        let positive = self.positive_event_count() as f64;
        let negative = self.negative_event_count() as f64;
        let total = positive + negative;
        if total <= 0.0 {
            0.0
        } else {
            (positive - negative) / total
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EveCandidates {
    pub clusters: Vec<EveCluster>,
    pub frame_window_start_us: u64,
    pub frame_window_end_us: u64,
    pub n_events_processed: usize,
    pub finding_method: CandidateFindingMethod,
}
