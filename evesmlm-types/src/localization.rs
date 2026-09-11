use serde::{Deserialize, Serialize};

pub const CTX_EVE_LOCALIZATION_RESULTS: &str = "augur.evesmlm.localization_results";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionReason {
    FitFailed,
    SigmaOutOfBounds,
    ResidualTooHigh,
}

impl RejectionReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::FitFailed => "Fit failed",
            Self::SigmaOutOfBounds => "Sigma out of bounds",
            Self::ResidualTooHigh => "Residual too high",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::FitFailed => "fit_failed",
            Self::SigmaOutOfBounds => "sigma_out_of_bounds",
            Self::ResidualTooHigh => "residual_too_high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitMethod {
    #[default]
    LogGaussian,
    Gaussian,
    RadialSymmetry,
    Phasor,
    MeanXY,
}

impl FitMethod {
    pub fn label(self) -> &'static str {
        match self {
            Self::LogGaussian => "Log-Gaussian",
            Self::Gaussian => "Gaussian",
            Self::RadialSymmetry => "Radial Symmetry",
            Self::Phasor => "Phasor",
            Self::MeanXY => "Mean XY",
        }
    }

    pub fn produces_sigma(self) -> bool {
        matches!(self, Self::LogGaussian | Self::Gaussian)
    }

    pub fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Gaussian,
            2 => Self::RadialSymmetry,
            3 => Self::Phasor,
            4 => Self::MeanXY,
            _ => Self::LogGaussian,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::LogGaussian => 0,
            Self::Gaussian => 1,
            Self::RadialSymmetry => 2,
            Self::Phasor => 3,
            Self::MeanXY => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EveLocalization {
    pub cluster_id: u64,
    pub x: f64,
    pub y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub timestamp_us: u64,
    pub span_start_us: u64,
    pub span_end_us: u64,
    pub n_events: usize,
    pub polarity_balance: f64,
    pub fit_residual: f64,
    pub fit_method: FitMethod,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EveLocalizationResults {
    pub localizations: Vec<EveLocalization>,
    pub frame_window_start_us: u64,
    pub frame_window_end_us: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedFitRow {
    pub row_id: u64,
    pub cluster_id: u64,
    pub x: f64,
    pub y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub fit_residual: f64,
    pub n_events: u64,
    pub polarity_balance: f64,
    pub rejection_reason: RejectionReason,
    pub timestamp_us: u64,
    pub span_start_us: u64,
    pub span_end_us: u64,
}
