#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

#[derive(Debug, Clone)]
pub struct EveLocalization {
    pub x: f64,
    pub y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub timestamp_us: u64,
    pub n_events: usize,
    pub polarity_balance: f64,
    pub fit_residual: f64,
    pub fit_method: FitMethod,
}

#[derive(Debug, Clone, Default)]
pub struct EveLocalizationResults {
    pub localizations: Vec<EveLocalization>,
    pub frame_window_start_us: u64,
    pub frame_window_end_us: u64,
}
