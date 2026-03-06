//! eveSMLM Post-Processing Plugin
//!
//! Filtering, drift correction, and rolling evaluation for EVE localization
//! streams produced by the fitting stage.

pub mod drift_correction;
pub mod evaluation;
pub mod filtering;

use std::collections::VecDeque;

use augur_core::analysis::{AnalysisOutput, AnalysisSeverity, AnalysisWarning, Overlay, Pixel};
pub use augur_plugin_evesmlm_fitting::{EveLocalization, EveLocalizationResults, FitMethod};

use evaluation::EvaluationState;

const OVERLAY_COLOR: [u8; 4] = [90, 170, 255, 220];
const MAX_DRIFT_SHIFT_PX: i32 = 8;

#[derive(Debug, Clone)]
pub struct PostProcSettings {
    pub min_n_events: usize,
    pub max_polarity_imbalance: f64,
    pub max_fit_residual: f64,
    pub drift_correction_enabled: bool,
    pub drift_window_frames: usize,
    pub show_enena: bool,
    pub show_insitu_psf: bool,
    pub show_on_time: bool,
    pub nm_per_pixel: f64,
    pub show_overlay: bool,
}

impl Default for PostProcSettings {
    fn default() -> Self {
        Self {
            min_n_events: 3,
            max_polarity_imbalance: 1.0,
            max_fit_residual: f64::INFINITY,
            drift_correction_enabled: false,
            drift_window_frames: 50,
            show_enena: true,
            show_insitu_psf: true,
            show_on_time: true,
            nm_per_pixel: 65.0,
            show_overlay: true,
        }
    }
}

pub struct EveSmlmPostProcPlugin {
    enabled: bool,
    settings: PostProcSettings,
    corrected_history: VecDeque<Vec<(f64, f64)>>,
    evaluation: EvaluationState,
    last_input_count: usize,
    last_output_count: usize,
    last_drift: (f64, f64),
    last_status: String,
}

impl Default for EveSmlmPostProcPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: PostProcSettings::default(),
            corrected_history: VecDeque::new(),
            evaluation: EvaluationState::default(),
            last_input_count: 0,
            last_output_count: 0,
            last_drift: (0.0, 0.0),
            last_status: "Enable the plugin to filter and evaluate EVE localization streams."
                .into(),
        }
    }
}

impl EveSmlmPostProcPlugin {
    pub fn name(&self) -> &str {
        "EVE Post-Processing"
    }

    pub fn description(&self) -> &str {
        "Filtering, drift correction, eNeNA precision, PSF accumulation, and on-time tracking."
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    pub fn settings(&self) -> &PostProcSettings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut PostProcSettings {
        &mut self.settings
    }

    pub fn last_status(&self) -> &str {
        &self.last_status
    }

    pub fn last_drift(&self) -> (f64, f64) {
        self.last_drift
    }

    pub fn current_enena_nm(&self) -> Option<f64> {
        self.evaluation.enena_sigma_nm(self.settings.nm_per_pixel)
    }

    pub fn evaluation(&self) -> &EvaluationState {
        &self.evaluation
    }

    pub fn process_localizations(
        &mut self,
        input: Option<&EveLocalizationResults>,
        output: &mut AnalysisOutput,
    ) -> EveLocalizationResults {
        let Some(results) = input else {
            self.last_input_count = 0;
            self.last_output_count = 0;
            self.last_drift = (0.0, 0.0);
            self.last_status = "Waiting for EVE Candidate Fitting.".into();
            output.warnings.push(AnalysisWarning {
                source: self.name().to_owned(),
                severity: AnalysisSeverity::Info,
                message: "EVE post-processing requires EVE localization results.".into(),
            });
            return EveLocalizationResults::default();
        };

        self.last_input_count = results.localizations.len();
        let filtered = filtering::filter_results(
            results,
            self.settings.min_n_events,
            self.settings.max_polarity_imbalance,
            self.settings.max_fit_residual,
        );

        let correction = if self.settings.drift_correction_enabled {
            let reference_points: Vec<(f64, f64)> = self
                .corrected_history
                .iter()
                .flat_map(|window| window.iter().copied())
                .collect();
            let moving_points: Vec<(f64, f64)> = filtered
                .localizations
                .iter()
                .map(|localization| (localization.x, localization.y))
                .collect();
            drift_correction::estimate_correction_shift(
                &reference_points,
                &moving_points,
                MAX_DRIFT_SHIFT_PX,
            )
        } else {
            (0.0, 0.0)
        };

        let corrected = if self.settings.drift_correction_enabled {
            drift_correction::apply_correction(&filtered, correction.0, correction.1)
        } else {
            filtered
        };

        self.last_output_count = corrected.localizations.len();
        self.last_drift = correction;
        self.push_history(&corrected);
        self.evaluation.update(&corrected);

        if self.settings.show_overlay && !corrected.localizations.is_empty() {
            output.overlays.push(Overlay::HighlightPixels {
                pixels: corrected
                    .localizations
                    .iter()
                    .map(|localization| Pixel {
                        x: localization.x.round().max(0.0) as u16,
                        y: localization.y.round().max(0.0) as u16,
                    })
                    .collect(),
                color: OVERLAY_COLOR,
            });
        }

        let mut status = format!(
            "{} in, {} out",
            self.last_input_count, self.last_output_count
        );
        if self.settings.drift_correction_enabled {
            status.push_str(&format!(
                ", drift ({:.2}, {:.2}) px",
                self.last_drift.0, self.last_drift.1
            ));
        }
        if self.settings.show_enena {
            if let Some(precision_nm) = self.current_enena_nm() {
                status.push_str(&format!(", eNeNA {:.1} nm", precision_nm));
            }
        }
        self.last_status = status;

        corrected
    }

    pub fn reset(&mut self) {
        self.corrected_history.clear();
        self.evaluation.reset();
        self.last_input_count = 0;
        self.last_output_count = 0;
        self.last_drift = (0.0, 0.0);
        self.last_status = "Waiting for the next localization batch.".into();
    }

    fn push_history(&mut self, corrected: &EveLocalizationResults) {
        self.corrected_history.push_back(
            corrected
                .localizations
                .iter()
                .map(|localization| (localization.x, localization.y))
                .collect(),
        );
        let max_len = self.settings.drift_window_frames.max(1);
        while self.corrected_history.len() > max_len {
            self.corrected_history.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn localization(x: f64, y: f64, n_events: usize) -> EveLocalization {
        EveLocalization {
            x,
            y,
            sigma_x: 1.2,
            sigma_y: 1.2,
            timestamp_us: 0,
            n_events,
            polarity_balance: 0.0,
            fit_residual: 0.1,
            fit_method: FitMethod::LogGaussian,
        }
    }

    fn results(localizations: Vec<EveLocalization>) -> EveLocalizationResults {
        EveLocalizationResults {
            localizations,
            frame_window_start_us: 0,
            frame_window_end_us: 1_000,
        }
    }

    #[test]
    fn filtering_removes_localizations_below_event_threshold() {
        let filtered = filtering::filter_results(
            &results(vec![localization(1.0, 1.0, 2), localization(2.0, 2.0, 5)]),
            3,
            1.0,
            f64::INFINITY,
        );

        assert_eq!(filtered.localizations.len(), 1);
        assert_eq!(filtered.localizations[0].n_events, 5);
    }

    #[test]
    fn drift_correction_identity_returns_zero_shift() {
        let reference = vec![(1.0, 1.0), (3.0, 3.0), (6.0, 2.0)];
        let correction = drift_correction::estimate_correction_shift(&reference, &reference, 4);
        assert!(correction.0.abs() <= 0.1);
        assert!(correction.1.abs() <= 0.1);
    }

    #[test]
    fn enena_accumulation_collects_expected_nearest_neighbor_distances() {
        let mut evaluation = EvaluationState::default();
        evaluation.update(&results(vec![
            localization(0.0, 0.0, 5),
            localization(3.0, 0.0, 5),
            localization(10.0, 0.0, 5),
        ]));

        let mut distances = evaluation.nn_distances_px.clone();
        distances.sort_by(|left, right| left.total_cmp(right));
        assert_eq!(distances, vec![3.0, 3.0, 7.0]);
    }
}
