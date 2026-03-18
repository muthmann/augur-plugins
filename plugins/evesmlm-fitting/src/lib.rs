//! eveSMLM Candidate Fitting Plugin
//!
//! Consumes `EveCandidates` and localizes each raw-event cluster to
//! sub-pixel precision with a configurable fitting backend.

pub mod gaussian;
pub mod log_gaussian;
pub mod mean_xy;
pub mod phasor;
pub mod radial_symmetry;
pub mod types;

use augur_plugin_api::{
    export_plugin, AnalysisSeverity, EventStoreHandle, FfiSubpixelMarker, HostContext, HostOutput,
    Localization, LocalizationResults, Plugin, PluginFrame, PluginInput, SettingItem, SettingKind,
    SettingsSchema, SettingsSection, StatusEntry, CTX_LOCALIZATION_RESULTS,
};
use augur_plugin_api::{
    HostDatasetDescriptor, HostDatasetKind, HostViewDescriptor, HostViewKind, HostViewPlacement,
    HostViewRegistry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1, TableSchema,
    TableValueType,
};
pub use augur_plugin_evesmlm_candidates::{
    CandidateFindingMethod, EveCandidates, EveCluster, EveEvent, CTX_EVE_CANDIDATES,
};
use serde_json::{json, Value};
pub use types::{EveLocalization, EveLocalizationResults, FitMethod, CTX_EVE_LOCALIZATION_RESULTS};

const OVERLAY_COLOR: [u8; 4] = [60, 220, 140, 220];
const CANDIDATE_DEPENDENCY: [&str; 1] = ["EVE Candidate Finding"];
pub const CURRENT_LOCALIZATIONS_DATASET_ID: &str = "augur.evesmlm.current_localizations";
pub const CURRENT_LOCALIZATIONS_VIEW_ID: &str = "augur.evesmlm.current_localizations.compact";

pub fn current_localizations_registry() -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: CURRENT_LOCALIZATIONS_DATASET_ID.into(),
            title: "Current EVE localizations".into(),
            kind: HostDatasetKind::TableV1(current_localizations_schema()),
            empty_message: "No EVE localizations in the current frame.".into(),
        }],
        views: vec![HostViewDescriptor {
            id: CURRENT_LOCALIZATIONS_VIEW_ID.into(),
            title: "Current Localizations".into(),
            dataset_id: CURRENT_LOCALIZATIONS_DATASET_ID.into(),
            placement: HostViewPlacement::AnalysisPanel,
            kind: HostViewKind::CompactTable,
        }],
    }
}

pub fn current_localizations_schema() -> TableSchema {
    TableSchema {
        columns: vec![
            TableColumn {
                id: "x_px".into(),
                title: "X (px)".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "y_px".into(),
                title: "Y (px)".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "sigma_x_px".into(),
                title: "Sigma X (px)".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "sigma_y_px".into(),
                title: "Sigma Y (px)".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "n_events".into(),
                title: "Events".into(),
                value_type: TableValueType::U64,
            },
        ],
        coordinate_space_2d: None,
    }
}

pub fn current_localizations_dataset(results: &EveLocalizationResults) -> TableDatasetV1 {
    TableDatasetV1::new(vec![
        TableColumnData {
            column_id: "x_px".into(),
            values: TableColumnValues::F64(
                results.localizations.iter().map(|value| value.x).collect(),
            ),
        },
        TableColumnData {
            column_id: "y_px".into(),
            values: TableColumnValues::F64(
                results.localizations.iter().map(|value| value.y).collect(),
            ),
        },
        TableColumnData {
            column_id: "sigma_x_px".into(),
            values: TableColumnValues::F64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.sigma_x)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "sigma_y_px".into(),
            values: TableColumnValues::F64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.sigma_y)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "n_events".into(),
            values: TableColumnValues::U64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.n_events as u64)
                    .collect(),
            ),
        },
    ])
    .expect("current localization columns should stay aligned")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FitEstimate {
    pub x: f64,
    pub y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub residual: f64,
}

#[derive(Debug, Clone)]
pub struct FittingSettings {
    pub fit_method: FitMethod,
    pub nm_per_pixel: f64,
    pub sigma_min_nm: f64,
    pub sigma_max_nm: f64,
    pub max_fit_residual: f64,
    pub show_overlay: bool,
}

impl Default for FittingSettings {
    fn default() -> Self {
        Self {
            fit_method: FitMethod::LogGaussian,
            nm_per_pixel: 65.0,
            sigma_min_nm: 80.0,
            sigma_max_nm: 200.0,
            max_fit_residual: 0.5,
            show_overlay: true,
        }
    }
}

pub struct EveSmlmFittingPlugin {
    enabled: bool,
    settings: FittingSettings,
    current_results: EveLocalizationResults,
    last_localization_count: usize,
    last_rejection_count: usize,
    last_status: String,
}

impl Default for EveSmlmFittingPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: FittingSettings::default(),
            current_results: EveLocalizationResults::default(),
            last_localization_count: 0,
            last_rejection_count: 0,
            last_status:
                "Enable the plugin to fit EVE candidate clusters to sub-pixel localizations.".into(),
        }
    }
}

impl EveSmlmFittingPlugin {
    fn analyze_candidates(
        &mut self,
        candidates: Option<&EveCandidates>,
        output: &mut HostOutput<'_>,
    ) -> (EveLocalizationResults, LocalizationResults) {
        let Some(candidates) = candidates else {
            self.last_localization_count = 0;
            self.last_rejection_count = 0;
            self.last_status = "Waiting for EVE Candidate Finding.".into();
            Self::warning(
                output,
                AnalysisSeverity::Info,
                "EVE fitting requires candidate clusters from EVE Candidate Finding.",
            );
            return (
                EveLocalizationResults::default(),
                LocalizationResults::default(),
            );
        };

        let mut localizations = Vec::new();
        let mut rejected = 0;
        for cluster in &candidates.clusters {
            let Some(fit) = fit_cluster(cluster, self.settings.fit_method) else {
                rejected += 1;
                continue;
            };

            if self.settings.fit_method.produces_sigma() {
                let sigma_x_nm = fit.sigma_x * self.settings.nm_per_pixel;
                let sigma_y_nm = fit.sigma_y * self.settings.nm_per_pixel;
                if sigma_x_nm < self.settings.sigma_min_nm
                    || sigma_x_nm > self.settings.sigma_max_nm
                    || sigma_y_nm < self.settings.sigma_min_nm
                    || sigma_y_nm > self.settings.sigma_max_nm
                {
                    rejected += 1;
                    continue;
                }
            }

            if fit.residual > self.settings.max_fit_residual {
                rejected += 1;
                continue;
            }

            localizations.push(EveLocalization {
                x: fit.x,
                y: fit.y,
                sigma_x: fit.sigma_x,
                sigma_y: fit.sigma_y,
                timestamp_us: estimate_timestamp_us(
                    &cluster.events,
                    fit.x,
                    fit.y,
                    fit_radius(cluster, &fit),
                ),
                n_events: cluster.event_count(),
                polarity_balance: cluster.polarity_balance(),
                fit_residual: fit.residual,
                fit_method: self.settings.fit_method,
            });
        }

        self.last_localization_count = localizations.len();
        self.last_rejection_count = rejected;
        self.last_status = format!(
            "{} localizations accepted, {} rejected with {}.",
            self.last_localization_count,
            self.last_rejection_count,
            self.settings.fit_method.label()
        );

        if self.settings.show_overlay && !localizations.is_empty() {
            let markers: Vec<FfiSubpixelMarker> = localizations
                .iter()
                .map(|localization| FfiSubpixelMarker {
                    x: localization.x as f32,
                    y: localization.y as f32,
                })
                .collect();
            output.add_crosshair_markers(&markers, OVERLAY_COLOR, 5);
        }

        let eve_results = EveLocalizationResults {
            localizations,
            frame_window_start_us: candidates.frame_window_start_us,
            frame_window_end_us: candidates.frame_window_end_us,
        };
        let compatibility_results = to_localization_results(&eve_results);

        (eve_results, compatibility_results)
    }

    pub fn reset(&mut self) {
        self.current_results = EveLocalizationResults::default();
        self.last_localization_count = 0;
        self.last_rejection_count = 0;
        self.last_status = "Waiting for the next candidate set.".into();
    }

    fn parse_usize(value: Value) -> Option<usize> {
        value.as_u64().and_then(|value| usize::try_from(value).ok())
    }

    fn warning(output: &mut HostOutput<'_>, severity: AnalysisSeverity, message: &str) {
        output.add_warning("EVE Candidate Fitting", severity, message);
    }
}

impl Plugin for EveSmlmFittingPlugin {
    fn name(&self) -> &'static str {
        "EVE Candidate Fitting"
    }

    fn description(&self) -> &'static str {
        "Sub-pixel localization of raw-event candidates with multiple fitting backends."
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
        EveSmlmFittingPlugin::reset(self);
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::DerivedData
    }

    fn dependencies(&self) -> &[&'static str] {
        &CANDIDATE_DEPENDENCY
    }

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        let candidates = match context.get::<EveCandidates>(CTX_EVE_CANDIDATES) {
            Ok(value) => value,
            Err(err) => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    &format!("Reading EVE candidates failed: {err}"),
                );
                None
            }
        };

        let (eve_results, compatibility) = self.analyze_candidates(candidates.as_ref(), output);
        self.current_results = eve_results.clone();
        if let Err(err) = context.publish(CTX_EVE_LOCALIZATION_RESULTS, &eve_results) {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Publishing EVE localizations failed: {err}"),
            );
        }
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
            sections: vec![SettingsSection {
                label: "Fitting".into(),
                description: Some(
                    "Estimate sub-pixel emitter positions directly from event-cluster histograms."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "fit_method".into(),
                        label: "Method".into(),
                        tooltip: Some(
                            "Choose the numerical backend used to fit each candidate cluster."
                                .into(),
                        ),
                        kind: SettingKind::Enum {
                            variants: vec![
                                FitMethod::LogGaussian.label().into(),
                                FitMethod::Gaussian.label().into(),
                                FitMethod::RadialSymmetry.label().into(),
                                FitMethod::Phasor.label().into(),
                                FitMethod::MeanXY.label().into(),
                            ],
                            default: self.settings.fit_method.index(),
                        },
                    },
                    SettingItem {
                        key: "nm_per_pixel".into(),
                        label: "Scale".into(),
                        tooltip: Some(
                            "Pixel size used when converting fitted sigmas into nanometers.".into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 1.0,
                            max: 500.0,
                            speed: 0.5,
                            default: self.settings.nm_per_pixel,
                        },
                    },
                    SettingItem {
                        key: "sigma_min_nm".into(),
                        label: "Sigma min".into(),
                        tooltip: Some(
                            "Lower accepted sigma bound for methods that estimate PSF width."
                                .into(),
                        ),
                        kind: SettingKind::F64Slider {
                            min: 10.0,
                            max: 500.0,
                            default: self.settings.sigma_min_nm,
                            suffix: Some(" nm".into()),
                        },
                    },
                    SettingItem {
                        key: "sigma_max_nm".into(),
                        label: "Sigma max".into(),
                        tooltip: Some(
                            "Upper accepted sigma bound for methods that estimate PSF width."
                                .into(),
                        ),
                        kind: SettingKind::F64Slider {
                            min: 10.0,
                            max: 500.0,
                            default: self.settings.sigma_max_nm,
                            suffix: Some(" nm".into()),
                        },
                    },
                    SettingItem {
                        key: "max_fit_residual".into(),
                        label: "Max residual".into(),
                        tooltip: Some(
                            "Reject localizations whose fit residual exceeds this threshold."
                                .into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 0.0,
                            max: 10.0,
                            speed: 0.01,
                            default: self.settings.max_fit_residual,
                        },
                    },
                    SettingItem {
                        key: "show_overlay".into(),
                        label: "Show overlay".into(),
                        tooltip: Some(
                            "Draw crosshair markers at accepted localization positions.".into(),
                        ),
                        kind: SettingKind::Bool {
                            default: self.settings.show_overlay,
                        },
                    },
                ],
            }],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "fit_method" => Some(json!(self.settings.fit_method.index())),
            "nm_per_pixel" => Some(json!(self.settings.nm_per_pixel)),
            "sigma_min_nm" => Some(json!(self.settings.sigma_min_nm)),
            "sigma_max_nm" => Some(json!(self.settings.sigma_max_nm)),
            "max_fit_residual" => Some(json!(self.settings.max_fit_residual)),
            "show_overlay" => Some(json!(self.settings.show_overlay)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "fit_method" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("fit_method must be an integer".into());
                };
                self.settings.fit_method = FitMethod::from_index(value);
            }
            "nm_per_pixel" => {
                let Some(value) = value.as_f64() else {
                    return Err("nm_per_pixel must be numeric".into());
                };
                self.settings.nm_per_pixel = value.clamp(1.0, 500.0);
            }
            "sigma_min_nm" => {
                let Some(value) = value.as_f64() else {
                    return Err("sigma_min_nm must be numeric".into());
                };
                self.settings.sigma_min_nm = value.clamp(10.0, 500.0);
                self.settings.sigma_max_nm =
                    self.settings.sigma_max_nm.max(self.settings.sigma_min_nm);
            }
            "sigma_max_nm" => {
                let Some(value) = value.as_f64() else {
                    return Err("sigma_max_nm must be numeric".into());
                };
                self.settings.sigma_max_nm = value.clamp(10.0, 500.0);
                self.settings.sigma_min_nm =
                    self.settings.sigma_min_nm.min(self.settings.sigma_max_nm);
            }
            "max_fit_residual" => {
                let Some(value) = value.as_f64() else {
                    return Err("max_fit_residual must be numeric".into());
                };
                self.settings.max_fit_residual = value.clamp(0.0, 10.0);
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
        vec![
            StatusEntry::Text(self.last_status.clone()),
            StatusEntry::LabeledValue {
                label: "Accepted".into(),
                value: self.last_localization_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Rejected".into(),
                value: self.last_rejection_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Method".into(),
                value: self.settings.fit_method.label().into(),
                color: None,
            },
        ]
    }

    fn host_views(&self) -> HostViewRegistry {
        current_localizations_registry()
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        if dataset_id != CURRENT_LOCALIZATIONS_DATASET_ID {
            return None;
        }

        serde_json::to_vec(&current_localizations_dataset(&self.current_results)).ok()
    }
}

fn fit_cluster(cluster: &EveCluster, method: FitMethod) -> Option<FitEstimate> {
    match method {
        FitMethod::LogGaussian => log_gaussian::fit(cluster),
        FitMethod::Gaussian => gaussian::fit(cluster),
        FitMethod::RadialSymmetry => radial_symmetry::fit(cluster),
        FitMethod::Phasor => phasor::fit(cluster),
        FitMethod::MeanXY => mean_xy::fit(cluster),
    }
}

fn fit_radius(cluster: &EveCluster, fit: &FitEstimate) -> f64 {
    if fit.sigma_x > 0.0 && fit.sigma_y > 0.0 {
        2.5 * fit.sigma_x.max(fit.sigma_y).max(1.0)
    } else {
        let dx = f64::from(cluster.x_max.saturating_sub(cluster.x_min)) + 1.0;
        let dy = f64::from(cluster.y_max.saturating_sub(cluster.y_min)) + 1.0;
        0.5 * dx.max(dy).max(1.0)
    }
}

fn estimate_timestamp_us(events: &[EveEvent], x: f64, y: f64, radius: f64) -> u64 {
    if events.is_empty() {
        return 0;
    }

    let radius2 = radius * radius;
    let mut weighted_timestamp = 0.0;
    let mut weight_sum = 0.0;
    for event in events {
        let dx = f64::from(event.x) - x;
        let dy = f64::from(event.y) - y;
        let dist2 = dx * dx + dy * dy;
        if dist2 > radius2 {
            continue;
        }
        let weight = 1.0 / (1.0 + dist2);
        weighted_timestamp += event.timestamp as f64 * weight;
        weight_sum += weight;
    }

    if weight_sum <= 0.0 {
        let mean_timestamp = events
            .iter()
            .map(|event| event.timestamp as f64)
            .sum::<f64>()
            / events.len() as f64;
        mean_timestamp.round() as u64
    } else {
        (weighted_timestamp / weight_sum).round() as u64
    }
}

pub fn to_localization_results(results: &EveLocalizationResults) -> LocalizationResults {
    LocalizationResults {
        localizations: results
            .localizations
            .iter()
            .map(|localization| Localization {
                x: localization.x,
                y: localization.y,
                sigma_x: localization.sigma_x,
                sigma_y: localization.sigma_y,
                amplitude: 0.0,
                background: 0.0,
                timestamp_us: localization.timestamp_us,
                fit_error: localization.fit_residual,
            })
            .collect(),
        frame_window_start_us: results.frame_window_start_us,
        frame_window_end_us: results.frame_window_end_us,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(x: u16, y: u16, polarity: bool, timestamp: u64) -> EveEvent {
        EveEvent {
            x,
            y,
            timestamp,
            polarity,
        }
    }

    fn cluster_from_histogram(entries: &[(u16, u16, u32)]) -> EveCluster {
        let mut pixel_histogram = Vec::new();
        let mut events = Vec::new();
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        let mut total = 0.0;
        let mut x_min = u16::MAX;
        let mut x_max = 0;
        let mut y_min = u16::MAX;
        let mut y_max = 0;
        let mut timestamp = 1;

        for (x, y, count) in entries {
            pixel_histogram.push((*x, *y, *count, 0));
            x_min = x_min.min(*x);
            x_max = x_max.max(*x);
            y_min = y_min.min(*y);
            y_max = y_max.max(*y);
            sum_x += f64::from(*x) * f64::from(*count);
            sum_y += f64::from(*y) * f64::from(*count);
            total += f64::from(*count);
            for _ in 0..*count {
                events.push(event(*x, *y, true, timestamp));
                timestamp += 1;
            }
        }

        EveCluster {
            pixel_histogram,
            events,
            centroid_x: if total > 0.0 { sum_x / total } else { 0.0 },
            centroid_y: if total > 0.0 { sum_y / total } else { 0.0 },
            x_min,
            x_max,
            y_min,
            y_max,
        }
    }

    fn gaussian_entries(
        size: (u16, u16),
        center: (f64, f64),
        sigma: (f64, f64),
        amplitude: f64,
        background: f64,
    ) -> Vec<(u16, u16, u32)> {
        let (width, height) = size;
        let (x0, y0) = center;
        let (sigma_x, sigma_y) = sigma;
        let mut entries = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let dx = f64::from(x) - x0;
                let dy = f64::from(y) - y0;
                let value = background
                    + amplitude
                        * (-0.5 * (dx * dx / sigma_x.powi(2) + dy * dy / sigma_y.powi(2))).exp();
                let count = value.round().max(0.0) as u32;
                if count > 0 {
                    entries.push((x, y, count));
                }
            }
        }
        entries
    }

    #[test]
    fn log_gaussian_recovers_synthetic_parabola() {
        let x0: f64 = 6.3;
        let y0: f64 = 5.7;
        let sigma_x: f64 = 1.4;
        let sigma_y: f64 = 1.8;
        let mut entries = Vec::new();
        for y in 1..11 {
            for x in 1..11 {
                let dx = f64::from(x) - x0;
                let dy = f64::from(y) - y0;
                let value =
                    40.0 - dx * dx / (2.0 * sigma_x.powi(2)) - dy * dy / (2.0 * sigma_y.powi(2));
                let count = value.round().max(0.0) as u32;
                if count > 0 {
                    entries.push((x, y, count));
                }
            }
        }

        let fit = log_gaussian::fit(&cluster_from_histogram(&entries)).unwrap();
        assert!((fit.x - x0).abs() <= 0.3);
        assert!((fit.y - y0).abs() <= 0.3);
        assert!((fit.sigma_x - sigma_x).abs() <= 0.3);
        assert!((fit.sigma_y - sigma_y).abs() <= 0.3);
    }

    #[test]
    fn gaussian_recovers_synthetic_center() {
        let x0 = 6.2;
        let y0 = 7.4;
        let entries = gaussian_entries((14, 14), (x0, y0), (1.5, 1.8), 45.0, 2.0);
        let fit = gaussian::fit(&cluster_from_histogram(&entries)).unwrap();
        assert!((fit.x - x0).abs() <= 0.2);
        assert!((fit.y - y0).abs() <= 0.2);
    }

    #[test]
    fn radial_symmetry_recovers_synthetic_center() {
        let x0 = 6.1;
        let y0 = 5.9;
        let entries = gaussian_entries((14, 14), (x0, y0), (1.4, 1.6), 50.0, 1.0);
        let fit = radial_symmetry::fit(&cluster_from_histogram(&entries)).unwrap();
        assert!((fit.x - x0).abs() <= 0.3);
        assert!((fit.y - y0).abs() <= 0.3);
    }

    #[test]
    fn phasor_recovers_synthetic_center() {
        let x0 = 5.6;
        let y0 = 7.1;
        let entries = gaussian_entries((12, 12), (x0, y0), (1.3, 1.3), 60.0, 1.0);
        let fit = phasor::fit(&cluster_from_histogram(&entries)).unwrap();
        assert!((fit.x - x0).abs() <= 0.5);
        assert!((fit.y - y0).abs() <= 0.5);
    }

    #[test]
    fn mean_xy_matches_weighted_centroid() {
        let cluster = cluster_from_histogram(&[(10, 10, 5), (11, 10, 5), (10, 11, 5), (11, 11, 5)]);
        let fit = mean_xy::fit(&cluster).unwrap();
        assert!((fit.x - 10.5).abs() <= 0.1);
        assert!((fit.y - 10.5).abs() <= 0.1);
    }

    #[test]
    fn empty_cluster_returns_none() {
        let cluster = EveCluster {
            pixel_histogram: Vec::new(),
            events: Vec::new(),
            centroid_x: 0.0,
            centroid_y: 0.0,
            x_min: 0,
            x_max: 0,
            y_min: 0,
            y_max: 0,
        };

        assert!(mean_xy::fit(&cluster).is_none());
        assert!(log_gaussian::fit(&cluster).is_none());
        assert!(gaussian::fit(&cluster).is_none());
    }

    #[test]
    fn host_view_registry_exposes_one_dataset_and_panel_view() {
        let registry = current_localizations_registry();

        assert_eq!(registry.datasets.len(), 1);
        assert_eq!(registry.views.len(), 1);
        assert_eq!(registry.datasets[0].id, CURRENT_LOCALIZATIONS_DATASET_ID);
        assert_eq!(registry.views[0].id, CURRENT_LOCALIZATIONS_VIEW_ID);
    }

    #[test]
    fn host_view_dataset_is_columnar_and_aligned() {
        let dataset = current_localizations_dataset(&EveLocalizationResults {
            localizations: vec![EveLocalization {
                x: 1.5,
                y: 2.5,
                sigma_x: 0.7,
                sigma_y: 0.8,
                timestamp_us: 10,
                n_events: 7,
                polarity_balance: 0.1,
                fit_residual: 0.02,
                fit_method: FitMethod::LogGaussian,
            }],
            frame_window_start_us: 0,
            frame_window_end_us: 50,
        });

        assert_eq!(dataset.row_count(), 1);
        assert_eq!(dataset.columns.len(), 5);
        assert_eq!(dataset.columns[0].column_id, "x_px");
        assert_eq!(dataset.columns[4].column_id, "n_events");
    }
}

export_plugin!(EveSmlmFittingPlugin);
