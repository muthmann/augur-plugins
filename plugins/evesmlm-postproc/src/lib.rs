//! eveSMLM Post-Processing Plugin
//!
//! Filtering, drift correction, and rolling evaluation for EVE localization
//! streams produced by the fitting stage.

pub mod drift_correction;
pub mod evaluation;
pub mod filtering;

use std::collections::VecDeque;

use augur_plugin_api::{
    export_plugin, AnalysisSeverity, FfiSubpixelMarker, HostContext, HostOutput, Plugin,
    PluginFrame, PluginInput, SettingItem, SettingKind, SettingsSchema, SettingsSection,
    StatusEntry, CTX_LOCALIZATION_RESULTS,
};
pub use augur_plugin_evesmlm_fitting::{
    to_localization_results, EveLocalization, EveLocalizationResults, FitMethod,
    CTX_EVE_LOCALIZATION_RESULTS,
};
use evaluation::EvaluationState;
use serde_json::{json, Value};

const OVERLAY_COLOR: [u8; 4] = [90, 170, 255, 220];
const MAX_DRIFT_SHIFT_PX: i32 = 8;
const FITTING_DEPENDENCY: [&str; 1] = ["EVE Candidate Fitting"];
const MAX_FIT_RESIDUAL_SENTINEL: f64 = 1_000_000.0;

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
    fn process_localizations(
        &mut self,
        input: Option<&EveLocalizationResults>,
        output: &mut HostOutput<'_>,
    ) -> EveLocalizationResults {
        let Some(results) = input else {
            self.last_input_count = 0;
            self.last_output_count = 0;
            self.last_drift = (0.0, 0.0);
            self.last_status = "Waiting for EVE Candidate Fitting.".into();
            Self::warning(
                output,
                AnalysisSeverity::Info,
                "EVE post-processing requires EVE localization results.",
            );
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
            let markers: Vec<FfiSubpixelMarker> = corrected
                .localizations
                .iter()
                .map(|localization| FfiSubpixelMarker {
                    x: localization.x as f32,
                    y: localization.y as f32,
                })
                .collect();
            output.add_crosshair_markers(&markers, OVERLAY_COLOR, 4);
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

    fn current_enena_nm(&self) -> Option<f64> {
        self.evaluation.enena_sigma_nm(self.settings.nm_per_pixel)
    }

    fn max_fit_residual_value(&self) -> f64 {
        if self.settings.max_fit_residual.is_finite() {
            self.settings.max_fit_residual
        } else {
            MAX_FIT_RESIDUAL_SENTINEL
        }
    }

    fn parse_usize(value: Value) -> Option<usize> {
        value.as_u64().and_then(|value| usize::try_from(value).ok())
    }

    fn warning(output: &mut HostOutput<'_>, severity: AnalysisSeverity, message: &str) {
        output.add_warning("EVE Post-Processing", severity, message);
    }
}

impl Plugin for EveSmlmPostProcPlugin {
    fn name(&self) -> &'static str {
        "EVE Post-Processing"
    }

    fn description(&self) -> &'static str {
        "Filtering, drift correction, eNeNA precision, PSF accumulation, and on-time tracking."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    fn reset(&mut self) {
        EveSmlmPostProcPlugin::reset(self);
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::DerivedData
    }

    fn dependencies(&self) -> &[&'static str] {
        &FITTING_DEPENDENCY
    }

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
    ) {
        let input = match context.get::<EveLocalizationResults>(CTX_EVE_LOCALIZATION_RESULTS) {
            Ok(value) => value,
            Err(err) => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    &format!("Reading EVE localization results failed: {err}"),
                );
                None
            }
        };

        let corrected = self.process_localizations(input.as_ref(), output);
        if let Err(err) = context.publish(CTX_EVE_LOCALIZATION_RESULTS, &corrected) {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Publishing corrected EVE localizations failed: {err}"),
            );
        }

        let compatibility = to_localization_results(&corrected);
        if let Err(err) = context.publish(CTX_LOCALIZATION_RESULTS, &compatibility) {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Publishing localization compatibility results failed: {err}"),
            );
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Filtering".into(),
                    description: Some(
                        "Reject localizations that do not meet the configured event-count, polarity, or fit-quality thresholds."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "min_n_events".into(),
                            label: "Min events".into(),
                            tooltip: Some("Minimum number of events required for a localization to survive filtering.".into()),
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 100,
                                default: i64::try_from(self.settings.min_n_events).unwrap_or(3),
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "max_polarity_imbalance".into(),
                            label: "Max polarity imbalance".into(),
                            tooltip: Some("Maximum allowed absolute polarity balance after fitting.".into()),
                            kind: SettingKind::F64Slider {
                                min: 0.0,
                                max: 1.0,
                                default: self.settings.max_polarity_imbalance,
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "max_fit_residual".into(),
                            label: "Max fit residual".into(),
                            tooltip: Some("Set a very large value to effectively disable residual-based filtering.".into()),
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: MAX_FIT_RESIDUAL_SENTINEL,
                                speed: 0.1,
                                default: self.max_fit_residual_value(),
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Drift correction".into(),
                    description: Some(
                        "Register each frame against a rolling reference built from recent corrected localizations."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "drift_correction_enabled".into(),
                            label: "Enable drift correction".into(),
                            tooltip: Some("Estimate and subtract a per-frame x/y shift before publishing results.".into()),
                            kind: SettingKind::Bool {
                                default: self.settings.drift_correction_enabled,
                            },
                        },
                        SettingItem {
                            key: "drift_window_frames".into(),
                            label: "Drift window".into(),
                            tooltip: Some("Number of corrected frames retained in the rolling reference.".into()),
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 500,
                                default: i64::try_from(self.settings.drift_window_frames)
                                    .unwrap_or(50),
                                suffix: Some(" frames".into()),
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Evaluation".into(),
                    description: Some(
                        "Maintain rolling eNeNA, PSF, and on-time summaries for the corrected stream."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "show_enena".into(),
                            label: "Track eNeNA".into(),
                            tooltip: Some("Update the nearest-neighbor precision estimate over time.".into()),
                            kind: SettingKind::Bool {
                                default: self.settings.show_enena,
                            },
                        },
                        SettingItem {
                            key: "show_insitu_psf".into(),
                            label: "Track in situ PSF".into(),
                            tooltip: Some("Accumulate a rolling mean PSF patch from corrected localizations.".into()),
                            kind: SettingKind::Bool {
                                default: self.settings.show_insitu_psf,
                            },
                        },
                        SettingItem {
                            key: "show_on_time".into(),
                            label: "Track on-time".into(),
                            tooltip: Some("Maintain a greedy histogram of across-frame track lengths.".into()),
                            kind: SettingKind::Bool {
                                default: self.settings.show_on_time,
                            },
                        },
                        SettingItem {
                            key: "nm_per_pixel".into(),
                            label: "Scale".into(),
                            tooltip: Some("Pixel size used when converting eNeNA estimates into nanometers.".into()),
                            kind: SettingKind::F64Drag {
                                min: 1.0,
                                max: 500.0,
                                speed: 0.5,
                                default: self.settings.nm_per_pixel,
                            },
                        },
                        SettingItem {
                            key: "show_overlay".into(),
                            label: "Show overlay".into(),
                            tooltip: Some("Draw corrected localization markers on the preview.".into()),
                            kind: SettingKind::Bool {
                                default: self.settings.show_overlay,
                            },
                        },
                    ],
                },
            ],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "min_n_events" => Some(json!(self.settings.min_n_events)),
            "max_polarity_imbalance" => Some(json!(self.settings.max_polarity_imbalance)),
            "max_fit_residual" => Some(json!(self.max_fit_residual_value())),
            "drift_correction_enabled" => Some(json!(self.settings.drift_correction_enabled)),
            "drift_window_frames" => Some(json!(self.settings.drift_window_frames)),
            "show_enena" => Some(json!(self.settings.show_enena)),
            "show_insitu_psf" => Some(json!(self.settings.show_insitu_psf)),
            "show_on_time" => Some(json!(self.settings.show_on_time)),
            "nm_per_pixel" => Some(json!(self.settings.nm_per_pixel)),
            "show_overlay" => Some(json!(self.settings.show_overlay)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "min_n_events" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("min_n_events must be an integer".into());
                };
                self.settings.min_n_events = value.clamp(1, 100);
            }
            "max_polarity_imbalance" => {
                let Some(value) = value.as_f64() else {
                    return Err("max_polarity_imbalance must be numeric".into());
                };
                self.settings.max_polarity_imbalance = value.clamp(0.0, 1.0);
            }
            "max_fit_residual" => {
                let Some(value) = value.as_f64() else {
                    return Err("max_fit_residual must be numeric".into());
                };
                let value = value.clamp(0.0, MAX_FIT_RESIDUAL_SENTINEL);
                self.settings.max_fit_residual = if value >= MAX_FIT_RESIDUAL_SENTINEL * 0.999 {
                    f64::INFINITY
                } else {
                    value
                };
            }
            "drift_correction_enabled" => {
                let Some(value) = value.as_bool() else {
                    return Err("drift_correction_enabled must be a boolean".into());
                };
                self.settings.drift_correction_enabled = value;
            }
            "drift_window_frames" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("drift_window_frames must be an integer".into());
                };
                self.settings.drift_window_frames = value.clamp(1, 500);
            }
            "show_enena" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_enena must be a boolean".into());
                };
                self.settings.show_enena = value;
            }
            "show_insitu_psf" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_insitu_psf must be a boolean".into());
                };
                self.settings.show_insitu_psf = value;
            }
            "show_on_time" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_on_time must be a boolean".into());
                };
                self.settings.show_on_time = value;
            }
            "nm_per_pixel" => {
                let Some(value) = value.as_f64() else {
                    return Err("nm_per_pixel must be numeric".into());
                };
                self.settings.nm_per_pixel = value.clamp(1.0, 500.0);
            }
            "show_overlay" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_overlay must be a boolean".into());
                };
                self.settings.show_overlay = value;
            }
            _ => return Err(format!("unknown setting: {key}")),
        }

        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = vec![
            StatusEntry::Text(self.last_status.clone()),
            StatusEntry::LabeledValue {
                label: "Input".into(),
                value: self.last_input_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Output".into(),
                value: self.last_output_count.to_string(),
                color: None,
            },
        ];

        if self.settings.drift_correction_enabled {
            entries.push(StatusEntry::LabeledValue {
                label: "Drift".into(),
                value: format!("{:.2}, {:.2} px", self.last_drift.0, self.last_drift.1),
                color: None,
            });
        }

        if self.settings.show_enena {
            if let Some(precision_nm) = self.current_enena_nm() {
                entries.push(StatusEntry::LabeledValue {
                    label: "eNeNA".into(),
                    value: format!("{precision_nm:.1} nm"),
                    color: None,
                });
            }

            let start = self.evaluation.nn_distances_px.len().saturating_sub(32);
            let recent = self.evaluation.nn_distances_px[start..].to_vec();
            if !recent.is_empty() {
                entries.push(StatusEntry::Sparkline {
                    label: "NN distances".into(),
                    values: recent,
                    lower_is_better: true,
                });
            }
        }

        if self.settings.show_insitu_psf {
            if let Some((size, _)) = self.evaluation.mean_psf() {
                entries.push(StatusEntry::LabeledValue {
                    label: "PSF".into(),
                    value: format!("{size}x{size} accumulated"),
                    color: None,
                });
            }
        }

        if self.settings.show_on_time {
            let track_count: usize = self
                .evaluation
                .on_time_histogram()
                .iter()
                .map(|(_, count)| *count)
                .sum();
            if track_count > 0 {
                entries.push(StatusEntry::LabeledValue {
                    label: "Tracks".into(),
                    value: track_count.to_string(),
                    color: None,
                });
            }
        }

        entries
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

export_plugin!(EveSmlmPostProcPlugin);
