use augur_plugin_api::{
    export_plugin, AnalysisSeverity, EventStoreHandle, GlobalSettings, HostContext, HostOutput,
    Plugin, PluginFrame, PluginInput, SettingItem, SettingKind, SettingsSchema, SettingsSection,
    StatusEntry, CTX_GLOBAL_SETTINGS,
};
use augur_plugin_types::{
    Localization, LocalizationResults, LocalizationRow, CTX_LOCALIZATION_RESULTS,
};
use serde_json::{json, Value};
use std::collections::VecDeque;

const DEFAULT_NM_PER_PIXEL: f64 = 65.0;
const DEFAULT_MAX_LOCALIZATIONS: usize = 1_000_000;
const ACCUMULATED_DATASET_ID: &str = "augur.localization.accumulated";
const LOCALIZATION_TABLE_VIEW_ID: &str = "augur.localization.accumulated.table";
const RECONSTRUCTION_VIEW_ID: &str = "augur.localization.accumulated.density";

#[derive(Debug, Clone)]
struct ReconstructionSettings {
    nm_per_pixel: f64,
    max_localizations: usize,
}

impl Default for ReconstructionSettings {
    fn default() -> Self {
        Self {
            nm_per_pixel: DEFAULT_NM_PER_PIXEL,
            max_localizations: DEFAULT_MAX_LOCALIZATIONS,
        }
    }
}

#[derive(Debug, Default)]
pub struct ReconstructionPlugin {
    enabled: bool,
    settings: ReconstructionSettings,
    table: VecDeque<LocalizationRow>,
    next_id: u64,
    frame_counter: u64,
    sensor_dims: Option<(u16, u16)>,
    dataset_generation: u64,
}

impl ReconstructionPlugin {
    fn parse_usize(value: Value) -> Option<usize> {
        value.as_u64().and_then(|value| usize::try_from(value).ok())
    }

    fn sigma_mean_px(localization: &Localization) -> f64 {
        0.5 * (localization.sigma_x + localization.sigma_y)
    }

    fn next_frame_number(&mut self) -> u64 {
        self.frame_counter = self.frame_counter.saturating_add(1);
        self.frame_counter
    }

    fn trim_to_cap(&mut self) {
        let overflow = self
            .table
            .len()
            .saturating_sub(self.settings.max_localizations);
        if overflow > 0 {
            self.table.drain(..overflow);
        }
    }

    fn bump_dataset_generation(&mut self) {
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
    }

    fn apply_nm_per_pixel(&mut self, value: f64) {
        let new_value = value.clamp(1.0, 500.0);
        let scale = new_value / self.settings.nm_per_pixel;
        if (scale - 1.0).abs() <= f64::EPSILON {
            self.settings.nm_per_pixel = new_value;
            return;
        }

        for row in &mut self.table {
            row.x_nm *= scale;
            row.y_nm *= scale;
            row.sigma_nm *= scale;
            row.uncertainty_xy_nm *= scale;
        }
        self.settings.nm_per_pixel = new_value;
        self.bump_dataset_generation();
    }

    fn sync_runtime_settings(&mut self, context: &HostContext<'_>, frame: &PluginFrame<'_>) {
        let globals = context
            .get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)
            .ok()
            .flatten();

        if let Some(globals) = globals {
            self.apply_nm_per_pixel(globals.nm_per_pixel);
            self.sensor_dims = Some((globals.sensor_width, globals.sensor_height));
        } else {
            self.sensor_dims = Some((frame.width(), frame.height()));
        }
    }

    fn localization_row(
        &mut self,
        frame_number: u64,
        localization: &Localization,
    ) -> LocalizationRow {
        let sigma_mean_px = Self::sigma_mean_px(localization);
        let nm_per_pixel = self.settings.nm_per_pixel;
        let row = LocalizationRow {
            id: self.next_id,
            frame: frame_number,
            x_nm: localization.x * nm_per_pixel,
            y_nm: localization.y * nm_per_pixel,
            sigma_nm: sigma_mean_px * nm_per_pixel,
            intensity: localization.amplitude,
            offset: localization.background,
            uncertainty_xy_nm: sigma_mean_px / localization.amplitude.abs().max(1.0).sqrt()
                * nm_per_pixel,
            timestamp_us: localization.timestamp_us,
        };
        self.next_id = self.next_id.saturating_add(1);
        row
    }

    fn accumulate_results(&mut self, frame_number: u64, results: &LocalizationResults) {
        if results.localizations.is_empty() {
            return;
        }
        self.table.reserve(results.localizations.len());
        for localization in &results.localizations {
            let row = self.localization_row(frame_number, localization);
            self.table.push_back(row);
        }
        self.trim_to_cap();
        self.bump_dataset_generation();
    }

    fn accumulated_coordinate_space(&self) -> Option<augur_plugin_api::TableCoordinateSpace2d> {
        let (sensor_width, sensor_height) = self.sensor_dims?;
        Some(augur_plugin_api::TableCoordinateSpace2d {
            x_column: "x_nm".into(),
            y_column: "y_nm".into(),
            x_min: 0.0,
            x_max: f64::from(sensor_width) * self.settings.nm_per_pixel,
            y_min: 0.0,
            y_max: f64::from(sensor_height) * self.settings.nm_per_pixel,
        })
    }

    fn accumulated_schema(&self) -> augur_plugin_api::TableSchema {
        augur_plugin_api::TableSchema {
            columns: vec![
                augur_plugin_api::TableColumn {
                    id: "id".into(),
                    title: "ID".into(),
                    value_type: augur_plugin_api::TableValueType::U64,
                },
                augur_plugin_api::TableColumn {
                    id: "frame".into(),
                    title: "Frame".into(),
                    value_type: augur_plugin_api::TableValueType::U64,
                },
                augur_plugin_api::TableColumn {
                    id: "x_nm".into(),
                    title: "X (nm)".into(),
                    value_type: augur_plugin_api::TableValueType::F64,
                },
                augur_plugin_api::TableColumn {
                    id: "y_nm".into(),
                    title: "Y (nm)".into(),
                    value_type: augur_plugin_api::TableValueType::F64,
                },
                augur_plugin_api::TableColumn {
                    id: "sigma_nm".into(),
                    title: "Sigma (nm)".into(),
                    value_type: augur_plugin_api::TableValueType::F64,
                },
                augur_plugin_api::TableColumn {
                    id: "intensity".into(),
                    title: "Intensity".into(),
                    value_type: augur_plugin_api::TableValueType::F64,
                },
                augur_plugin_api::TableColumn {
                    id: "offset".into(),
                    title: "Offset".into(),
                    value_type: augur_plugin_api::TableValueType::F64,
                },
                augur_plugin_api::TableColumn {
                    id: "uncertainty_xy_nm".into(),
                    title: "Uncertainty XY (nm)".into(),
                    value_type: augur_plugin_api::TableValueType::F64,
                },
                augur_plugin_api::TableColumn {
                    id: "timestamp_us".into(),
                    title: "Timestamp (us)".into(),
                    value_type: augur_plugin_api::TableValueType::U64,
                },
            ],
            coordinate_space_2d: self.accumulated_coordinate_space(),
        }
    }

    fn accumulated_dataset(&self) -> augur_plugin_api::TableDatasetV1 {
        augur_plugin_api::TableDatasetV1::new(vec![
            augur_plugin_api::TableColumnData {
                column_id: "id".into(),
                values: augur_plugin_api::TableColumnValues::U64(
                    self.table.iter().map(|row| row.id).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "frame".into(),
                values: augur_plugin_api::TableColumnValues::U64(
                    self.table.iter().map(|row| row.frame).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "x_nm".into(),
                values: augur_plugin_api::TableColumnValues::F64(
                    self.table.iter().map(|row| row.x_nm).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "y_nm".into(),
                values: augur_plugin_api::TableColumnValues::F64(
                    self.table.iter().map(|row| row.y_nm).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "sigma_nm".into(),
                values: augur_plugin_api::TableColumnValues::F64(
                    self.table.iter().map(|row| row.sigma_nm).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "intensity".into(),
                values: augur_plugin_api::TableColumnValues::F64(
                    self.table.iter().map(|row| row.intensity).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "offset".into(),
                values: augur_plugin_api::TableColumnValues::F64(
                    self.table.iter().map(|row| row.offset).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "uncertainty_xy_nm".into(),
                values: augur_plugin_api::TableColumnValues::F64(
                    self.table.iter().map(|row| row.uncertainty_xy_nm).collect(),
                ),
            },
            augur_plugin_api::TableColumnData {
                column_id: "timestamp_us".into(),
                values: augur_plugin_api::TableColumnValues::U64(
                    self.table.iter().map(|row| row.timestamp_us).collect(),
                ),
            },
        ])
        .expect("accumulated localization columns should stay aligned")
    }

    fn host_view_registry(&self) -> augur_plugin_api::HostViewRegistry {
        augur_plugin_api::HostViewRegistry {
            datasets: vec![augur_plugin_api::HostDatasetDescriptor {
                id: ACCUMULATED_DATASET_ID.into(),
                title: "Accumulated localizations".into(),
                kind: augur_plugin_api::HostDatasetKind::TableV1(self.accumulated_schema()),
                empty_message: "No accumulated localizations yet.".into(),
            }],
            views: vec![
                augur_plugin_api::HostViewDescriptor {
                    id: LOCALIZATION_TABLE_VIEW_ID.into(),
                    title: "Localization Table".into(),
                    dataset_id: ACCUMULATED_DATASET_ID.into(),
                    placement: augur_plugin_api::HostViewPlacement::Window,
                    kind: augur_plugin_api::HostViewKind::TableWindow,
                },
                augur_plugin_api::HostViewDescriptor {
                    id: RECONSTRUCTION_VIEW_ID.into(),
                    title: "Reconstruction".into(),
                    dataset_id: ACCUMULATED_DATASET_ID.into(),
                    placement: augur_plugin_api::HostViewPlacement::Window,
                    kind: augur_plugin_api::HostViewKind::Density2dFromTable {
                        x_column: "x_nm".into(),
                        y_column: "y_nm".into(),
                    },
                },
            ],
        }
    }
}

impl Plugin for ReconstructionPlugin {
    fn name(&self) -> &'static str {
        "Localization Reconstruction"
    }

    fn description(&self) -> &'static str {
        "Accumulates localization tables for host-side super-resolution rendering and export."
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
        self.table = VecDeque::new();
        self.next_id = 0;
        self.frame_counter = 0;
        self.sensor_dims = None;
        self.bump_dataset_generation();
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::DerivedData
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        self.sync_runtime_settings(context, frame);
        let frame_number = self.next_frame_number();

        match context.get::<LocalizationResults>(CTX_LOCALIZATION_RESULTS) {
            Ok(Some(results)) => self.accumulate_results(frame_number, &results),
            Ok(None) => {}
            Err(err) => output.add_warning(
                self.name(),
                AnalysisSeverity::Warning,
                &format!("Failed to decode localization results: {err}"),
            ),
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            sections: vec![SettingsSection {
                label: "Accumulation".into(),
                description: Some(
                    "Host-side reconstruction consumes the accumulated localization table exposed here."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "max_localizations".into(),
                        label: "Max localizations".into(),
                        tooltip: Some(
                            "Safety cap for the number of accumulated localizations retained in memory."
                                .into(),
                        ),
                        kind: SettingKind::I64Slider {
                            min: 10_000,
                            max: 10_000_000,
                            default: DEFAULT_MAX_LOCALIZATIONS as i64,
                            suffix: None,
                        },
                    },
                ],
            }],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "max_localizations" => Some(json!(self.settings.max_localizations)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "nm_per_pixel" => {
                let Some(value) = value.as_f64() else {
                    return Err("nm_per_pixel must be a number".into());
                };
                self.apply_nm_per_pixel(value);
                Ok(())
            }
            "max_localizations" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("max_localizations must be an integer".into());
                };
                self.settings.max_localizations = value.clamp(10_000, 10_000_000);
                self.trim_to_cap();
                self.table.shrink_to_fit();
                self.bump_dataset_generation();
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        vec![
            StatusEntry::LabeledValue {
                label: "Localizations".into(),
                value: self.table.len().to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Frame".into(),
                value: self.frame_counter.to_string(),
                color: None,
            },
        ]
    }

    fn host_views(&self) -> augur_plugin_api::HostViewRegistry {
        self.host_view_registry()
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        if dataset_id != ACCUMULATED_DATASET_ID {
            return None;
        }

        serde_json::to_vec(&self.accumulated_dataset()).ok()
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        if dataset_id == ACCUMULATED_DATASET_ID {
            self.dataset_generation
        } else {
            0
        }
    }
}

export_plugin!(ReconstructionPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn localization() -> Localization {
        Localization {
            x: 3.5,
            y: 4.0,
            sigma_x: 2.0,
            sigma_y: 2.0,
            amplitude: 100.0,
            background: 7.5,
            timestamp_us: 55,
            fit_error: 0.1,
        }
    }

    #[test]
    fn accumulation_converts_localizations_into_nm_rows() {
        let mut plugin = ReconstructionPlugin::default();
        plugin.sensor_dims = Some((1280, 720));

        plugin.accumulate_results(
            4,
            &LocalizationResults {
                localizations: vec![localization()],
                frame_window_start_us: 0,
                frame_window_end_us: 0,
            },
        );

        let row = &plugin.table[0];
        assert_eq!(row.id, 0);
        assert_eq!(row.frame, 4);
        assert!((row.x_nm - 227.5).abs() < 1e-6);
        assert!((row.y_nm - 260.0).abs() < 1e-6);
        assert!((row.sigma_nm - 130.0).abs() < 1e-6);
        assert!((row.uncertainty_xy_nm - 13.0).abs() < 1e-6);
        assert_eq!(row.intensity, 100.0);
        assert_eq!(row.offset, 7.5);
        assert_eq!(row.timestamp_us, 55);
    }

    #[test]
    fn accumulation_trims_oldest_rows_when_cap_is_exceeded() {
        let mut plugin = ReconstructionPlugin::default();
        plugin.settings.max_localizations = 1;

        plugin.accumulate_results(
            1,
            &LocalizationResults {
                localizations: vec![localization(), localization()],
                frame_window_start_us: 0,
                frame_window_end_us: 0,
            },
        );

        assert_eq!(plugin.table.len(), 1);
        assert_eq!(plugin.table[0].id, 1);
    }

    #[test]
    fn host_view_registry_exposes_one_dataset_and_two_window_views() {
        let mut plugin = ReconstructionPlugin::default();
        plugin.sensor_dims = Some((1280, 720));

        let registry = plugin.host_view_registry();

        assert_eq!(registry.datasets.len(), 1);
        assert_eq!(registry.views.len(), 2);
        assert_eq!(registry.datasets[0].id, ACCUMULATED_DATASET_ID);
        assert_eq!(registry.views[0].id, LOCALIZATION_TABLE_VIEW_ID);
        assert_eq!(registry.views[1].id, RECONSTRUCTION_VIEW_ID);
    }
}
