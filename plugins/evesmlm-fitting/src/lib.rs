//! eveSMLM Candidate Fitting Plugin
//!
//! Consumes `EveCandidates` and localizes each raw-event cluster to
//! sub-pixel precision with a configurable fitting backend.

use std::collections::HashMap;

pub mod gaussian;
pub mod log_gaussian;
pub mod mean_xy;
pub mod phasor;
pub mod radial_symmetry;

use augur_plugin_api::{
    export_plugin, AnalysisSeverity, EventStoreHandle, FfiColorRgba, FfiMarkerOverlayItem,
    FfiMarkerShape, GlobalSettings, HostActionDescriptor, HostActionRequestQueue, HostActionScope,
    HostContext, HostOutput, Plugin, PluginFrame, PluginInput, SettingItem, SettingKind,
    SettingsSchema, SettingsSection, StatusEntry, CTX_GLOBAL_SETTINGS,
    CTX_INVESTIGATION_ACTION_REQUESTS, HOST_ACTION_CLUSTER_ROWS_PARAM,
};
use augur_plugin_api::{
    HostDatasetDescriptor, HostDatasetDisplayMetadata, HostDatasetKind, HostDatasetRelation,
    HostMarkerShape, HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry,
    TableColumn, TableColumnData, TableColumnDisplayEntry, TableColumnDisplayFormat,
    TableColumnDisplayMetadata, TableColumnValues, TableCoordinateSpace2d, TableCoordinateSpace3d,
    TableDatasetV1, TableRowProvenance, TableSchema, TableValueType,
};
use augur_plugin_types::{Localization, LocalizationResults, CTX_LOCALIZATION_RESULTS};
pub use evesmlm_types::{
    current_localizations_dataset, current_localizations_registry,
    current_localizations_registry_for_results, current_localizations_schema,
    current_localizations_schema_for_results, localization_row_id, localization_time_bounds,
    localization_xy_bounds, to_localization_results, EveCandidates, EveCluster, EveEvent,
    EveLocalization, EveLocalizationResults, FitMethod, RejectedFitRow, RejectionReason,
    ACCEPTED_CANDIDATE_EVENTS_DATASET_ID, CTX_EVE_CANDIDATES, CTX_EVE_LOCALIZATION_RESULTS,
    CURRENT_LOCALIZATIONS_3D_VIEW_ID, CURRENT_LOCALIZATIONS_DATASET_ID,
    CURRENT_LOCALIZATIONS_LAYER_ID, CURRENT_LOCALIZATIONS_VIEW_ID,
};
use serde_json::{json, Value};

const OVERLAY_COLOR: [u8; 4] = [60, 220, 140, 220];
const CANDIDATE_DEPENDENCY: [&str; 1] = ["EVE Candidate Finding"];
pub const REJECTED_FITS_DATASET_ID: &str = "augur.evesmlm.rejected_fits";
pub const REJECTED_FITS_LAYER_ID: &str = "augur.layer.evesmlm.rejected_fits";
pub const REJECTED_FITS_COMPACT_VIEW_ID: &str = "augur.evesmlm.rejected_fits.compact";
pub const REJECTED_FITS_TABLE_VIEW_ID: &str = "augur.evesmlm.rejected_fits.table";
pub const REJECTED_FITS_3D_VIEW_ID: &str = "augur.evesmlm.rejected_fits.scatter3d";

pub const REFIT_PREVIEW_DATASET_ID: &str = "augur.evesmlm.refit_preview";
pub const REFIT_PREVIEW_LAYER_ID: &str = "augur.layer.evesmlm.refit_preview";
pub const REFIT_PREVIEW_VIEW_ID: &str = "augur.evesmlm.refit_preview.compact";

pub const ACTION_REFIT_CLUSTER: &str = "augur.evesmlm.refit_cluster";
pub const ACTION_COMMIT_REFIT: &str = "augur.evesmlm.commit_refit";
pub const ACTION_DISCARD_REFIT: &str = "augur.evesmlm.discard_refit";

pub fn refit_preview_registry_for_results(
    results: &EveLocalizationResults,
    sensor_dims: Option<(u16, u16)>,
) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: REFIT_PREVIEW_DATASET_ID.into(),
            title: "Refit preview".into(),
            kind: HostDatasetKind::TableV1(refit_preview_schema(results, sensor_dims)),
            empty_message: "No pending re-fit preview.".into(),
            display: Some(HostDatasetDisplayMetadata {
                layer_title: Some("Refit preview".into()),
                default_visibility: Some(true),
                default_color: Some([255, 210, 90, 240]),
                default_marker_shape: Some(HostMarkerShape::Circle),
                default_size: Some(8.0),
            }),
            relations: vec![HostDatasetRelation {
                target_dataset_id: ACCEPTED_CANDIDATE_EVENTS_DATASET_ID.into(),
                via_column: "cluster_id".into(),
                target_column: "cluster_id".into(),
            }],
        }],
        views: vec![HostViewDescriptor {
            id: REFIT_PREVIEW_VIEW_ID.into(),
            title: "Refit Preview".into(),
            dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
            placement: HostViewPlacement::AnalysisPanel,
            kind: HostViewKind::CompactTable,
        }],
        actions: Vec::new(),
    }
}

pub fn refit_preview_schema(
    results: &EveLocalizationResults,
    sensor_dims: Option<(u16, u16)>,
) -> TableSchema {
    let mut schema = current_localizations_schema_for_results(results, sensor_dims);
    schema.layer_id = Some(REFIT_PREVIEW_LAYER_ID.into());
    schema.semantic_label = Some("refit preview".into());
    schema
}

pub fn refit_preview_dataset(results: &EveLocalizationResults) -> TableDatasetV1 {
    current_localizations_dataset(results)
}

fn refit_action_param_schema() -> SettingsSchema {
    SettingsSchema {
        sections: vec![SettingsSection {
            label: "Refit parameters".into(),
            description: Some(
                "Re-run the chosen cluster's fit with these parameters and preview the result before committing."
                    .into(),
            ),
            default_open: true,
            items: vec![
                SettingItem {
                    key: "fit_method".into(),
                    label: "Method".into(),
                    tooltip: Some("Fitting backend to use for this cluster.".into()),
                    kind: SettingKind::Enum {
                        variants: vec![
                            FitMethod::LogGaussian.label().into(),
                            FitMethod::Gaussian.label().into(),
                            FitMethod::RadialSymmetry.label().into(),
                            FitMethod::Phasor.label().into(),
                            FitMethod::MeanXY.label().into(),
                        ],
                        default: FitMethod::LogGaussian.index(),
                    },
                },
                SettingItem {
                    key: "sigma_min_nm".into(),
                    label: "Sigma min".into(),
                    tooltip: Some("Reject fits with sigma below this bound.".into()),
                    kind: SettingKind::F64Slider {
                        min: 10.0,
                        max: 500.0,
                        default: FittingSettings::default().sigma_min_nm,
                        suffix: Some(" nm".into()),
                    },
                },
                SettingItem {
                    key: "sigma_max_nm".into(),
                    label: "Sigma max".into(),
                    tooltip: Some("Reject fits with sigma above this bound.".into()),
                    kind: SettingKind::F64Slider {
                        min: 10.0,
                        max: 500.0,
                        default: FittingSettings::default().sigma_max_nm,
                        suffix: Some(" nm".into()),
                    },
                },
                SettingItem {
                    key: "max_fit_residual".into(),
                    label: "Max residual".into(),
                    tooltip: Some("Reject fits whose residual exceeds this threshold.".into()),
                    kind: SettingKind::F64Drag {
                        min: 0.0,
                        max: 10.0,
                        speed: 0.01,
                        default: FittingSettings::default().max_fit_residual,
                    },
                },
            ],
        }],
    }
}

pub fn rejected_fit_row_id(row: &RejectedFitRow) -> u64 {
    row.timestamp_us
        ^ row.cluster_id.rotate_left(7)
        ^ row.x.to_bits().rotate_left(19)
        ^ row.y.to_bits().rotate_left(31)
        ^ row.fit_residual.to_bits().rotate_left(43)
        ^ (row.rejection_reason as u64).rotate_left(53)
        ^ row.span_start_us.rotate_left(17)
        ^ row.span_end_us.rotate_left(29)
}

fn rejected_fits_registry(
    rows: &[RejectedFitRow],
    sensor_dims: Option<(u16, u16)>,
    frame_window_start_us: u64,
    frame_window_end_us: u64,
) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: REJECTED_FITS_DATASET_ID.into(),
            title: "Rejected EVE fits".into(),
            kind: HostDatasetKind::TableV1(rejected_fits_schema(
                rows,
                sensor_dims,
                frame_window_start_us,
                frame_window_end_us,
            )),
            empty_message: "No rejected EVE fits in the current analysis window.".into(),
            display: Some(HostDatasetDisplayMetadata {
                layer_title: Some("Rejected EVE fits".into()),
                default_visibility: Some(false),
                default_color: Some([255, 90, 90, 200]),
                default_marker_shape: Some(HostMarkerShape::Diamond),
                default_size: Some(5.0),
            }),
            relations: vec![HostDatasetRelation {
                target_dataset_id: ACCEPTED_CANDIDATE_EVENTS_DATASET_ID.into(),
                via_column: "cluster_id".into(),
                target_column: "cluster_id".into(),
            }],
        }],
        views: vec![
            HostViewDescriptor {
                id: REJECTED_FITS_COMPACT_VIEW_ID.into(),
                title: "Rejected Fits".into(),
                dataset_id: REJECTED_FITS_DATASET_ID.into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            },
            HostViewDescriptor {
                id: REJECTED_FITS_TABLE_VIEW_ID.into(),
                title: "Rejected Fits Table".into(),
                dataset_id: REJECTED_FITS_DATASET_ID.into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::TableWindow,
            },
            HostViewDescriptor {
                id: REJECTED_FITS_3D_VIEW_ID.into(),
                title: "Rejected Fits 3D".into(),
                dataset_id: REJECTED_FITS_DATASET_ID.into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::Scatter3dFromTable {
                    x_column: "x_px".into(),
                    y_column: "y_px".into(),
                    z_column: "timestamp_us".into(),
                },
            },
        ],
        actions: Vec::new(),
    }
}

fn rejected_fits_schema(
    rows: &[RejectedFitRow],
    sensor_dims: Option<(u16, u16)>,
    frame_window_start_us: u64,
    frame_window_end_us: u64,
) -> TableSchema {
    TableSchema {
        columns: vec![
            TableColumn {
                id: "row_id".into(),
                title: "ID".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "cluster_id".into(),
                title: "Cluster".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "timestamp_us".into(),
                title: "Timestamp (us)".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "span_start_us".into(),
                title: "Span Start (us)".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "span_end_us".into(),
                title: "Span End (us)".into(),
                value_type: TableValueType::U64,
            },
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
                id: "fit_residual".into(),
                title: "Fit residual".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "n_events".into(),
                title: "Events".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "polarity_balance".into(),
                title: "Polarity balance".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "rejection_reason".into(),
                title: "Rejection reason".into(),
                value_type: TableValueType::String,
            },
        ],
        coordinate_space_2d: rejected_fits_2d_space(rows, sensor_dims),
        coordinate_space_3d: rejected_fits_3d_space(
            rows,
            sensor_dims,
            frame_window_start_us,
            frame_window_end_us,
        ),
        row_id_column: Some("row_id".into()),
        time_column: Some("timestamp_us".into()),
        layer_id: Some(REJECTED_FITS_LAYER_ID.into()),
        semantic_label: Some("rejected fits".into()),
        provenance: Some(TableRowProvenance {
            anchor_time_column: Some("timestamp_us".into()),
            span_start_column: Some("span_start_us".into()),
            span_end_column: Some("span_end_us".into()),
            anchor_frame_column: None,
        }),
        column_display: vec![
            TableColumnDisplayEntry {
                column_id: "row_id".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Identifier),
                    hide_in_compact: true,
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "cluster_id".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Identifier),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "timestamp_us".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::TimestampMicros),
                    label: Some("Time".into()),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "span_start_us".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::TimestampMicros),
                    label: Some("Span start".into()),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "span_end_us".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::TimestampMicros),
                    label: Some("Span end".into()),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "x_px".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 1 }),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "y_px".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 1 }),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "sigma_x_px".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 2 }),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "sigma_y_px".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 2 }),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "fit_residual".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 3 }),
                    ..Default::default()
                },
            },
            TableColumnDisplayEntry {
                column_id: "rejection_reason".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Category),
                    headline: true,
                    ..Default::default()
                },
            },
        ],
    }
}

fn rejected_fits_dataset(rows: &[RejectedFitRow]) -> TableDatasetV1 {
    TableDatasetV1::new(vec![
        TableColumnData {
            column_id: "row_id".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.row_id).collect()),
        },
        TableColumnData {
            column_id: "cluster_id".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.cluster_id).collect()),
        },
        TableColumnData {
            column_id: "timestamp_us".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.timestamp_us).collect()),
        },
        TableColumnData {
            column_id: "span_start_us".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.span_start_us).collect()),
        },
        TableColumnData {
            column_id: "span_end_us".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.span_end_us).collect()),
        },
        TableColumnData {
            column_id: "x_px".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.x).collect()),
        },
        TableColumnData {
            column_id: "y_px".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.y).collect()),
        },
        TableColumnData {
            column_id: "sigma_x_px".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.sigma_x).collect()),
        },
        TableColumnData {
            column_id: "sigma_y_px".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.sigma_y).collect()),
        },
        TableColumnData {
            column_id: "fit_residual".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.fit_residual).collect()),
        },
        TableColumnData {
            column_id: "n_events".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.n_events).collect()),
        },
        TableColumnData {
            column_id: "polarity_balance".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.polarity_balance).collect()),
        },
        TableColumnData {
            column_id: "rejection_reason".into(),
            values: TableColumnValues::String(
                rows.iter()
                    .map(|row| row.rejection_reason.as_str().to_owned())
                    .collect(),
            ),
        },
    ])
    .expect("rejected-fit columns should stay aligned")
}

fn rejected_fits_2d_space(
    rows: &[RejectedFitRow],
    sensor_dims: Option<(u16, u16)>,
) -> Option<TableCoordinateSpace2d> {
    sensor_dims
        .map(|(width, height)| (0.0, f64::from(width), 0.0, f64::from(height)))
        .or_else(|| rejected_fit_xy_bounds(rows))
        .map(|(x_min, x_max, y_min, y_max)| TableCoordinateSpace2d {
            x_column: "x_px".into(),
            y_column: "y_px".into(),
            x_min,
            x_max,
            y_min,
            y_max,
        })
}

fn rejected_fits_3d_space(
    rows: &[RejectedFitRow],
    sensor_dims: Option<(u16, u16)>,
    frame_window_start_us: u64,
    frame_window_end_us: u64,
) -> Option<TableCoordinateSpace3d> {
    let (x_min, x_max, y_min, y_max) = sensor_dims
        .map(|(width, height)| (0.0, f64::from(width), 0.0, f64::from(height)))
        .or_else(|| rejected_fit_xy_bounds(rows))?;
    let (z_min, z_max) = rejected_fit_time_bounds(rows)
        .unwrap_or((frame_window_start_us as f64, frame_window_end_us as f64));
    Some(TableCoordinateSpace3d {
        x_column: "x_px".into(),
        y_column: "y_px".into(),
        z_column: "timestamp_us".into(),
        x_min,
        x_max,
        y_min,
        y_max,
        z_min,
        z_max,
    })
}

fn rejected_fit_xy_bounds(rows: &[RejectedFitRow]) -> Option<(f64, f64, f64, f64)> {
    let mut rows = rows.iter();
    let first = rows.next()?;
    let mut x_min = first.x;
    let mut x_max = first.x;
    let mut y_min = first.y;
    let mut y_max = first.y;
    for row in rows {
        x_min = x_min.min(row.x);
        x_max = x_max.max(row.x);
        y_min = y_min.min(row.y);
        y_max = y_max.max(row.y);
    }
    Some((x_min, x_max.max(x_min), y_min, y_max.max(y_min)))
}

fn rejected_fit_time_bounds(rows: &[RejectedFitRow]) -> Option<(f64, f64)> {
    let mut rows = rows.iter();
    let first = rows.next()?;
    let mut min_time = first.timestamp_us;
    let mut max_time = first.timestamp_us;
    for row in rows {
        min_time = min_time.min(row.timestamp_us);
        max_time = max_time.max(row.timestamp_us);
    }
    Some((min_time as f64, max_time.max(min_time) as f64))
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
    pub show_rejected_overlay: bool,
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
            show_rejected_overlay: false,
        }
    }
}

pub struct EveSmlmFittingPlugin {
    enabled: bool,
    settings: FittingSettings,
    current_results: EveLocalizationResults,
    current_rejected_fits: Vec<RejectedFitRow>,
    host_results: EveLocalizationResults,
    host_rejected_fits: Vec<RejectedFitRow>,
    sensor_dims: Option<(u16, u16)>,
    last_localization_count: usize,
    last_rejection_count: usize,
    last_fit_failure_count: usize,
    last_sigma_rejection_count: usize,
    last_residual_rejection_count: usize,
    last_status: String,
    dataset_generation: u64,
    refit_preview_results: EveLocalizationResults,
    /// Parallel to `refit_preview_results.localizations`: for each preview
    /// row, the `row_id` of the current localization it should replace on
    /// commit (or `None` if commit should append).
    refit_preview_replaces: Vec<Option<u64>>,
    last_consumed_action_request_id: u64,
    last_action_notice: Option<String>,
}

impl Default for EveSmlmFittingPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: FittingSettings::default(),
            current_results: EveLocalizationResults::default(),
            current_rejected_fits: Vec::new(),
            host_results: EveLocalizationResults::default(),
            host_rejected_fits: Vec::new(),
            sensor_dims: None,
            last_localization_count: 0,
            last_rejection_count: 0,
            last_fit_failure_count: 0,
            last_sigma_rejection_count: 0,
            last_residual_rejection_count: 0,
            last_status:
                "Enable the plugin to fit EVE candidate clusters to sub-pixel localizations.".into(),
            dataset_generation: 0,
            refit_preview_results: EveLocalizationResults::default(),
            refit_preview_replaces: Vec::new(),
            last_consumed_action_request_id: 0,
            last_action_notice: None,
        }
    }
}

impl EveSmlmFittingPlugin {
    fn nm_per_pixel(&self, context: &HostContext<'_>) -> f64 {
        context
            .get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)
            .ok()
            .flatten()
            .map(|settings| settings.nm_per_pixel)
            .unwrap_or(self.settings.nm_per_pixel)
    }

    fn sync_sensor_dims(&mut self, context: &HostContext<'_>, frame: &PluginFrame<'_>) {
        self.sensor_dims = context
            .get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)
            .ok()
            .flatten()
            .map(|settings| (settings.sensor_width, settings.sensor_height))
            .or(Some((frame.width(), frame.height())));
    }

    fn analyze_candidates(
        &mut self,
        candidates: Option<&EveCandidates>,
        output: &mut HostOutput<'_>,
        nm_per_pixel: f64,
    ) -> (
        EveLocalizationResults,
        LocalizationResults,
        Vec<RejectedFitRow>,
    ) {
        let Some(candidates) = candidates else {
            self.last_localization_count = 0;
            self.last_rejection_count = 0;
            self.last_fit_failure_count = 0;
            self.last_sigma_rejection_count = 0;
            self.last_residual_rejection_count = 0;
            self.last_status = "Waiting for EVE Candidate Finding.".into();
            Self::warning(
                output,
                AnalysisSeverity::Info,
                "EVE fitting requires candidate clusters from EVE Candidate Finding.",
            );
            return (
                EveLocalizationResults::default(),
                LocalizationResults::default(),
                Vec::new(),
            );
        };

        let mut localizations = Vec::new();
        let mut rejected_fits = Vec::new();
        let mut fit_failures = 0usize;
        let mut sigma_rejections = 0usize;
        let mut residual_rejections = 0usize;
        for cluster in &candidates.clusters {
            let (span_start_us, span_end_us) = cluster_time_span(cluster);
            let timestamp_fallback = estimate_timestamp_us(
                &cluster.events,
                cluster.centroid_x,
                cluster.centroid_y,
                cluster_extent_radius(cluster),
            );
            let Some(fit) = fit_cluster(cluster, self.settings.fit_method) else {
                fit_failures += 1;
                let mut rejected = RejectedFitRow {
                    row_id: 0,
                    cluster_id: cluster.cluster_id,
                    x: cluster.centroid_x,
                    y: cluster.centroid_y,
                    sigma_x: 0.0,
                    sigma_y: 0.0,
                    fit_residual: 0.0,
                    n_events: cluster.event_count() as u64,
                    polarity_balance: cluster.polarity_balance(),
                    rejection_reason: RejectionReason::FitFailed,
                    timestamp_us: timestamp_fallback,
                    span_start_us,
                    span_end_us,
                };
                rejected.row_id = rejected_fit_row_id(&rejected);
                rejected_fits.push(rejected);
                continue;
            };

            if self.settings.fit_method.produces_sigma() {
                let sigma_x_nm = fit.sigma_x * nm_per_pixel;
                let sigma_y_nm = fit.sigma_y * nm_per_pixel;
                if sigma_x_nm < self.settings.sigma_min_nm
                    || sigma_x_nm > self.settings.sigma_max_nm
                    || sigma_y_nm < self.settings.sigma_min_nm
                    || sigma_y_nm > self.settings.sigma_max_nm
                {
                    sigma_rejections += 1;
                    let timestamp_us = estimate_timestamp_us(
                        &cluster.events,
                        fit.x,
                        fit.y,
                        fit_radius(cluster, &fit),
                    );
                    let mut rejected = RejectedFitRow {
                        row_id: 0,
                        cluster_id: cluster.cluster_id,
                        x: fit.x,
                        y: fit.y,
                        sigma_x: fit.sigma_x,
                        sigma_y: fit.sigma_y,
                        fit_residual: fit.residual,
                        n_events: cluster.event_count() as u64,
                        polarity_balance: cluster.polarity_balance(),
                        rejection_reason: RejectionReason::SigmaOutOfBounds,
                        timestamp_us,
                        span_start_us,
                        span_end_us,
                    };
                    rejected.row_id = rejected_fit_row_id(&rejected);
                    rejected_fits.push(rejected);
                    continue;
                }
            }

            if fit.residual > self.settings.max_fit_residual {
                residual_rejections += 1;
                let timestamp_us =
                    estimate_timestamp_us(&cluster.events, fit.x, fit.y, fit_radius(cluster, &fit));
                let mut rejected = RejectedFitRow {
                    row_id: 0,
                    cluster_id: cluster.cluster_id,
                    x: fit.x,
                    y: fit.y,
                    sigma_x: fit.sigma_x,
                    sigma_y: fit.sigma_y,
                    fit_residual: fit.residual,
                    n_events: cluster.event_count() as u64,
                    polarity_balance: cluster.polarity_balance(),
                    rejection_reason: RejectionReason::ResidualTooHigh,
                    timestamp_us,
                    span_start_us,
                    span_end_us,
                };
                rejected.row_id = rejected_fit_row_id(&rejected);
                rejected_fits.push(rejected);
                continue;
            }

            localizations.push(EveLocalization {
                cluster_id: cluster.cluster_id,
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
                span_start_us,
                span_end_us,
                n_events: cluster.event_count(),
                polarity_balance: cluster.polarity_balance(),
                fit_residual: fit.residual,
                fit_method: self.settings.fit_method,
            });
        }

        self.last_localization_count = localizations.len();
        self.last_fit_failure_count = fit_failures;
        self.last_sigma_rejection_count = sigma_rejections;
        self.last_residual_rejection_count = residual_rejections;
        self.last_rejection_count = fit_failures + sigma_rejections + residual_rejections;
        self.last_status = format!(
            "{} accepted, {} rejected ({} fit failures, {} sigma bounds, {} residual) with {}.",
            self.last_localization_count,
            self.last_rejection_count,
            self.last_fit_failure_count,
            self.last_sigma_rejection_count,
            self.last_residual_rejection_count,
            self.settings.fit_method.label()
        );

        if self.settings.show_overlay && !localizations.is_empty() {
            let stable_ids: Vec<String> = localizations
                .iter()
                .map(|localization| localization_row_id(localization).to_string())
                .collect();
            let markers: Vec<FfiMarkerOverlayItem> = localizations
                .iter()
                .zip(stable_ids.iter())
                .map(|(localization, stable_id)| FfiMarkerOverlayItem {
                    x: localization.x as f32,
                    y: localization.y as f32,
                    shape: FfiMarkerShape::Cross,
                    size: 6.0,
                    color: FfiColorRgba::from_rgba(OVERLAY_COLOR),
                    timestamp_us: localization.timestamp_us,
                    has_timestamp: true,
                    stable_id: stable_id.as_str().into(),
                    source_dataset_id: CURRENT_LOCALIZATIONS_DATASET_ID.into(),
                    source_row_id: stable_id.as_str().into(),
                })
                .collect();
            output.add_marker_overlay(
                &markers,
                Some(CURRENT_LOCALIZATIONS_DATASET_ID),
                Some(CURRENT_LOCALIZATIONS_LAYER_ID),
                Some(self.name()),
            );
        }

        if self.settings.show_rejected_overlay && !rejected_fits.is_empty() {
            let stable_ids: Vec<String> = rejected_fits
                .iter()
                .map(|row| row.row_id.to_string())
                .collect();
            let markers: Vec<FfiMarkerOverlayItem> = rejected_fits
                .iter()
                .zip(stable_ids.iter())
                .map(|(row, stable_id)| FfiMarkerOverlayItem {
                    x: row.x as f32,
                    y: row.y as f32,
                    shape: FfiMarkerShape::Diamond,
                    size: 5.0,
                    color: FfiColorRgba::from_rgba([255, 90, 90, 180]),
                    timestamp_us: row.timestamp_us,
                    has_timestamp: true,
                    stable_id: stable_id.as_str().into(),
                    source_dataset_id: REJECTED_FITS_DATASET_ID.into(),
                    source_row_id: stable_id.as_str().into(),
                })
                .collect();
            output.add_marker_overlay(
                &markers,
                Some(REJECTED_FITS_DATASET_ID),
                Some(REJECTED_FITS_LAYER_ID),
                Some(self.name()),
            );
        }

        let eve_results = EveLocalizationResults {
            localizations,
            frame_window_start_us: candidates.frame_window_start_us,
            frame_window_end_us: candidates.frame_window_end_us,
        };
        let compatibility_results = to_localization_results(&eve_results);

        (eve_results, compatibility_results, rejected_fits)
    }

    pub fn reset(&mut self) {
        self.current_results = EveLocalizationResults::default();
        self.current_rejected_fits.clear();
        self.host_results = EveLocalizationResults::default();
        self.host_rejected_fits.clear();
        self.sensor_dims = None;
        self.last_localization_count = 0;
        self.last_rejection_count = 0;
        self.last_fit_failure_count = 0;
        self.last_sigma_rejection_count = 0;
        self.last_residual_rejection_count = 0;
        self.last_status = "Waiting for the next candidate set.".into();
        self.refit_preview_results = EveLocalizationResults::default();
        self.refit_preview_replaces.clear();
        self.last_action_notice = None;
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
    }

    fn parse_usize(value: Value) -> Option<usize> {
        value.as_u64().and_then(|value| usize::try_from(value).ok())
    }

    fn update_history_bounds(results: &mut EveLocalizationResults) {
        let Some(first) = results.localizations.first() else {
            results.frame_window_start_us = 0;
            results.frame_window_end_us = 0;
            return;
        };
        let mut start = first.span_start_us;
        let mut end = first.span_end_us.max(first.span_start_us);
        for localization in &results.localizations[1..] {
            start = start.min(localization.span_start_us);
            end = end.max(localization.span_end_us.max(localization.span_start_us));
        }
        results.frame_window_start_us = start;
        results.frame_window_end_us = end;
    }

    fn upsert_history_localization(&mut self, localization: EveLocalization) {
        self.host_rejected_fits
            .retain(|row| row.cluster_id != localization.cluster_id);
        if let Some(index) = self
            .host_results
            .localizations
            .iter()
            .position(|existing| existing.cluster_id == localization.cluster_id)
        {
            self.host_results.localizations[index] = localization;
        } else {
            self.host_results.localizations.push(localization);
        }
        self.host_results.localizations.sort_by_key(|row| {
            (
                row.span_start_us,
                row.span_end_us,
                row.timestamp_us,
                row.cluster_id,
            )
        });
        Self::update_history_bounds(&mut self.host_results);
    }

    fn upsert_history_rejected_fit(&mut self, row: RejectedFitRow) {
        if self
            .host_results
            .localizations
            .iter()
            .any(|localization| localization.cluster_id == row.cluster_id)
        {
            return;
        }
        if let Some(index) = self
            .host_rejected_fits
            .iter()
            .position(|existing| existing.cluster_id == row.cluster_id)
        {
            self.host_rejected_fits[index] = row;
        } else {
            self.host_rejected_fits.push(row);
        }
        self.host_rejected_fits.sort_by_key(|entry| {
            (
                entry.span_start_us,
                entry.span_end_us,
                entry.timestamp_us,
                entry.cluster_id,
            )
        });
    }

    fn integrate_frame_history(
        &mut self,
        localizations: &[EveLocalization],
        rejected_fits: &[RejectedFitRow],
    ) {
        for localization in localizations.iter().cloned() {
            self.upsert_history_localization(localization);
        }
        for row in rejected_fits.iter().cloned() {
            self.upsert_history_rejected_fit(row);
        }
    }

    fn parse_u64_field(value: &Value) -> Option<u64> {
        value
            .as_u64()
            .or_else(|| value.as_i64().and_then(|value| u64::try_from(value).ok()))
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    }

    fn parse_u16_field(value: &Value) -> Option<u16> {
        Self::parse_u64_field(value)
            .and_then(|value| u16::try_from(value).ok())
            .or_else(|| {
                value
                    .as_f64()
                    .map(|value| value.round().clamp(0.0, f64::from(u16::MAX)) as u16)
            })
    }

    fn parse_bool_field(value: &Value) -> Option<bool> {
        value
            .as_bool()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    }

    fn cluster_from_action_params(params: &Value, expected_cluster_id: u64) -> Option<EveCluster> {
        let rows = params.get(HOST_ACTION_CLUSTER_ROWS_PARAM)?.as_array()?;
        if rows.is_empty() {
            return None;
        }

        let mut events = Vec::with_capacity(rows.len());
        let mut pixel_histogram: HashMap<(u16, u16), (u32, u32)> = HashMap::new();
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        let mut count: f64 = 0.0;
        let mut x_min = u16::MAX;
        let mut x_max = 0u16;
        let mut y_min = u16::MAX;
        let mut y_max = 0u16;

        for row in rows {
            let object = row.as_object()?;
            let cluster_id = Self::parse_u64_field(object.get("cluster_id")?)?;
            if cluster_id != expected_cluster_id {
                return None;
            }
            let x = Self::parse_u16_field(object.get("x_px")?)?;
            let y = Self::parse_u16_field(object.get("y_px")?)?;
            let timestamp = Self::parse_u64_field(object.get("timestamp_us")?)?;
            let polarity = Self::parse_bool_field(object.get("polarity")?)?;

            events.push(EveEvent {
                timestamp,
                x,
                y,
                polarity,
            });

            let entry = pixel_histogram.entry((x, y)).or_insert((0, 0));
            if polarity {
                entry.0 = entry.0.saturating_add(1);
            } else {
                entry.1 = entry.1.saturating_add(1);
            }
            x_min = x_min.min(x);
            x_max = x_max.max(x);
            y_min = y_min.min(y);
            y_max = y_max.max(y);
            sum_x += f64::from(x);
            sum_y += f64::from(y);
            count += 1.0;
        }

        if events.is_empty() {
            return None;
        }

        let mut pixel_histogram: Vec<_> = pixel_histogram
            .into_iter()
            .map(|((x, y), (positive, negative))| (x, y, positive, negative))
            .collect();
        pixel_histogram.sort_by_key(|(x, y, _, _)| (*y, *x));

        Some(EveCluster {
            cluster_id: expected_cluster_id,
            pixel_histogram,
            events,
            centroid_x: sum_x / count.max(1.0),
            centroid_y: sum_y / count.max(1.0),
            x_min,
            x_max,
            y_min,
            y_max,
            complete: true,
            boundary: None,
        })
    }

    fn warning(output: &mut HostOutput<'_>, severity: AnalysisSeverity, message: &str) {
        output.add_warning("EVE Candidate Fitting", severity, message);
    }

    fn handle_action_requests(
        &mut self,
        context: &mut HostContext<'_>,
        output: &mut HostOutput<'_>,
        candidates: Option<&EveCandidates>,
        nm_per_pixel: f64,
    ) {
        let queue = match context
            .get_persistent::<HostActionRequestQueue>(CTX_INVESTIGATION_ACTION_REQUESTS)
        {
            Ok(Some(queue)) => queue,
            Ok(None) => return,
            Err(err) => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    &format!("Reading action requests failed: {err}"),
                );
                return;
            }
        };

        let mut handled_any = false;
        for request in &queue.requests {
            if request.request_id <= self.last_consumed_action_request_id {
                continue;
            }
            match request.action_id.as_str() {
                ACTION_REFIT_CLUSTER => {
                    self.handle_refit_cluster(request, output, candidates, nm_per_pixel);
                    handled_any = true;
                }
                ACTION_COMMIT_REFIT => {
                    self.handle_commit_refit(request, output);
                    handled_any = true;
                }
                ACTION_DISCARD_REFIT => {
                    self.handle_discard_refit(request, output);
                    handled_any = true;
                }
                _ => continue,
            }
            self.last_consumed_action_request_id = request.request_id;
        }

        if handled_any {
            self.dataset_generation = self.dataset_generation.wrapping_add(1);
        }
    }

    fn handle_refit_cluster(
        &mut self,
        request: &augur_plugin_api::HostActionRequest,
        output: &mut HostOutput<'_>,
        candidates: Option<&EveCandidates>,
        nm_per_pixel: f64,
    ) {
        use augur_plugin_api::HostActionScopePayload;
        let (dataset_id, group_column, group_value) = match &request.scope_payload {
            HostActionScopePayload::Cluster {
                dataset_id,
                group_column,
                group_value,
            } => (
                dataset_id.clone(),
                group_column.clone(),
                group_value.clone(),
            ),
            _ => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    "Re-fit action requires a Cluster scope payload.",
                );
                return;
            }
        };
        if dataset_id != ACCEPTED_CANDIDATE_EVENTS_DATASET_ID || group_column != "cluster_id" {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!(
                    "Ignoring re-fit request for unsupported scope ({dataset_id}/{group_column})."
                ),
            );
            return;
        }

        let cluster_id: u64 = match group_value.parse() {
            Ok(value) => value,
            Err(_) => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    &format!("Re-fit request has non-numeric cluster id: {group_value}"),
                );
                return;
            }
        };

        let params = &request.params;
        let cluster_from_params = Self::cluster_from_action_params(params, cluster_id);
        let cluster_from_candidates = candidates.and_then(|candidates| {
            candidates
                .clusters
                .iter()
                .find(|cluster| cluster.cluster_id == cluster_id)
                .cloned()
        });
        let Some(cluster) = cluster_from_params.or(cluster_from_candidates) else {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Re-fit request for cluster {cluster_id} has no usable cluster snapshot."),
            );
            return;
        };
        let fit_method = params
            .get("fit_method")
            .and_then(|value| Self::parse_usize(value.clone()))
            .map(FitMethod::from_index)
            .unwrap_or(self.settings.fit_method);
        let sigma_min_nm = params
            .get("sigma_min_nm")
            .and_then(Value::as_f64)
            .unwrap_or(self.settings.sigma_min_nm);
        let sigma_max_nm = params
            .get("sigma_max_nm")
            .and_then(Value::as_f64)
            .unwrap_or(self.settings.sigma_max_nm);
        let max_fit_residual = params
            .get("max_fit_residual")
            .and_then(Value::as_f64)
            .unwrap_or(self.settings.max_fit_residual);

        let Some(fit) = fit_cluster(&cluster, fit_method) else {
            self.last_action_notice = Some(format!("Re-fit failed for cluster {cluster_id}."));
            Self::warning(
                output,
                AnalysisSeverity::Info,
                &format!("Re-fit for cluster {cluster_id} did not converge."),
            );
            return;
        };

        if fit_method.produces_sigma() {
            let sigma_x_nm = fit.sigma_x * nm_per_pixel;
            let sigma_y_nm = fit.sigma_y * nm_per_pixel;
            if sigma_x_nm < sigma_min_nm
                || sigma_x_nm > sigma_max_nm
                || sigma_y_nm < sigma_min_nm
                || sigma_y_nm > sigma_max_nm
            {
                self.last_action_notice = Some(format!(
                    "Re-fit for cluster {cluster_id} is outside sigma bounds."
                ));
                Self::warning(
                    output,
                    AnalysisSeverity::Info,
                    &format!("Re-fit for cluster {cluster_id} rejected by sigma bounds."),
                );
                return;
            }
        }

        if fit.residual > max_fit_residual {
            self.last_action_notice = Some(format!(
                "Re-fit for cluster {cluster_id} exceeds residual threshold."
            ));
            Self::warning(
                output,
                AnalysisSeverity::Info,
                &format!("Re-fit for cluster {cluster_id} rejected by residual threshold."),
            );
            return;
        }

        let timestamp_us =
            estimate_timestamp_us(&cluster.events, fit.x, fit.y, fit_radius(&cluster, &fit));
        let (span_start_us, span_end_us) = cluster_time_span(&cluster);
        let new_localization = EveLocalization {
            cluster_id,
            x: fit.x,
            y: fit.y,
            sigma_x: fit.sigma_x,
            sigma_y: fit.sigma_y,
            timestamp_us,
            span_start_us,
            span_end_us,
            n_events: cluster.event_count(),
            polarity_balance: cluster.polarity_balance(),
            fit_residual: fit.residual,
            fit_method,
        };

        let replaces = find_current_localization_for_cluster(&self.host_results, &cluster)
            .map(localization_row_id);

        self.refit_preview_results
            .localizations
            .push(new_localization);
        self.refit_preview_replaces.push(replaces);
        Self::update_history_bounds(&mut self.refit_preview_results);

        self.last_action_notice = Some(format!(
            "Re-fit preview added for cluster {cluster_id} ({}).",
            fit_method.label()
        ));
    }

    fn handle_commit_refit(
        &mut self,
        request: &augur_plugin_api::HostActionRequest,
        output: &mut HostOutput<'_>,
    ) {
        use augur_plugin_api::HostActionScopePayload;
        let (dataset_id, row_id) = match &request.scope_payload {
            HostActionScopePayload::Row { dataset_id, row_id } => {
                (dataset_id.clone(), row_id.clone())
            }
            _ => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    "Commit action requires a Row scope payload.",
                );
                return;
            }
        };
        if dataset_id != REFIT_PREVIEW_DATASET_ID {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Ignoring commit for unsupported dataset {dataset_id}."),
            );
            return;
        }

        let target_row_id: u64 = match row_id.parse() {
            Ok(value) => value,
            Err(_) => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    &format!("Commit row_id is not numeric: {row_id}"),
                );
                return;
            }
        };

        let index = self
            .refit_preview_results
            .localizations
            .iter()
            .position(|localization| localization_row_id(localization) == target_row_id);
        let Some(index) = index else {
            Self::warning(
                output,
                AnalysisSeverity::Info,
                &format!("Commit row {target_row_id} is not in the preview."),
            );
            return;
        };

        let localization = self.refit_preview_results.localizations.remove(index);
        self.refit_preview_replaces.remove(index);
        let cluster_id = localization.cluster_id;
        self.upsert_history_localization(localization.clone());
        self.host_rejected_fits
            .retain(|row| row.cluster_id != cluster_id);

        if let Some(old_index) = self
            .current_results
            .localizations
            .iter()
            .position(|entry| entry.cluster_id == cluster_id)
        {
            self.current_results.localizations[old_index] = localization;
        }

        Self::update_history_bounds(&mut self.refit_preview_results);

        self.last_action_notice = Some(format!("Committed refit preview row {target_row_id}."));
    }

    fn handle_discard_refit(
        &mut self,
        request: &augur_plugin_api::HostActionRequest,
        output: &mut HostOutput<'_>,
    ) {
        use augur_plugin_api::HostActionScopePayload;
        let dataset_id = match &request.scope_payload {
            HostActionScopePayload::Dataset { dataset_id } => dataset_id.clone(),
            _ => {
                Self::warning(
                    output,
                    AnalysisSeverity::Warning,
                    "Discard action requires a Dataset scope payload.",
                );
                return;
            }
        };
        if dataset_id != REFIT_PREVIEW_DATASET_ID {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Ignoring discard for unsupported dataset {dataset_id}."),
            );
            return;
        }

        let dropped = self.refit_preview_results.localizations.len();
        self.refit_preview_results = EveLocalizationResults::default();
        self.refit_preview_replaces.clear();
        self.last_action_notice = Some(format!("Discarded {dropped} preview row(s)."));
    }

    fn emit_refit_preview_overlay(&self, output: &mut HostOutput<'_>) {
        let localizations = &self.refit_preview_results.localizations;
        let stable_ids: Vec<String> = localizations
            .iter()
            .map(|localization| localization_row_id(localization).to_string())
            .collect();
        let markers: Vec<FfiMarkerOverlayItem> = localizations
            .iter()
            .zip(stable_ids.iter())
            .map(|(localization, stable_id)| FfiMarkerOverlayItem {
                x: localization.x as f32,
                y: localization.y as f32,
                shape: FfiMarkerShape::FilledCircle,
                size: 8.0,
                color: FfiColorRgba::from_rgba([255, 210, 90, 240]),
                timestamp_us: localization.timestamp_us,
                has_timestamp: true,
                stable_id: stable_id.as_str().into(),
                source_dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
                source_row_id: stable_id.as_str().into(),
            })
            .collect();
        output.add_marker_overlay(
            &markers,
            Some(REFIT_PREVIEW_DATASET_ID),
            Some(REFIT_PREVIEW_LAYER_ID),
            Some(self.name()),
        );
    }
}

fn find_current_localization_for_cluster<'a>(
    results: &'a EveLocalizationResults,
    cluster: &EveCluster,
) -> Option<&'a EveLocalization> {
    if let Some(localization) = results
        .localizations
        .iter()
        .find(|localization| localization.cluster_id == cluster.cluster_id)
    {
        return Some(localization);
    }
    let timestamp_range_us: i64 = 2_000;
    let mut best: Option<(f64, &'a EveLocalization)> = None;
    for localization in &results.localizations {
        let dt = (localization.timestamp_us as i64)
            .saturating_sub_unsigned(cluster_anchor_timestamp(cluster));
        if dt.abs() > timestamp_range_us {
            continue;
        }
        let dx = localization.x - cluster.centroid_x;
        let dy = localization.y - cluster.centroid_y;
        let score = dx * dx + dy * dy + (dt as f64).powi(2) * 1e-6;
        if best.map_or(true, |(b, _)| score < b) {
            best = Some((score, localization));
        }
    }
    best.map(|(_, localization)| localization)
}

fn cluster_anchor_timestamp(cluster: &EveCluster) -> u64 {
    if cluster.events.is_empty() {
        return 0;
    }
    let sum: u128 = cluster
        .events
        .iter()
        .map(|event| event.timestamp as u128)
        .sum();
    (sum / cluster.events.len() as u128) as u64
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
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        self.sync_sensor_dims(context, frame);
        let nm_per_pixel = self.nm_per_pixel(context);
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

        let (eve_results, _compatibility, rejected_fits) =
            self.analyze_candidates(candidates.as_ref(), output, nm_per_pixel);
        self.current_results = eve_results.clone();
        self.current_rejected_fits = rejected_fits.clone();
        self.integrate_frame_history(&eve_results.localizations, &rejected_fits);
        self.dataset_generation = self.dataset_generation.wrapping_add(1);

        self.handle_action_requests(context, output, candidates.as_ref(), nm_per_pixel);

        if self.settings.show_overlay && !self.refit_preview_results.localizations.is_empty() {
            self.emit_refit_preview_overlay(output);
        }

        let published_results = self.current_results.clone();
        let compatibility = to_localization_results(&published_results);
        if let Err(err) = context.publish(CTX_EVE_LOCALIZATION_RESULTS, &published_results) {
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
                    SettingItem {
                        key: "show_rejected_overlay".into(),
                        label: "Show rejected".into(),
                        tooltip: Some(
                            "Draw rejected fits as linked diamond markers in the preview.".into(),
                        ),
                        kind: SettingKind::Bool {
                            default: self.settings.show_rejected_overlay,
                        },
                    },
                ],
            }],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "fit_method" => Some(json!(self.settings.fit_method.index())),
            "sigma_min_nm" => Some(json!(self.settings.sigma_min_nm)),
            "sigma_max_nm" => Some(json!(self.settings.sigma_max_nm)),
            "max_fit_residual" => Some(json!(self.settings.max_fit_residual)),
            "show_overlay" => Some(json!(self.settings.show_overlay)),
            "show_rejected_overlay" => Some(json!(self.settings.show_rejected_overlay)),
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
            "show_rejected_overlay" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_rejected_overlay must be a boolean".into());
                };
                self.settings.show_rejected_overlay = value;
            }
            _ => return Err(format!("unknown setting: {key}")),
        }

        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = vec![
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
        ];
        if self.last_rejection_count > 0 {
            entries.push(StatusEntry::LabeledValue {
                label: "Fit fail".into(),
                value: self.last_fit_failure_count.to_string(),
                color: None,
            });
            entries.push(StatusEntry::LabeledValue {
                label: "Sigma".into(),
                value: self.last_sigma_rejection_count.to_string(),
                color: None,
            });
            entries.push(StatusEntry::LabeledValue {
                label: "Residual".into(),
                value: self.last_residual_rejection_count.to_string(),
                color: None,
            });
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        let mut registry =
            current_localizations_registry_for_results(&self.host_results, self.sensor_dims);
        let rejected_registry = rejected_fits_registry(
            &self.host_rejected_fits,
            self.sensor_dims,
            self.host_results.frame_window_start_us,
            self.host_results.frame_window_end_us,
        );
        registry.datasets.extend(rejected_registry.datasets);
        registry.views.extend(rejected_registry.views);
        let preview_registry =
            refit_preview_registry_for_results(&self.refit_preview_results, self.sensor_dims);
        registry.datasets.extend(preview_registry.datasets);
        registry.views.extend(preview_registry.views);

        let param_schema = serde_json::to_value(refit_action_param_schema()).ok();
        registry.actions = vec![
            HostActionDescriptor {
                id: ACTION_REFIT_CLUSTER.into(),
                title: "Re-fit cluster…".into(),
                scope: HostActionScope::Cluster {
                    dataset_id: ACCEPTED_CANDIDATE_EVENTS_DATASET_ID.into(),
                    group_column: "cluster_id".into(),
                },
                param_schema,
            },
            HostActionDescriptor {
                id: ACTION_COMMIT_REFIT.into(),
                title: "Commit refit".into(),
                scope: HostActionScope::Row {
                    dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
                },
                param_schema: None,
            },
            HostActionDescriptor {
                id: ACTION_DISCARD_REFIT.into(),
                title: "Discard refit preview".into(),
                scope: HostActionScope::Dataset {
                    dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
                },
                param_schema: None,
            },
        ];
        registry
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            CURRENT_LOCALIZATIONS_DATASET_ID => {
                serde_json::to_vec(&current_localizations_dataset(&self.host_results)).ok()
            }
            REJECTED_FITS_DATASET_ID => {
                serde_json::to_vec(&rejected_fits_dataset(&self.host_rejected_fits)).ok()
            }
            REFIT_PREVIEW_DATASET_ID => {
                serde_json::to_vec(&refit_preview_dataset(&self.refit_preview_results)).ok()
            }
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            CURRENT_LOCALIZATIONS_DATASET_ID
            | REJECTED_FITS_DATASET_ID
            | REFIT_PREVIEW_DATASET_ID => self.dataset_generation,
            _ => 0,
        }
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

fn cluster_extent_radius(cluster: &EveCluster) -> f64 {
    let dx = f64::from(cluster.x_max.saturating_sub(cluster.x_min)) + 1.0;
    let dy = f64::from(cluster.y_max.saturating_sub(cluster.y_min)) + 1.0;
    0.5 * dx.max(dy).max(1.0)
}

fn fit_radius(cluster: &EveCluster, fit: &FitEstimate) -> f64 {
    if fit.sigma_x > 0.0 && fit.sigma_y > 0.0 {
        2.5 * fit.sigma_x.max(fit.sigma_y).max(1.0)
    } else {
        cluster_extent_radius(cluster)
    }
}

fn cluster_time_span(cluster: &EveCluster) -> (u64, u64) {
    let Some(first) = cluster.events.first() else {
        return (0, 0);
    };
    let mut start = first.timestamp;
    let mut end = first.timestamp;
    for event in &cluster.events[1..] {
        start = start.min(event.timestamp);
        end = end.max(event.timestamp);
    }
    (start, end.max(start))
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

    fn localization(x: f64, y: f64, timestamp_us: u64) -> EveLocalization {
        EveLocalization {
            cluster_id: timestamp_us,
            x,
            y,
            sigma_x: 0.7,
            sigma_y: 0.8,
            timestamp_us,
            span_start_us: timestamp_us.saturating_sub(1),
            span_end_us: timestamp_us.saturating_add(1),
            n_events: 7,
            polarity_balance: 0.1,
            fit_residual: 0.02,
            fit_method: FitMethod::LogGaussian,
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
            cluster_id: 0,
            pixel_histogram,
            events,
            centroid_x: if total > 0.0 { sum_x / total } else { 0.0 },
            centroid_y: if total > 0.0 { sum_y / total } else { 0.0 },
            x_min,
            x_max,
            y_min,
            y_max,
            complete: true,
            boundary: None,
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
    fn log_gaussian_recovers_synthetic_gaussian() {
        let x0: f64 = 6.3;
        let y0: f64 = 5.7;
        let sigma_x: f64 = 1.4;
        let sigma_y: f64 = 1.8;
        let amplitude: f64 = 50.0;
        let mut entries = Vec::new();
        for y in 1..11 {
            for x in 1..11 {
                let dx = f64::from(x) - x0;
                let dy = f64::from(y) - y0;
                let value = amplitude
                    * (-0.5 * (dx * dx / sigma_x.powi(2) + dy * dy / sigma_y.powi(2))).exp();
                let count = value.round().max(0.0) as u32;
                if count > 0 {
                    entries.push((x, y, count));
                }
            }
        }

        let fit = log_gaussian::fit(&cluster_from_histogram(&entries)).unwrap();
        assert!((fit.x - x0).abs() <= 0.3, "x: {} vs {}", fit.x, x0);
        assert!((fit.y - y0).abs() <= 0.3, "y: {} vs {}", fit.y, y0);
        assert!(
            (fit.sigma_x - sigma_x).abs() <= 0.3,
            "sigma_x: {} vs {}",
            fit.sigma_x,
            sigma_x
        );
        assert!(
            (fit.sigma_y - sigma_y).abs() <= 0.3,
            "sigma_y: {} vs {}",
            fit.sigma_y,
            sigma_y
        );
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
            cluster_id: 0,
            pixel_histogram: Vec::new(),
            events: Vec::new(),
            centroid_x: 0.0,
            centroid_y: 0.0,
            x_min: 0,
            x_max: 0,
            y_min: 0,
            y_max: 0,
            complete: true,
            boundary: None,
        };

        assert!(mean_xy::fit(&cluster).is_none());
        assert!(log_gaussian::fit(&cluster).is_none());
        assert!(gaussian::fit(&cluster).is_none());
    }

    #[test]
    fn host_view_registry_exposes_one_dataset_and_panel_view() {
        let registry = current_localizations_registry();

        assert_eq!(registry.datasets.len(), 1);
        assert_eq!(registry.views.len(), 2);
        assert_eq!(registry.datasets[0].id, CURRENT_LOCALIZATIONS_DATASET_ID);
        assert_eq!(registry.views[0].id, CURRENT_LOCALIZATIONS_VIEW_ID);
        assert_eq!(registry.views[1].id, CURRENT_LOCALIZATIONS_3D_VIEW_ID);
        assert!(registry.datasets[0].display.is_some());
    }

    #[test]
    fn host_view_dataset_is_columnar_and_aligned() {
        let dataset = current_localizations_dataset(&EveLocalizationResults {
            localizations: vec![localization(1.5, 2.5, 10)],
            frame_window_start_us: 0,
            frame_window_end_us: 50,
        });

        assert_eq!(dataset.row_count(), 1);
        assert_eq!(dataset.columns.len(), 13);
        assert_eq!(dataset.columns[0].column_id, "row_id");
        assert_eq!(dataset.columns[1].column_id, "cluster_id");
        assert_eq!(dataset.columns[4].column_id, "span_end_us");
        assert_eq!(dataset.columns[12].column_id, "fit_method");
    }

    #[test]
    fn current_localization_schema_exposes_linking_metadata() {
        let schema = current_localizations_schema_for_results(
            &EveLocalizationResults {
                localizations: vec![localization(12.0, 18.0, 15)],
                frame_window_start_us: 10,
                frame_window_end_us: 20,
            },
            Some((128, 64)),
        );
        assert_eq!(schema.row_id_column.as_deref(), Some("row_id"));
        assert_eq!(schema.time_column.as_deref(), Some("timestamp_us"));
        assert_eq!(
            schema
                .coordinate_space_3d
                .as_ref()
                .map(|space| space.z_column.as_str()),
            Some("timestamp_us")
        );
        assert_eq!(
            schema.layer_id.as_deref(),
            Some(CURRENT_LOCALIZATIONS_LAYER_ID)
        );
        let provenance = schema.provenance.as_ref().expect("provenance");
        assert_eq!(
            provenance.anchor_time_column.as_deref(),
            Some("timestamp_us")
        );
        assert_eq!(
            provenance.span_start_column.as_deref(),
            Some("span_start_us")
        );
        assert_eq!(provenance.span_end_column.as_deref(), Some("span_end_us"));
    }

    #[test]
    fn current_localization_registry_relates_rows_to_accepted_candidate_events() {
        let registry = current_localizations_registry_for_results(
            &EveLocalizationResults {
                localizations: vec![localization(12.0, 18.0, 15)],
                frame_window_start_us: 10,
                frame_window_end_us: 20,
            },
            Some((128, 64)),
        );
        let relations = &registry.datasets[0].relations;
        assert_eq!(relations.len(), 1);
        assert_eq!(
            relations[0].target_dataset_id,
            ACCEPTED_CANDIDATE_EVENTS_DATASET_ID
        );
        assert_eq!(relations[0].via_column, "cluster_id");
        assert_eq!(relations[0].target_column, "cluster_id");
    }

    #[test]
    fn current_localization_dataset_uses_repeatable_row_ids() {
        let dataset = current_localizations_dataset(&EveLocalizationResults {
            localizations: vec![localization(1.0, 2.0, 11), localization(1.0, 2.0, 11)],
            frame_window_start_us: 0,
            frame_window_end_us: 50,
        });
        let ids = match &dataset.column("row_id").expect("row id column").values {
            TableColumnValues::U64(values) => values.clone(),
            other => panic!("unexpected row id values: {other:?}"),
        };
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }

    #[test]
    fn rejected_fit_registry_exposes_dataset_table_and_3d_views() {
        let registry = rejected_fits_registry(
            &[RejectedFitRow {
                row_id: 1,
                cluster_id: 7,
                x: 10.5,
                y: 12.5,
                sigma_x: 0.0,
                sigma_y: 0.0,
                fit_residual: 0.0,
                n_events: 5,
                polarity_balance: 0.2,
                rejection_reason: RejectionReason::FitFailed,
                timestamp_us: 15,
                span_start_us: 10,
                span_end_us: 20,
            }],
            Some((128, 64)),
            10,
            20,
        );

        assert_eq!(registry.datasets.len(), 1);
        assert_eq!(registry.views.len(), 3);
        assert_eq!(registry.datasets[0].id, REJECTED_FITS_DATASET_ID);
        assert_eq!(registry.views[0].id, REJECTED_FITS_COMPACT_VIEW_ID);
        assert!(matches!(registry.views[0].kind, HostViewKind::CompactTable));
        assert_eq!(registry.views[1].id, REJECTED_FITS_TABLE_VIEW_ID);
        assert!(matches!(registry.views[1].kind, HostViewKind::TableWindow));
        assert_eq!(registry.views[2].id, REJECTED_FITS_3D_VIEW_ID);
        let schema = match &registry.datasets[0].kind {
            HostDatasetKind::TableV1(schema) => schema,
            other => panic!("unexpected dataset kind: {other:?}"),
        };
        assert_eq!(schema.row_id_column.as_deref(), Some("row_id"));
        assert_eq!(schema.layer_id.as_deref(), Some(REJECTED_FITS_LAYER_ID));
        let provenance = schema.provenance.as_ref().expect("provenance");
        assert_eq!(
            provenance.anchor_time_column.as_deref(),
            Some("timestamp_us")
        );
        assert_eq!(
            provenance.span_start_column.as_deref(),
            Some("span_start_us")
        );
        assert_eq!(provenance.span_end_column.as_deref(), Some("span_end_us"));
        assert_eq!(registry.datasets[0].relations.len(), 1);
        assert_eq!(
            registry.datasets[0].relations[0].target_dataset_id,
            ACCEPTED_CANDIDATE_EVENTS_DATASET_ID
        );
    }

    #[test]
    fn rejected_fit_dataset_is_columnar_and_repeatable() {
        let row = RejectedFitRow {
            row_id: 99,
            cluster_id: 5,
            x: 4.0,
            y: 6.0,
            sigma_x: 0.0,
            sigma_y: 0.0,
            fit_residual: 0.1,
            n_events: 8,
            polarity_balance: -0.25,
            rejection_reason: RejectionReason::ResidualTooHigh,
            timestamp_us: 22,
            span_start_us: 20,
            span_end_us: 30,
        };
        let dataset = rejected_fits_dataset(&[row.clone(), row]);

        assert_eq!(dataset.row_count(), 2);
        assert_eq!(dataset.columns.len(), 13);
        assert_eq!(dataset.columns[0].column_id, "row_id");
        assert_eq!(dataset.columns[12].column_id, "rejection_reason");
    }

    use std::ffi::c_void;

    use augur_plugin_api::{
        FfiColorRgba as TestFfiColorRgba, FfiMarkerOverlayItem as TestFfiMarkerOverlayItem,
        FfiOutputCallbacks, FfiPixel, FfiSlice, FfiString, FfiSubpixelMarker,
    };

    unsafe extern "C" fn noop_pixels(
        _ctx: *mut c_void,
        _pixels: FfiSlice<FfiPixel>,
        _color: TestFfiColorRgba,
    ) {
    }
    unsafe extern "C" fn noop_crosshairs(
        _ctx: *mut c_void,
        _markers: FfiSlice<FfiSubpixelMarker>,
        _color: TestFfiColorRgba,
        _arm: u16,
    ) {
    }
    unsafe extern "C" fn noop_marker_overlay(
        _ctx: *mut c_void,
        _markers: FfiSlice<TestFfiMarkerOverlayItem>,
        _dataset: FfiString,
        _layer: FfiString,
        _src: FfiString,
    ) {
    }
    unsafe extern "C" fn noop_warning(
        _ctx: *mut c_void,
        _source: FfiString,
        _severity: AnalysisSeverity,
        _message: FfiString,
    ) {
    }

    fn noop_output_callbacks() -> FfiOutputCallbacks {
        FfiOutputCallbacks {
            ctx: std::ptr::null_mut(),
            add_highlight_pixels: noop_pixels,
            add_crosshair_markers: noop_crosshairs,
            add_marker_overlay: noop_marker_overlay,
            add_warning: noop_warning,
        }
    }

    fn cluster_snapshot_params(
        cluster_id: u64,
        events: &[(u16, u16, bool, u64)],
        fit_method: FitMethod,
    ) -> Value {
        let rows = events
            .iter()
            .map(|(x, y, polarity, timestamp_us)| {
                json!({
                    "cluster_id": cluster_id,
                    "x_px": x,
                    "y_px": y,
                    "polarity": polarity,
                    "timestamp_us": timestamp_us,
                })
            })
            .collect();
        let mut params = serde_json::Map::new();
        params.insert("fit_method".into(), json!(fit_method.index() as u64));
        params.insert(HOST_ACTION_CLUSTER_ROWS_PARAM.into(), Value::Array(rows));
        Value::Object(params)
    }

    #[test]
    fn refit_preview_registry_uses_distinct_layer_and_dataset_ids() {
        let registry =
            refit_preview_registry_for_results(&EveLocalizationResults::default(), Some((64, 64)));
        assert_eq!(registry.datasets.len(), 1);
        assert_eq!(registry.datasets[0].id, REFIT_PREVIEW_DATASET_ID);
        assert_eq!(registry.views.len(), 1);
        assert_eq!(registry.views[0].id, REFIT_PREVIEW_VIEW_ID);
        let schema = match &registry.datasets[0].kind {
            HostDatasetKind::TableV1(schema) => schema,
            other => panic!("unexpected dataset kind: {other:?}"),
        };
        assert_eq!(schema.layer_id.as_deref(), Some(REFIT_PREVIEW_LAYER_ID));
        assert_eq!(schema.semantic_label.as_deref(), Some("refit preview"));
    }

    #[test]
    fn host_views_registers_three_actions_with_expected_scopes() {
        let plugin = EveSmlmFittingPlugin::default();
        let registry = plugin.host_views();

        assert_eq!(registry.actions.len(), 3);
        assert_eq!(registry.actions[0].id, ACTION_REFIT_CLUSTER);
        assert!(matches!(
            registry.actions[0].scope,
            HostActionScope::Cluster { ref dataset_id, ref group_column }
                if dataset_id == ACCEPTED_CANDIDATE_EVENTS_DATASET_ID
                    && group_column == "cluster_id"
        ));
        assert!(registry.actions[0].param_schema.is_some());

        assert_eq!(registry.actions[1].id, ACTION_COMMIT_REFIT);
        assert!(matches!(
            registry.actions[1].scope,
            HostActionScope::Row { ref dataset_id } if dataset_id == REFIT_PREVIEW_DATASET_ID
        ));
        assert!(registry.actions[1].param_schema.is_none());

        assert_eq!(registry.actions[2].id, ACTION_DISCARD_REFIT);
        assert!(matches!(
            registry.actions[2].scope,
            HostActionScope::Dataset { ref dataset_id } if dataset_id == REFIT_PREVIEW_DATASET_ID
        ));
    }

    #[test]
    fn refit_cluster_uses_snapshot_rows_when_current_frame_cluster_is_missing() {
        let mut plugin = EveSmlmFittingPlugin::default();
        let request = augur_plugin_api::HostActionRequest {
            request_id: 1,
            action_id: ACTION_REFIT_CLUSTER.into(),
            scope_payload: augur_plugin_api::HostActionScopePayload::Cluster {
                dataset_id: ACCEPTED_CANDIDATE_EVENTS_DATASET_ID.into(),
                group_column: "cluster_id".into(),
                group_value: "7".into(),
            },
            params: cluster_snapshot_params(
                7,
                &[(10, 20, true, 100), (12, 20, false, 130)],
                FitMethod::MeanXY,
            ),
        };

        let mut callbacks = noop_output_callbacks();
        let mut output = augur_plugin_api::HostOutput::new(&mut callbacks);
        plugin.handle_refit_cluster(&request, &mut output, None, 65.0);

        assert_eq!(plugin.refit_preview_results.localizations.len(), 1);
        let preview = &plugin.refit_preview_results.localizations[0];
        assert_eq!(preview.cluster_id, 7);
        assert!((preview.x - 11.0).abs() < 1e-6);
        assert!((preview.y - 20.0).abs() < 1e-6);
        assert_eq!(preview.n_events, 2);
        assert_eq!(preview.span_start_us, 100);
        assert_eq!(preview.span_end_us, 130);
        assert_eq!(plugin.refit_preview_replaces, vec![None]);
    }

    #[test]
    fn commit_persists_preview_into_host_results_even_without_current_frame_match() {
        let mut plugin = EveSmlmFittingPlugin::default();
        let preview = localization(3.5, 4.5, 200);
        let preview_row_id = localization_row_id(&preview);
        plugin.refit_preview_results.localizations.push(preview);
        plugin.refit_preview_replaces.push(None);
        plugin.host_rejected_fits.push(RejectedFitRow {
            row_id: 99,
            cluster_id: 200,
            x: 3.0,
            y: 4.0,
            sigma_x: 0.0,
            sigma_y: 0.0,
            fit_residual: 0.2,
            n_events: 6,
            polarity_balance: 0.1,
            rejection_reason: RejectionReason::ResidualTooHigh,
            timestamp_us: 180,
            span_start_us: 170,
            span_end_us: 210,
        });

        let request = augur_plugin_api::HostActionRequest {
            request_id: 1,
            action_id: ACTION_COMMIT_REFIT.into(),
            scope_payload: augur_plugin_api::HostActionScopePayload::Row {
                dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
                row_id: preview_row_id.to_string(),
            },
            params: serde_json::json!({}),
        };

        let mut callbacks = noop_output_callbacks();
        let mut output = augur_plugin_api::HostOutput::new(&mut callbacks);
        plugin.handle_commit_refit(&request, &mut output);

        assert!(plugin.refit_preview_results.localizations.is_empty());
        assert!(plugin.current_results.localizations.is_empty());
        assert_eq!(plugin.host_results.localizations.len(), 1);
        assert_eq!(plugin.host_results.localizations[0].cluster_id, 200);
        assert_eq!(plugin.host_results.localizations[0].x, 3.5);
        assert!(plugin.host_rejected_fits.is_empty());

        let dataset_bytes = plugin
            .host_view_dataset(CURRENT_LOCALIZATIONS_DATASET_ID)
            .expect("host dataset bytes");
        let dataset: TableDatasetV1 =
            serde_json::from_slice(&dataset_bytes).expect("table dataset should deserialize");
        assert_eq!(dataset.row_count(), 1);
    }

    #[test]
    fn commit_replaces_current_localization_when_cluster_matches_current_frame() {
        let mut plugin = EveSmlmFittingPlugin::default();
        let old = localization(1.0, 2.0, 100);
        plugin.current_results.localizations.push(old);

        let preview = localization(1.1, 2.1, 100);
        let preview_row_id = localization_row_id(&preview);
        plugin.refit_preview_results.localizations.push(preview);
        plugin.refit_preview_replaces.push(None);

        let request = augur_plugin_api::HostActionRequest {
            request_id: 1,
            action_id: ACTION_COMMIT_REFIT.into(),
            scope_payload: augur_plugin_api::HostActionScopePayload::Row {
                dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
                row_id: preview_row_id.to_string(),
            },
            params: serde_json::json!({}),
        };

        let mut callbacks = noop_output_callbacks();
        let mut output = augur_plugin_api::HostOutput::new(&mut callbacks);
        plugin.handle_commit_refit(&request, &mut output);

        assert_eq!(plugin.current_results.localizations.len(), 1);
        assert_eq!(plugin.current_results.localizations[0].x, 1.1);
        assert_eq!(plugin.current_results.localizations[0].y, 2.1);
        assert_eq!(plugin.host_results.localizations.len(), 1);
        assert_eq!(plugin.host_results.localizations[0].cluster_id, 100);
    }

    #[test]
    fn discard_clears_preview_without_touching_current_results() {
        let mut plugin = EveSmlmFittingPlugin::default();
        plugin
            .current_results
            .localizations
            .push(localization(1.0, 2.0, 100));
        let baseline = plugin.current_results.clone();

        plugin
            .refit_preview_results
            .localizations
            .push(localization(9.0, 9.0, 900));
        plugin.refit_preview_replaces.push(None);

        let request = augur_plugin_api::HostActionRequest {
            request_id: 1,
            action_id: ACTION_DISCARD_REFIT.into(),
            scope_payload: augur_plugin_api::HostActionScopePayload::Dataset {
                dataset_id: REFIT_PREVIEW_DATASET_ID.into(),
            },
            params: serde_json::json!({}),
        };

        let mut callbacks = noop_output_callbacks();
        let mut output = augur_plugin_api::HostOutput::new(&mut callbacks);
        plugin.handle_discard_refit(&request, &mut output);

        assert!(plugin.refit_preview_results.localizations.is_empty());
        assert!(plugin.refit_preview_replaces.is_empty());
        let baseline_bytes = serde_json::to_vec(&baseline).unwrap();
        let after_bytes = serde_json::to_vec(&plugin.current_results).unwrap();
        assert_eq!(baseline_bytes, after_bytes);
    }
}

export_plugin!(EveSmlmFittingPlugin);
