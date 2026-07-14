//! Stage-A commissioning monitor.
//!
//! Live view of the Teensy photodiode DAQ (decimated waveform, calibrated
//! optical log-contrast `a`, clipping/headroom and stream-integrity status)
//! plus **gated** manual controller commands (connect, configure, start,
//! stop) for wiring and crosstalk commissioning.
//!
//! Safety contract (Stage-A control-software spec):
//! - devices open only when `HostContext::execution()` reports
//!   `LiveCapture` **and** `effects_allowed` — replay and offline analysis
//!   can never touch the serial port, and a stale worker is shut down the
//!   moment the context stops permitting effects;
//! - commands are host actions, never persistent settings, so a reloaded
//!   settings file cannot re-arm hardware;
//! - `process_frame()` only drains the bounded I/O worker queues.

use std::collections::BTreeMap;

use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostActionDescriptor, HostActionRequestQueue, HostActionScope,
    HostContext, HostDatasetDescriptor, HostDatasetKind, HostOutput, HostViewDescriptor,
    HostViewKind, HostViewPlacement, HostViewRegistry, Plugin, PluginFrame, Series1dLine,
    Series1dPoint, Series1dV1, SettingItem, SettingKind, SettingsSchema, SettingsSection,
    StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1, TableSchema,
    TableValueType, CTX_INVESTIGATION_ACTION_REQUESTS,
};
use serde_json::{json, Value};
use stage_a_io::{
    estimate_contrast, AdcCalibration, Command, ContrastEstimate, DeviceEvent, FrameType, IoWorker,
    StageAClient, StreamIntegrity, WorkerOutput, WorkerRequest,
};

const WAVEFORM_DATASET_ID: &str = "stage-a-monitor.waveform";
const STATUS_DATASET_ID: &str = "stage-a-monitor.status";
const WAVEFORM_VIEW_ID: &str = "stage-a-monitor.waveform.view";
const STATUS_VIEW_ID: &str = "stage-a-monitor.status.view";

const ACTION_CONNECT: &str = "stage-a-monitor.connect";
const ACTION_DISCONNECT: &str = "stage-a-monitor.disconnect";
const ACTION_START: &str = "stage-a-monitor.start";
const ACTION_STOP: &str = "stage-a-monitor.stop";
const ACTION_APPLY_DRIVE: &str = "stage-a-monitor.apply-drive";

/// Retained sample window for the live view + contrast estimate.
const SAMPLE_RING_CAPACITY: usize = 32_768;
/// Points published per waveform refresh (decimated).
const WAVEFORM_POINTS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Disconnected,
    Connected,
    Acquiring,
}

pub struct StageAMonitorPlugin {
    enabled: bool,
    // -- device --
    worker: Option<IoWorker>,
    connection: ConnectionState,
    firmware: String,
    next_tag: u64,
    /// Tags of in-flight requests -> human-readable purpose.
    in_flight: BTreeMap<u64, String>,
    last_error: Option<String>,
    integrity: StreamIntegrity,
    effects_blocked_reason: Option<String>,
    // -- settings --
    port_hint: String,
    sample_rate_hz: i64,
    block_samples: i64,
    calibration: AdcCalibration,
    // -- data --
    sample_ring: Vec<u16>,
    ring_next_sample_index: u64,
    sample_rate_seen_hz: u32,
    contrast: Option<ContrastEstimate>,
    contrast_error: Option<String>,
    dataset_generation: u64,
    consumed_action_ids: Vec<u64>,
}

impl Default for StageAMonitorPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            worker: None,
            connection: ConnectionState::Disconnected,
            firmware: String::new(),
            next_tag: 1,
            in_flight: BTreeMap::new(),
            last_error: None,
            integrity: StreamIntegrity::default(),
            effects_blocked_reason: None,
            port_hint: "auto".into(),
            sample_rate_hz: 20_000,
            block_samples: 256,
            calibration: AdcCalibration::default(),
            sample_ring: Vec::with_capacity(SAMPLE_RING_CAPACITY),
            ring_next_sample_index: 0,
            sample_rate_seen_hz: 0,
            contrast: None,
            contrast_error: None,
            dataset_generation: 0,
            consumed_action_ids: Vec::new(),
        }
    }
}

impl StageAMonitorPlugin {
    fn bump_generation(&mut self) {
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
    }

    fn queue_command(&mut self, purpose: &str, command: Command) {
        let Some(worker) = &self.worker else {
            self.last_error = Some(format!("{purpose}: no device connection"));
            return;
        };
        let tag = self.next_tag;
        self.next_tag += 1;
        match worker.try_send(WorkerRequest::Send { tag, command }) {
            Ok(()) => {
                self.in_flight.insert(tag, purpose.to_owned());
            }
            Err(err) => self.last_error = Some(format!("{purpose}: {err}")),
        }
    }

    fn connect(&mut self) {
        if self.worker.is_some() {
            return;
        }
        match open_transport(&self.port_hint) {
            Ok(client) => {
                self.worker = Some(IoWorker::spawn(client));
                self.last_error = None;
                self.queue_command("hello", Command::new("HELLO").field("protocol", 1));
            }
            Err(err) => {
                self.last_error = Some(err);
            }
        }
        self.bump_generation();
    }

    fn disconnect(&mut self, reason: &str) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown(reason);
        }
        self.connection = ConnectionState::Disconnected;
        self.in_flight.clear();
        self.bump_generation();
    }

    fn start_acquisition(&mut self) {
        self.queue_command(
            "config",
            Command::new("CONFIG")
                .field("mode", "A1")
                .field("rate_hz", self.sample_rate_hz)
                .field("block_samples", self.block_samples)
                .field("raw", 1)
                .field("summary", 1),
        );
        self.queue_command("start", Command::new("START"));
        if let Some(worker) = &self.worker {
            let _ = worker.try_send(WorkerRequest::SetPinging(true));
        }
    }

    fn stop_acquisition(&mut self) {
        self.queue_command("stop", Command::new("STOP").field("reason", "operator"));
        if let Some(worker) = &self.worker {
            let _ = worker.try_send(WorkerRequest::SetPinging(false));
        }
    }

    fn apply_drive(&mut self, params: &Value) {
        let freq_mhz = params.get("freq_mhz").and_then(Value::as_i64).unwrap_or(0);
        let center_dac = params
            .get("center_dac")
            .and_then(Value::as_i64)
            .unwrap_or(2_048);
        let amplitude_dac = params
            .get("amplitude_dac")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        self.queue_command(
            "drive",
            Command::new("CONFIG")
                .field("mode", "A1")
                .field("wave", "SINE")
                .field("freq_mhz", freq_mhz)
                .field("center_dac", center_dac)
                .field("amplitude_dac", amplitude_dac)
                .field("rate_hz", self.sample_rate_hz)
                .field("block_samples", self.block_samples)
                .field("raw", 1)
                .field("summary", 1),
        );
    }

    fn drain_worker(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        let outputs = worker.drain_outputs();
        if outputs.is_empty() {
            return;
        }
        let mut changed = false;
        let mut stopped: Option<String> = None;
        for output in outputs {
            changed = true;
            match output {
                WorkerOutput::Reply { tag, result } => {
                    let purpose = self.in_flight.remove(&tag).unwrap_or_default();
                    match result {
                        Ok(fields) => self.handle_reply(&purpose, &fields),
                        Err(err) if err.contains("unknown_config_field") => {
                            // Feature detection: firmware 0.2.0 has no
                            // waveform backend and rejects the reserved v2
                            // drive fields.
                            self.last_error = Some(format!(
                                "{purpose}: firmware has no waveform backend (v1) — drive \
                                 control needs the mock or the future v2 firmware"
                            ));
                        }
                        Err(err) => {
                            self.last_error = Some(format!("{purpose}: {err}"));
                        }
                    }
                }
                WorkerOutput::Event(DeviceEvent::Data(frame)) => match frame.header.frame_type {
                    FrameType::SamplesU16 => {
                        if let Some(codes) = frame.samples() {
                            self.sample_rate_seen_hz = frame.header.sample_rate_hz;
                            self.push_samples(&codes, frame.header.first_sample_index);
                        }
                    }
                    FrameType::Summary | FrameType::Marker | FrameType::Control => {}
                    FrameType::Unknown(_) => {}
                },
                WorkerOutput::Event(DeviceEvent::Async { name, fields }) => {
                    if name == "FAULT" {
                        // Firmware watchdog dropped the controller to
                        // SAFE_IDLE — reflect it instead of showing a stale
                        // "acquiring" state.
                        if self.connection == ConnectionState::Acquiring {
                            self.connection = ConnectionState::Connected;
                        }
                        self.last_error = Some(format!(
                            "controller fault: {} — dropped to SAFE_IDLE",
                            fields.get("code").map(String::as_str).unwrap_or("unknown")
                        ));
                        if let Some(worker) = &self.worker {
                            let _ = worker.try_send(WorkerRequest::SetPinging(false));
                        }
                    }
                }
                WorkerOutput::Integrity(integrity) => {
                    self.integrity = integrity;
                }
                WorkerOutput::Stopped { reason } => {
                    stopped = Some(reason);
                }
            }
        }
        if let Some(reason) = stopped {
            self.worker = None;
            self.connection = ConnectionState::Disconnected;
            self.last_error = Some(format!("device connection ended: {reason}"));
        }
        if changed {
            self.refresh_contrast();
            self.bump_generation();
        }
    }

    fn handle_reply(&mut self, purpose: &str, fields: &BTreeMap<String, String>) {
        match purpose {
            "hello" => {
                self.firmware = fields
                    .get("firmware")
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                self.connection = ConnectionState::Connected;
            }
            "start" => {
                self.connection = ConnectionState::Acquiring;
            }
            "stop" => {
                self.connection = ConnectionState::Connected;
            }
            _ => {}
        }
    }

    fn push_samples(&mut self, codes: &[u16], first_sample_index: u64) {
        self.ring_next_sample_index = first_sample_index + codes.len() as u64;
        self.sample_ring.extend_from_slice(codes);
        let len = self.sample_ring.len();
        if len > SAMPLE_RING_CAPACITY {
            self.sample_ring.drain(..len - SAMPLE_RING_CAPACITY);
        }
    }

    fn refresh_contrast(&mut self) {
        if self.sample_ring.len() < stage_a_io::estimator::MIN_SAMPLES {
            return;
        }
        match estimate_contrast(&self.sample_ring, &self.calibration) {
            Ok(estimate) => {
                self.contrast = Some(estimate);
                self.contrast_error = None;
            }
            Err(err) => {
                self.contrast = None;
                self.contrast_error = Some(err.to_string());
            }
        }
    }

    fn waveform_dataset(&self) -> Series1dV1 {
        let rate = if self.sample_rate_seen_hz > 0 {
            f64::from(self.sample_rate_seen_hz)
        } else {
            self.sample_rate_hz as f64
        };
        let n = self.sample_ring.len();
        let stride = (n / WAVEFORM_POINTS).max(1);
        let first_index = self.ring_next_sample_index.saturating_sub(n as u64);
        let points: Vec<Series1dPoint> = self
            .sample_ring
            .iter()
            .enumerate()
            .step_by(stride)
            .map(|(i, &code)| Series1dPoint {
                x: (first_index + i as u64) as f64 / rate * 1_000.0,
                y: self.calibration.code_to_volts(code),
            })
            .collect();
        Series1dV1 {
            x_label: "time [ms]".into(),
            y_label: "photodiode [V]".into(),
            lines: vec![Series1dLine {
                name: "photodiode".into(),
                points,
            }],
        }
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let state = match (&self.effects_blocked_reason, self.connection) {
            (Some(reason), _) => format!("locked ({reason})"),
            (None, ConnectionState::Disconnected) => "disconnected".into(),
            (None, ConnectionState::Connected) => "connected".into(),
            (None, ConnectionState::Acquiring) => "acquiring".into(),
        };
        let (a_text, clip_text) = match (&self.contrast, &self.contrast_error) {
            (Some(estimate), _) => (
                format!("{:.4}", estimate.a),
                format!(
                    "{:.2}% low / {:.2}% high",
                    estimate.low_clip_fraction * 100.0,
                    estimate.high_clip_fraction * 100.0
                ),
            ),
            (None, Some(err)) => ("invalid".into(), err.clone()),
            (None, None) => ("—".into(), "—".into()),
        };
        let integrity = if self.integrity.is_clean() {
            "clean".to_owned()
        } else {
            format!(
                "crc={} gaps={} skipped={} overruns={}",
                self.integrity.crc_failures,
                self.integrity.sequence_gaps,
                self.integrity.skipped_bytes,
                self.integrity.dropped_samples
            )
        };
        let text_column = |id: &str, value: String| TableColumnData {
            column_id: id.to_owned(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                text_column("state", state),
                text_column("firmware", self.firmware.clone()),
                text_column("a", a_text),
                text_column("clipping", clip_text),
                text_column("integrity", integrity),
                text_column("error", self.last_error.clone().unwrap_or_default()),
            ],
        }
    }

    fn status_schema(&self) -> TableSchema {
        let column = |id: &str, title: &str| TableColumn {
            id: id.to_owned(),
            title: title.to_owned(),
            value_type: TableValueType::String,
        };
        TableSchema {
            columns: vec![
                column("state", "State"),
                column("firmware", "Firmware"),
                column("a", "a = ln(Vmax/Vmin)"),
                column("clipping", "Clipping"),
                column("integrity", "Stream integrity"),
                column("error", "Last error"),
            ],
            ..TableSchema::default()
        }
    }

    fn consume_actions(&mut self, context: &HostContext<'_>) -> Vec<(String, Value)> {
        let Ok(Some(queue)) =
            context.get::<HostActionRequestQueue>(CTX_INVESTIGATION_ACTION_REQUESTS)
        else {
            return Vec::new();
        };
        let mut consumed = Vec::new();
        for request in queue.requests {
            if self.consumed_action_ids.contains(&request.request_id) {
                continue;
            }
            if !request.action_id.starts_with("stage-a-monitor.") {
                continue;
            }
            self.consumed_action_ids.push(request.request_id);
            if self.consumed_action_ids.len() > 256 {
                self.consumed_action_ids.remove(0);
            }
            consumed.push((request.action_id, request.params));
        }
        consumed
    }
}

fn open_transport(port_hint: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let path = resolve_port(port_hint)?;
    let transport =
        stage_a_io::SerialTransport::open(&path, 115_200, std::time::Duration::from_millis(20))
            .map_err(|err| err.to_string())?;
    Ok(StageAClient::new(transport))
}

fn resolve_port(port_hint: &str) -> Result<String, String> {
    if port_hint != "auto" {
        return Ok(port_hint.to_owned());
    }
    let ports = serial_ports();
    ports
        .into_iter()
        .next()
        .ok_or_else(|| "no USB serial device found (looked for usbmodem/ttyACM)".to_owned())
}

fn serial_ports() -> Vec<String> {
    serialport_names()
        .into_iter()
        .filter(|name| name.contains("usbmodem") || name.contains("ttyACM"))
        .collect()
}

fn serialport_names() -> Vec<String> {
    stage_a_io::transport::available_port_names()
}

impl Plugin for StageAMonitorPlugin {
    fn name(&self) -> &'static str {
        "Stage-A Monitor"
    }

    fn description(&self) -> &'static str {
        "Live Teensy photodiode readout with calibrated optical contrast and gated manual drive control (commissioning)."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.disconnect("plugin disabled");
        }
    }

    fn reset(&mut self) {
        self.sample_ring.clear();
        self.contrast = None;
        self.contrast_error = None;
        self.bump_generation();
    }

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        // Fail closed: any pass without live-capture effects tears the
        // connection down and refuses commands.
        let execution = context.execution();
        if !execution.hardware_effects_allowed() {
            self.effects_blocked_reason = Some(format!(
                "hardware effects not allowed in {:?}",
                execution.mode
            ));
            if self.worker.is_some() {
                self.disconnect("execution context revoked effects");
            }
            return;
        }
        self.effects_blocked_reason = None;

        for (action_id, params) in self.consume_actions(context) {
            match action_id.as_str() {
                ACTION_CONNECT => self.connect(),
                ACTION_DISCONNECT => self.disconnect("operator"),
                ACTION_START => self.start_acquisition(),
                ACTION_STOP => self.stop_acquisition(),
                ACTION_APPLY_DRIVE => self.apply_drive(&params),
                _ => {}
            }
        }

        self.drain_worker();
    }

    fn settings_schema(&self) -> SettingsSchema {
        let mut port_variants = vec!["auto".to_owned()];
        port_variants.extend(serial_ports());
        let port_default = port_variants
            .iter()
            .position(|p| *p == self.port_hint)
            .unwrap_or(0);
        SettingsSchema {
            sections: vec![SettingsSection {
                label: "Device".into(),
                description: Some(
                    "Serial DAQ configuration. Connect/start/stop are actions on the status \
                     table, never settings — a reloaded settings file can't arm hardware."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "port".into(),
                        label: "Serial port".into(),
                        tooltip: Some("Teensy USB serial device (auto = first usbmodem)".into()),
                        kind: SettingKind::Enum {
                            variants: port_variants,
                            default: port_default,
                        },
                    },
                    SettingItem {
                        key: "sample_rate_hz".into(),
                        label: "ADC sample rate".into(),
                        tooltip: Some("Commanded photodiode sample rate".into()),
                        kind: SettingKind::I64Slider {
                            min: 1_000,
                            max: 100_000,
                            default: self.sample_rate_hz,
                            suffix: Some(" Hz".into()),
                        },
                    },
                    SettingItem {
                        key: "dark_millivolts".into(),
                        label: "Dark level".into(),
                        tooltip: Some(
                            "Light-blocked photodiode level; a is computed from dark-corrected \
                             voltages"
                                .into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 0.0,
                            max: 3_300.0,
                            speed: 1.0,
                            default: self.calibration.dark_volts * 1_000.0,
                        },
                    },
                ],
            }],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "port" => Some(json!(self.port_hint)),
            "sample_rate_hz" => Some(json!(self.sample_rate_hz)),
            "dark_millivolts" => Some(json!(self.calibration.dark_volts * 1_000.0)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "port" => {
                self.port_hint = value.as_str().ok_or("port must be a string")?.to_owned();
                Ok(())
            }
            "sample_rate_hz" => {
                self.sample_rate_hz = value
                    .as_i64()
                    .ok_or("sample_rate_hz must be an integer")?
                    .clamp(1_000, 100_000);
                Ok(())
            }
            "dark_millivolts" => {
                let mv = value.as_f64().ok_or("dark_millivolts must be a number")?;
                self.calibration.dark_volts = (mv / 1_000.0).clamp(0.0, 3.3);
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        if let Some(reason) = &self.effects_blocked_reason {
            entries.push(StatusEntry::Text(format!("Hardware locked: {reason}")));
        }
        entries.push(StatusEntry::Text(match self.connection {
            ConnectionState::Disconnected => "Teensy: disconnected".into(),
            ConnectionState::Connected => format!("Teensy: connected ({})", self.firmware),
            ConnectionState::Acquiring => format!(
                "Teensy: acquiring at {} S/s",
                if self.sample_rate_seen_hz > 0 {
                    self.sample_rate_seen_hz as i64
                } else {
                    self.sample_rate_hz
                }
            ),
        }));
        if let Some(estimate) = &self.contrast {
            entries.push(StatusEntry::Text(format!("a = {:.4}", estimate.a)));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        HostViewRegistry {
            datasets: vec![
                HostDatasetDescriptor {
                    id: WAVEFORM_DATASET_ID.into(),
                    title: "Photodiode waveform".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "No photodiode samples yet — connect and start.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: STATUS_DATASET_ID.into(),
                    title: "Stage-A monitor status".into(),
                    kind: HostDatasetKind::TableV1(self.status_schema()),
                    empty_message: "Monitor idle.".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: WAVEFORM_VIEW_ID.into(),
                    title: "Photodiode".into(),
                    dataset_id: WAVEFORM_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: STATUS_VIEW_ID.into(),
                    title: "Monitor status".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
            ],
            actions: vec![
                HostActionDescriptor {
                    id: ACTION_CONNECT.into(),
                    title: "Connect".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
                HostActionDescriptor {
                    id: ACTION_DISCONNECT.into(),
                    title: "Disconnect".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
                HostActionDescriptor {
                    id: ACTION_START.into(),
                    title: "Start acquisition".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
                HostActionDescriptor {
                    id: ACTION_STOP.into(),
                    title: "Stop".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
                HostActionDescriptor {
                    id: ACTION_APPLY_DRIVE.into(),
                    title: "Apply drive (expert)".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: serde_json::to_value(SettingsSchema {
                        sections: vec![SettingsSection {
                            label: "Drive".into(),
                            description: Some(
                                "Integer DAC drive codes — the optical contrast is measured \
                                 from the photodiode, never assumed from these values."
                                    .into(),
                            ),
                            default_open: true,
                            items: vec![
                                SettingItem {
                                    key: "freq_mhz".into(),
                                    label: "Frequency".into(),
                                    tooltip: Some("Drive frequency in millihertz".into()),
                                    kind: SettingKind::I64Drag {
                                        min: 0,
                                        max: 200_000_000,
                                        default: 1_000_000,
                                    },
                                },
                                SettingItem {
                                    key: "center_dac".into(),
                                    label: "Center DAC code".into(),
                                    tooltip: None,
                                    kind: SettingKind::I64Drag {
                                        min: 0,
                                        max: 4_095,
                                        default: 2_048,
                                    },
                                },
                                SettingItem {
                                    key: "amplitude_dac".into(),
                                    label: "Amplitude DAC code".into(),
                                    tooltip: None,
                                    kind: SettingKind::I64Drag {
                                        min: 0,
                                        max: 2_047,
                                        default: 0,
                                    },
                                },
                            ],
                        }],
                    })
                    .ok(),
                },
            ],
        }
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            WAVEFORM_DATASET_ID => serde_json::to_vec(&self.waveform_dataset()).ok(),
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            WAVEFORM_DATASET_ID | STATUS_DATASET_ID => self.dataset_generation.max(1),
            _ => 0,
        }
    }
}

impl Drop for StageAMonitorPlugin {
    fn drop(&mut self) {
        self.disconnect("plugin destroyed");
    }
}

export_plugin!(StageAMonitorPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waveform_dataset_decimates_and_calibrates() {
        let mut plugin = StageAMonitorPlugin::default();
        plugin.sample_rate_seen_hz = 20_000;
        plugin.push_samples(&vec![2_048_u16; 8_192], 0);
        let dataset = plugin.waveform_dataset();
        assert_eq!(dataset.lines.len(), 1);
        assert!(dataset.lines[0].points.len() <= WAVEFORM_POINTS + 1);
        let volts = dataset.lines[0].points[0].y;
        assert!((volts - 2_048.0 * 3.3 / 4_095.0).abs() < 1e-9);
    }

    #[test]
    fn sample_ring_is_bounded() {
        let mut plugin = StageAMonitorPlugin::default();
        plugin.push_samples(&vec![1_u16; SAMPLE_RING_CAPACITY], 0);
        plugin.push_samples(&vec![2_u16; 4_096], SAMPLE_RING_CAPACITY as u64);
        assert_eq!(plugin.sample_ring.len(), SAMPLE_RING_CAPACITY);
        assert_eq!(*plugin.sample_ring.last().unwrap(), 2);
    }

    #[test]
    fn status_dataset_matches_its_schema() {
        let plugin = StageAMonitorPlugin::default();
        let dataset = plugin.status_dataset();
        let schema = plugin.status_schema();
        assert_eq!(dataset.columns.len(), schema.columns.len());
        for (data, column) in dataset.columns.iter().zip(&schema.columns) {
            assert_eq!(data.column_id, column.id);
            assert_eq!(data.len(), 1);
        }
    }
}
