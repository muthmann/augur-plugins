use augur_plugin_api::{
    export_plugin, AnalysisSeverity, HostContext, HostOutput, Localization, LocalizationResults,
    LocalizationRow, LocalizationTable, Plugin, PluginFrame, PluginInput, SettingItem, SettingKind,
    SettingsSchema, SettingsSection, StatusEntry, CTX_LOCALIZATION_RESULTS,
};
use serde_json::{json, Value};

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
    table: Vec<LocalizationRow>,
    next_id: u64,
    frame_counter: u64,
    sensor_dims: Option<(u16, u16)>,
}

impl ReconstructionPlugin {
    fn parse_usize(value: Value) -> Option<usize> {
        value.as_u64().and_then(|value| usize::try_from(value).ok())
    }

    fn sigma_nm(&self, localization: &Localization) -> f64 {
        0.5 * (localization.sigma_x + localization.sigma_y) * self.settings.nm_per_pixel
    }

    fn xy_uncertainty_nm(&self, localization: &Localization) -> f64 {
        let sigma_mean_px = 0.5 * (localization.sigma_x + localization.sigma_y);
        let signal = localization.amplitude.abs().max(1.0).sqrt();
        sigma_mean_px / signal * self.settings.nm_per_pixel
    }

    fn next_frame_number(&mut self) -> u64 {
        self.frame_counter = self.frame_counter.saturating_add(1);
        self.frame_counter
    }

    fn trim_to_cap(&mut self) {
        if self.table.len() <= self.settings.max_localizations {
            return;
        }
        let overflow = self.table.len() - self.settings.max_localizations;
        self.table.drain(..overflow);
    }

    fn localization_row(
        &mut self,
        frame_number: u64,
        localization: &Localization,
    ) -> LocalizationRow {
        let row = LocalizationRow {
            id: self.next_id,
            frame: frame_number,
            x_nm: localization.x * self.settings.nm_per_pixel,
            y_nm: localization.y * self.settings.nm_per_pixel,
            sigma_nm: self.sigma_nm(localization),
            intensity: localization.amplitude,
            offset: localization.background,
            uncertainty_xy_nm: self.xy_uncertainty_nm(localization),
            timestamp_us: localization.timestamp_us,
        };
        self.next_id = self.next_id.saturating_add(1);
        row
    }

    fn accumulate_results(&mut self, frame_number: u64, results: &LocalizationResults) {
        for localization in &results.localizations {
            let row = self.localization_row(frame_number, localization);
            self.table.push(row);
        }
        self.trim_to_cap();
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
        self.table.clear();
        self.next_id = 0;
        self.frame_counter = 0;
        self.sensor_dims = None;
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::DerivedData
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
    ) {
        self.sensor_dims = Some((frame.width(), frame.height()));
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
                        key: "nm_per_pixel".into(),
                        label: "Scale".into(),
                        tooltip: Some(
                            "Pixel size used to export localization coordinates in nanometers.".into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 1.0,
                            max: 500.0,
                            speed: 0.5,
                            default: DEFAULT_NM_PER_PIXEL,
                        },
                    },
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
            "nm_per_pixel" => Some(json!(self.settings.nm_per_pixel)),
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
                let new_value = value.clamp(1.0, 500.0);
                let scale = new_value / self.settings.nm_per_pixel;
                if (scale - 1.0).abs() > f64::EPSILON {
                    for row in &mut self.table {
                        row.x_nm *= scale;
                        row.y_nm *= scale;
                        row.sigma_nm *= scale;
                        row.uncertainty_xy_nm *= scale;
                    }
                }
                self.settings.nm_per_pixel = new_value;
                Ok(())
            }
            "max_localizations" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("max_localizations must be an integer".into());
                };
                self.settings.max_localizations = value.clamp(10_000, 10_000_000);
                self.trim_to_cap();
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

    fn accumulated_localizations(&self) -> Option<Vec<u8>> {
        if self.table.is_empty() {
            return None;
        }
        let (sensor_width, sensor_height) = self.sensor_dims?;
        serde_json::to_vec(&LocalizationTable {
            rows: self.table.clone(),
            nm_per_pixel: self.settings.nm_per_pixel,
            sensor_width,
            sensor_height,
        })
        .ok()
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

    #[test]
    fn host_view_dataset_uses_the_same_rows_as_compatibility_table() {
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

        let compatibility = plugin
            .accumulated_localizations()
            .expect("compatibility payload");
        let compatibility: LocalizationTable =
            serde_json::from_slice(&compatibility).expect("compatibility table");
        let dataset = plugin.accumulated_dataset();

        assert_eq!(compatibility.rows.len(), dataset.row_count());
        assert_eq!(dataset.columns[0].column_id, "id");
        assert_eq!(dataset.columns[2].column_id, "x_nm");
    }
}
