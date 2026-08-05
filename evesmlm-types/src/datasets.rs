//! The current-localization dataset, its schema and the host-view registry
//! built from it, plus the conversion to the standard `LocalizationResults`.
//!
//! These live here rather than in the fitting plugin because post-processing
//! republishes the same dataset — see the crate docs for why a plugin must not
//! link another plugin's rlib.

use augur_plugin_api::{
    HostDatasetDescriptor, HostDatasetDisplayMetadata, HostDatasetKind, HostDatasetRelation,
    HostMarkerShape, HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry,
    TableColumn, TableColumnData, TableColumnDisplayEntry, TableColumnDisplayFormat,
    TableColumnDisplayMetadata, TableColumnValues, TableCoordinateSpace2d, TableCoordinateSpace3d,
    TableDatasetV1, TableRowProvenance, TableSchema, TableValueType,
};
use augur_plugin_types::{Localization, LocalizationResults};

use crate::candidates::ACCEPTED_CANDIDATE_EVENTS_DATASET_ID;
use crate::localization::{EveLocalization, EveLocalizationResults};

pub const CURRENT_LOCALIZATIONS_DATASET_ID: &str = "augur.evesmlm.current_localizations";
pub const CURRENT_LOCALIZATIONS_LAYER_ID: &str = "augur.layer.evesmlm.current_localizations";
pub const CURRENT_LOCALIZATIONS_VIEW_ID: &str = "augur.evesmlm.current_localizations.compact";
pub const CURRENT_LOCALIZATIONS_3D_VIEW_ID: &str = "augur.evesmlm.current_localizations.scatter3d";

pub fn current_localizations_registry() -> HostViewRegistry {
    current_localizations_registry_for_results(&EveLocalizationResults::default(), None)
}

pub fn current_localizations_registry_for_results(
    results: &EveLocalizationResults,
    sensor_dims: Option<(u16, u16)>,
) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: CURRENT_LOCALIZATIONS_DATASET_ID.into(),
            title: "Current EVE localizations".into(),
            kind: HostDatasetKind::TableV1(current_localizations_schema_for_results(
                results,
                sensor_dims,
            )),
            empty_message: "No EVE localizations in the current frame.".into(),
            display: Some(HostDatasetDisplayMetadata {
                layer_title: Some("Current EVE localizations".into()),
                default_visibility: Some(true),
                default_color: Some([90, 170, 255, 255]),
                default_marker_shape: Some(HostMarkerShape::Cross),
                default_size: Some(6.0),
            }),
            relations: vec![HostDatasetRelation {
                target_dataset_id: ACCEPTED_CANDIDATE_EVENTS_DATASET_ID.into(),
                via_column: "cluster_id".into(),
                target_column: "cluster_id".into(),
            }],
        }],
        views: vec![
            HostViewDescriptor {
                id: CURRENT_LOCALIZATIONS_VIEW_ID.into(),
                title: "Current Localizations".into(),
                dataset_id: CURRENT_LOCALIZATIONS_DATASET_ID.into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            },
            HostViewDescriptor {
                id: CURRENT_LOCALIZATIONS_3D_VIEW_ID.into(),
                title: "Current Localizations 3D".into(),
                dataset_id: CURRENT_LOCALIZATIONS_DATASET_ID.into(),
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

pub fn current_localizations_schema() -> TableSchema {
    current_localizations_schema_for_results(&EveLocalizationResults::default(), None)
}

pub fn current_localizations_schema_for_results(
    results: &EveLocalizationResults,
    sensor_dims: Option<(u16, u16)>,
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
                id: "fit_residual".into(),
                title: "Fit residual".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "fit_method".into(),
                title: "Fit method".into(),
                value_type: TableValueType::String,
            },
        ],
        coordinate_space_2d: current_localizations_2d_space(results, sensor_dims),
        coordinate_space_3d: current_localizations_3d_space(results, sensor_dims),
        row_id_column: Some("row_id".into()),
        time_column: Some("timestamp_us".into()),
        layer_id: Some(CURRENT_LOCALIZATIONS_LAYER_ID.into()),
        semantic_label: Some("localizations".into()),
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
                column_id: "fit_method".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Category),
                    ..Default::default()
                },
            },
        ],
    }
}

pub fn current_localizations_dataset(results: &EveLocalizationResults) -> TableDatasetV1 {
    TableDatasetV1::new(vec![
        TableColumnData {
            column_id: "row_id".into(),
            values: TableColumnValues::U64(
                results
                    .localizations
                    .iter()
                    .map(localization_row_id)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "cluster_id".into(),
            values: TableColumnValues::U64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.cluster_id)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "timestamp_us".into(),
            values: TableColumnValues::U64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.timestamp_us)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "span_start_us".into(),
            values: TableColumnValues::U64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.span_start_us)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "span_end_us".into(),
            values: TableColumnValues::U64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.span_end_us)
                    .collect(),
            ),
        },
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
        TableColumnData {
            column_id: "polarity_balance".into(),
            values: TableColumnValues::F64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.polarity_balance)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "fit_residual".into(),
            values: TableColumnValues::F64(
                results
                    .localizations
                    .iter()
                    .map(|value| value.fit_residual)
                    .collect(),
            ),
        },
        TableColumnData {
            column_id: "fit_method".into(),
            values: TableColumnValues::String(
                results
                    .localizations
                    .iter()
                    .map(|value| value.fit_method.label().to_owned())
                    .collect(),
            ),
        },
    ])
    .expect("current localization columns should stay aligned")
}

fn current_localizations_2d_space(
    results: &EveLocalizationResults,
    sensor_dims: Option<(u16, u16)>,
) -> Option<TableCoordinateSpace2d> {
    sensor_dims
        .map(|(width, height)| (0.0, f64::from(width), 0.0, f64::from(height)))
        .or_else(|| localization_xy_bounds(results))
        .map(|(x_min, x_max, y_min, y_max)| TableCoordinateSpace2d {
            x_column: "x_px".into(),
            y_column: "y_px".into(),
            x_min,
            x_max,
            y_min,
            y_max,
        })
}

fn current_localizations_3d_space(
    results: &EveLocalizationResults,
    sensor_dims: Option<(u16, u16)>,
) -> Option<TableCoordinateSpace3d> {
    let (x_min, x_max, y_min, y_max) = sensor_dims
        .map(|(width, height)| (0.0, f64::from(width), 0.0, f64::from(height)))
        .or_else(|| localization_xy_bounds(results))?;
    let (z_min, z_max) = localization_time_bounds(results)?;
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
pub fn localization_xy_bounds(results: &EveLocalizationResults) -> Option<(f64, f64, f64, f64)> {
    let mut localizations = results.localizations.iter();
    let first = localizations.next()?;
    let mut x_min = first.x;
    let mut x_max = first.x;
    let mut y_min = first.y;
    let mut y_max = first.y;
    for localization in localizations {
        x_min = x_min.min(localization.x);
        x_max = x_max.max(localization.x);
        y_min = y_min.min(localization.y);
        y_max = y_max.max(localization.y);
    }
    Some((x_min, x_max.max(x_min), y_min, y_max.max(y_min)))
}

pub fn localization_time_bounds(results: &EveLocalizationResults) -> Option<(f64, f64)> {
    if let Some(first) = results.localizations.first() {
        let mut min_time = first.timestamp_us;
        let mut max_time = first.timestamp_us;
        for localization in &results.localizations {
            min_time = min_time.min(localization.timestamp_us);
            max_time = max_time.max(localization.timestamp_us);
        }
        return Some((min_time as f64, max_time.max(min_time) as f64));
    }

    if results.frame_window_end_us >= results.frame_window_start_us {
        return Some((
            results.frame_window_start_us as f64,
            results.frame_window_end_us as f64,
        ));
    }

    None
}

pub fn localization_row_id(localization: &EveLocalization) -> u64 {
    localization.cluster_id.rotate_left(3)
        ^ localization.timestamp_us
        ^ localization.x.to_bits().rotate_left(7)
        ^ localization.y.to_bits().rotate_left(19)
        ^ localization.sigma_x.to_bits().rotate_left(31)
        ^ localization.sigma_y.to_bits().rotate_left(43)
        ^ localization.fit_residual.to_bits().rotate_left(53)
        ^ (localization.n_events as u64).rotate_left(11)
        ^ (localization.fit_method.index() as u64).rotate_left(59)
        ^ localization.span_start_us.rotate_left(17)
        ^ localization.span_end_us.rotate_left(29)
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
