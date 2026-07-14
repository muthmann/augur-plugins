//! Stage-A function generator — familiarisation plugin.
//!
//! Manual control of the Pockels-cell drive: waveform (sine, square,
//! sawtooth), frequency, and the commanded DAC modulation depth
//! (`amplitude_dac`). The commanded amplitude sets the *phase* modulation
//! of the Pockels cell, which maps non-linearly to transmitted intensity —
//! so the optical amplitude shown here is always the photodiode-measured
//! log-contrast `a = ln(V_max/V_min)`, never the DAC excursion.
//!
//! Firmware 0.2.0 has no waveform backend yet: it rejects the reserved v2
//! drive fields with `unknown_config_field` (the feature-detection
//! contract, `stage-a-controller/docs/features/waveform-drive.md`). Until
//! the DDS firmware lands, select the **`mock`** port: it runs the
//! waveform-extended mock controller in-process and streams a synthetic
//! photodiode response through a Pockels-like sin² transfer — the full
//! control loop with zero hardware and zero risk.
//!
//! Safety contract (same as `stage-a-monitor`):
//! - devices open only when the execution context is `LiveCapture` with
//!   `effects_allowed`; anything else tears the connection down;
//! - drive parameters are persistent *settings*, but nothing starts the
//!   hardware except an explicit Apply **action**;
//! - `process_frame()` only drains the bounded I/O worker queues.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    MockController, MockState, StageAClient, StreamIntegrity, WorkerOutput, WorkerRequest,
};

const WAVEFORM_DATASET_ID: &str = "stage-a-funcgen.waveform";
const STATUS_DATASET_ID: &str = "stage-a-funcgen.status";
const WAVEFORM_VIEW_ID: &str = "stage-a-funcgen.waveform.view";
const STATUS_VIEW_ID: &str = "stage-a-funcgen.status.view";

const ACTION_CONNECT: &str = "stage-a-funcgen.connect";
const ACTION_DISCONNECT: &str = "stage-a-funcgen.disconnect";
const ACTION_APPLY: &str = "stage-a-funcgen.apply";
const ACTION_STOP: &str = "stage-a-funcgen.stop";

/// Retained sample window for the live view + contrast estimate.
const SAMPLE_RING_CAPACITY: usize = 32_768;
/// Points published per waveform refresh (decimated).
const WAVEFORM_POINTS: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Disconnected,
    Connected,
    Driving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wave {
    Sine,
    Square,
    Saw,
}

impl Wave {
    const VARIANTS: [Wave; 3] = [Wave::Sine, Wave::Square, Wave::Saw];

    fn name(self) -> &'static str {
        match self {
            Self::Sine => "SINE",
            Self::Square => "SQUARE",
            Self::Saw => "SAW",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::VARIANTS.into_iter().find(|w| w.name() == name)
    }
}

/// In-process mock controller thread behind the `mock` port.
struct MockService {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl MockService {
    fn spawn() -> (Self, StageAClient<stage_a_io::MockTransport>) {
        let link = stage_a_io::MockLink::new();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let mut controller = MockController::new(link.device_end()).with_waveform_extension();
        let join = std::thread::Builder::new()
            .name("stage-a-funcgen-mock".into())
            .spawn(move || {
                let mut last_block = Instant::now();
                while !thread_stop.load(Ordering::Relaxed) {
                    controller.poll_commands();
                    if controller.state() == MockState::Running
                        && last_block.elapsed() >= controller.block_period()
                    {
                        last_block = Instant::now();
                        controller.emit_configured_block();
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
            .expect("spawning the mock controller thread must succeed");
        (
            Self {
                stop,
                join: Some(join),
            },
            StageAClient::new(link.host_end()),
        )
    }
}

impl Drop for MockService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub struct StageAFuncGenPlugin {
    enabled: bool,
    // -- device --
    worker: Option<IoWorker>,
    mock_service: Option<MockService>,
    connection: ConnectionState,
    firmware: String,
    has_waveform_backend: Option<bool>,
    next_tag: u64,
    in_flight: BTreeMap<u64, String>,
    last_error: Option<String>,
    integrity: StreamIntegrity,
    effects_blocked_reason: Option<String>,
    // -- settings (drive parameters; applying them is an explicit action) --
    port_hint: String,
    wave: Wave,
    frequency_hz: f64,
    center_dac: i64,
    amplitude_dac: i64,
    sample_rate_hz: i64,
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

impl Default for StageAFuncGenPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            worker: None,
            mock_service: None,
            connection: ConnectionState::Disconnected,
            firmware: String::new(),
            has_waveform_backend: None,
            next_tag: 1,
            in_flight: BTreeMap::new(),
            last_error: None,
            integrity: StreamIntegrity::default(),
            effects_blocked_reason: None,
            port_hint: "mock".into(),
            wave: Wave::Sine,
            frequency_hz: 1_000.0,
            center_dac: 2_048,
            amplitude_dac: 512,
            sample_rate_hz: 20_000,
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

impl StageAFuncGenPlugin {
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
        if self.port_hint == "mock" {
            let (service, client) = MockService::spawn();
            self.mock_service = Some(service);
            self.worker = Some(IoWorker::spawn(client));
            self.last_error = None;
            self.queue_command("hello", Command::new("HELLO").field("protocol", 1));
        } else {
            match open_serial(&self.port_hint) {
                Ok(client) => {
                    self.worker = Some(IoWorker::spawn(client));
                    self.last_error = None;
                    self.queue_command("hello", Command::new("HELLO").field("protocol", 1));
                }
                Err(err) => self.last_error = Some(err),
            }
        }
        self.bump_generation();
    }

    fn disconnect(&mut self, reason: &str) {
        if let Some(worker) = self.worker.take() {
            // Shut the worker down first: its final STOP still needs the
            // mock service (if any) alive to be acknowledged.
            worker.shutdown(reason);
        }
        self.mock_service = None;
        self.connection = ConnectionState::Disconnected;
        self.firmware.clear();
        self.has_waveform_backend = None;
        self.in_flight.clear();
        self.bump_generation();
    }

    /// STOP → CONFIG (drive fields) → START, honouring the firmware state
    /// machine (CONFIG is only legal from SAFE_IDLE/CONFIGURED).
    fn apply_drive(&mut self) {
        let center = self.center_dac.clamp(0, 4_095);
        let amplitude = self.amplitude_dac.clamp(0, 2_047);
        if center + amplitude > 4_095 || amplitude > center {
            self.last_error = Some(format!(
                "drive: center {center} ± amplitude {amplitude} exceeds the 0–4095 DAC range"
            ));
            return;
        }
        let freq_mhz = ((self.frequency_hz.max(0.001)) * 1_000.0).round() as i64;
        self.queue_command("stop", Command::new("STOP").field("reason", "reconfigure"));
        self.queue_command(
            "drive",
            Command::new("CONFIG")
                .field("mode", "A1")
                .field("wave", self.wave.name())
                .field("freq_mhz", freq_mhz)
                .field("center_dac", center)
                .field("amplitude_dac", amplitude)
                .field("rate_hz", self.sample_rate_hz)
                .field("block_samples", 256)
                .field("raw", 1)
                .field("summary", 1),
        );
        self.queue_command("start", Command::new("START"));
        if let Some(worker) = &self.worker {
            let _ = worker.try_send(WorkerRequest::SetPinging(true));
        }
    }

    fn stop_drive(&mut self) {
        self.queue_command("stop", Command::new("STOP").field("reason", "operator"));
        if let Some(worker) = &self.worker {
            let _ = worker.try_send(WorkerRequest::SetPinging(false));
        }
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
                            self.has_waveform_backend = Some(false);
                            self.last_error = Some(
                                "firmware has no waveform backend (v1) — select the mock port \
                                 or wait for the v2 DDS firmware"
                                    .into(),
                            );
                        }
                        Err(err) => {
                            self.last_error = Some(format!("{purpose}: {err}"));
                        }
                    }
                }
                WorkerOutput::Event(DeviceEvent::Data(frame)) => {
                    if frame.header.frame_type == FrameType::SamplesU16 {
                        if let Some(codes) = frame.samples() {
                            self.sample_rate_seen_hz = frame.header.sample_rate_hz;
                            self.push_samples(&codes, frame.header.first_sample_index);
                        }
                    }
                }
                WorkerOutput::Event(DeviceEvent::Async { name, fields }) => {
                    if name == "FAULT" {
                        if self.connection == ConnectionState::Driving {
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
            self.mock_service = None;
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
                self.has_waveform_backend = Some(
                    fields
                        .get("capabilities")
                        .is_some_and(|caps| caps.split(',').any(|c| c == "WAVE")),
                );
                self.connection = ConnectionState::Connected;
            }
            "drive" => {
                self.has_waveform_backend = Some(true);
            }
            "start" => {
                self.connection = ConnectionState::Driving;
            }
            "stop" => {
                if self.connection == ConnectionState::Driving {
                    self.connection = ConnectionState::Connected;
                }
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

    fn drive_summary(&self) -> String {
        format!(
            "{} @ {:.3} Hz, {} ± {} DAC",
            self.wave.name(),
            self.frequency_hz,
            self.center_dac,
            self.amplitude_dac
        )
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let state = match (&self.effects_blocked_reason, self.connection) {
            (Some(reason), _) => format!("locked ({reason})"),
            (None, ConnectionState::Disconnected) => "disconnected".into(),
            (None, ConnectionState::Connected) => "connected".into(),
            (None, ConnectionState::Driving) => "driving".into(),
        };
        let backend = match self.has_waveform_backend {
            Some(true) => "waveform-capable".into(),
            Some(false) => "no waveform backend (v1)".into(),
            None => "—".into(),
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
                text_column("backend", backend),
                text_column("drive", self.drive_summary()),
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
                column("backend", "Waveform backend"),
                column("drive", "Commanded drive"),
                column("a", "Measured a = ln(Vmax/Vmin)"),
                column("clipping", "Clipping"),
                column("integrity", "Stream integrity"),
                column("error", "Last error"),
            ],
            ..TableSchema::default()
        }
    }

    fn consume_actions(&mut self, context: &HostContext<'_>) -> Vec<String> {
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
            if !request.action_id.starts_with("stage-a-funcgen.") {
                continue;
            }
            self.consumed_action_ids.push(request.request_id);
            if self.consumed_action_ids.len() > 256 {
                self.consumed_action_ids.remove(0);
            }
            consumed.push(request.action_id);
        }
        consumed
    }
}

fn open_serial(port_hint: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let path = if port_hint == "auto" {
        serial_ports()
            .into_iter()
            .next()
            .ok_or_else(|| "no USB serial device found (looked for usbmodem/ttyACM)".to_owned())?
    } else {
        port_hint.to_owned()
    };
    let transport =
        stage_a_io::SerialTransport::open(&path, 115_200, std::time::Duration::from_millis(20))
            .map_err(|err| err.to_string())?;
    Ok(StageAClient::new(transport))
}

fn serial_ports() -> Vec<String> {
    stage_a_io::transport::available_port_names()
        .into_iter()
        .filter(|name| name.contains("usbmodem") || name.contains("ttyACM"))
        .collect()
}

impl Plugin for StageAFuncGenPlugin {
    fn name(&self) -> &'static str {
        "Stage-A Function Generator"
    }

    fn description(&self) -> &'static str {
        "Manual Pockels-cell drive (sine/square/sawtooth, frequency, DAC amplitude) with photodiode-measured optical contrast; mock port for hardware-free familiarisation."
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
        // connection down and refuses commands — even for the mock port,
        // so switching the port setting can never bypass the gate.
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

        for action_id in self.consume_actions(context) {
            match action_id.as_str() {
                ACTION_CONNECT => self.connect(),
                ACTION_DISCONNECT => self.disconnect("operator"),
                ACTION_APPLY => self.apply_drive(),
                ACTION_STOP => self.stop_drive(),
                _ => {}
            }
        }

        self.drain_worker();
    }

    fn settings_schema(&self) -> SettingsSchema {
        let mut port_variants = vec!["mock".to_owned(), "auto".to_owned()];
        port_variants.extend(serial_ports());
        let port_default = port_variants
            .iter()
            .position(|p| *p == self.port_hint)
            .unwrap_or(0);
        let wave_variants: Vec<String> =
            Wave::VARIANTS.iter().map(|w| w.name().to_owned()).collect();
        let wave_default = Wave::VARIANTS
            .iter()
            .position(|w| *w == self.wave)
            .unwrap_or(0);
        SettingsSchema {
            sections: vec![SettingsSection {
                label: "Function generator".into(),
                description: Some(
                    "Drive parameters are settings; nothing reaches the hardware until the \
                     Apply action. The optical amplitude is measured from the photodiode — \
                     the DAC amplitude is a phase-modulation depth, not a light level."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "port".into(),
                        label: "Port".into(),
                        tooltip: Some(
                            "mock = in-process simulated controller (no hardware); \
                             auto = first Teensy USB serial device"
                                .into(),
                        ),
                        kind: SettingKind::Enum {
                            variants: port_variants,
                            default: port_default,
                        },
                    },
                    SettingItem {
                        key: "wave".into(),
                        label: "Waveform".into(),
                        tooltip: Some("SINE, SQUARE, or SAW (sawtooth / Sägezahn)".into()),
                        kind: SettingKind::Enum {
                            variants: wave_variants,
                            default: wave_default,
                        },
                    },
                    SettingItem {
                        key: "frequency_hz".into(),
                        label: "Frequency".into(),
                        tooltip: Some("Drive frequency (sent as integer millihertz)".into()),
                        kind: SettingKind::F64Drag {
                            min: 0.001,
                            max: 200_000.0,
                            speed: 1.0,
                            default: self.frequency_hz,
                        },
                    },
                    SettingItem {
                        key: "center_dac".into(),
                        label: "Center DAC code".into(),
                        tooltip: Some("Working-point code (0–4095)".into()),
                        kind: SettingKind::I64Drag {
                            min: 0,
                            max: 4_095,
                            default: self.center_dac,
                        },
                    },
                    SettingItem {
                        key: "amplitude_dac".into(),
                        label: "Amplitude DAC code".into(),
                        tooltip: Some(
                            "Pockels phase-modulation depth (0–2047); the optical contrast \
                             this produces is read from the measured a"
                                .into(),
                        ),
                        kind: SettingKind::I64Drag {
                            min: 0,
                            max: 2_047,
                            default: self.amplitude_dac,
                        },
                    },
                    SettingItem {
                        key: "sample_rate_hz".into(),
                        label: "ADC sample rate".into(),
                        tooltip: Some("Photodiode sample rate for the feedback stream".into()),
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
            "wave" => Some(json!(self.wave.name())),
            "frequency_hz" => Some(json!(self.frequency_hz)),
            "center_dac" => Some(json!(self.center_dac)),
            "amplitude_dac" => Some(json!(self.amplitude_dac)),
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
            "wave" => {
                let name = value.as_str().ok_or("wave must be a string")?;
                self.wave = Wave::from_name(name)
                    .ok_or_else(|| format!("unknown waveform: {name} (SINE/SQUARE/SAW)"))?;
                Ok(())
            }
            "frequency_hz" => {
                let hz = value.as_f64().ok_or("frequency_hz must be a number")?;
                self.frequency_hz = hz.clamp(0.001, 200_000.0);
                Ok(())
            }
            "center_dac" => {
                self.center_dac = value
                    .as_i64()
                    .ok_or("center_dac must be an integer")?
                    .clamp(0, 4_095);
                Ok(())
            }
            "amplitude_dac" => {
                self.amplitude_dac = value
                    .as_i64()
                    .ok_or("amplitude_dac must be an integer")?
                    .clamp(0, 2_047);
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
            ConnectionState::Disconnected => "FuncGen: disconnected".into(),
            ConnectionState::Connected => format!("FuncGen: connected ({})", self.firmware),
            ConnectionState::Driving => format!("FuncGen: driving {}", self.drive_summary()),
        }));
        if let Some(estimate) = &self.contrast {
            entries.push(StatusEntry::Text(format!("a = {:.4}", estimate.a)));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        let dataset_action = |id: &str, title: &str| HostActionDescriptor {
            id: id.into(),
            title: title.into(),
            scope: HostActionScope::Dataset {
                dataset_id: STATUS_DATASET_ID.into(),
            },
            param_schema: None,
        };
        HostViewRegistry {
            datasets: vec![
                HostDatasetDescriptor {
                    id: WAVEFORM_DATASET_ID.into(),
                    title: "FuncGen photodiode waveform".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "No photodiode samples yet — connect and apply a drive.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: STATUS_DATASET_ID.into(),
                    title: "Function generator status".into(),
                    kind: HostDatasetKind::TableV1(self.status_schema()),
                    empty_message: "Function generator idle.".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: WAVEFORM_VIEW_ID.into(),
                    title: "FuncGen photodiode".into(),
                    dataset_id: WAVEFORM_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: STATUS_VIEW_ID.into(),
                    title: "Function generator".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
            ],
            actions: vec![
                dataset_action(ACTION_CONNECT, "Connect"),
                dataset_action(ACTION_DISCONNECT, "Disconnect"),
                dataset_action(ACTION_APPLY, "Apply drive"),
                dataset_action(ACTION_STOP, "Stop drive"),
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

impl Drop for StageAFuncGenPlugin {
    fn drop(&mut self) {
        self.disconnect("plugin destroyed");
    }
}

export_plugin!(StageAFuncGenPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn drain_until<F: FnMut(&mut StageAFuncGenPlugin) -> bool>(
        plugin: &mut StageAFuncGenPlugin,
        timeout: Duration,
        mut done: F,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            plugin.drain_worker();
            if done(plugin) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("condition not reached within {timeout:?}");
    }

    /// Full mock loop: connect → apply sine → measured a appears → stop.
    #[test]
    fn mock_port_round_trip_measures_optical_contrast() {
        let mut plugin = StageAFuncGenPlugin::default();
        plugin.calibration.dark_volts = 40.0 * 3.3 / 4_095.0;
        plugin.connect();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.connection == ConnectionState::Connected
        });
        assert_eq!(plugin.has_waveform_backend, Some(true));
        assert_eq!(plugin.firmware, "0.2.0-mock");

        plugin.apply_drive();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.connection == ConnectionState::Driving && p.contrast.is_some()
        });
        let a = plugin.contrast.as_ref().expect("contrast measured").a;
        assert!(a > 0.0, "modulated drive must produce positive contrast");
        assert!(plugin.integrity.is_clean());
        assert!(plugin.last_error.is_none(), "{:?}", plugin.last_error);

        plugin.stop_drive();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.connection == ConnectionState::Connected
        });
        plugin.disconnect("test done");
        assert_eq!(plugin.connection, ConnectionState::Disconnected);
    }

    /// Square and sawtooth are accepted and produce a measurable contrast.
    #[test]
    fn square_and_saw_waveforms_drive_the_mock() {
        for wave in [Wave::Square, Wave::Saw] {
            let mut plugin = StageAFuncGenPlugin::default();
            plugin.wave = wave;
            plugin.connect();
            drain_until(&mut plugin, Duration::from_secs(2), |p| {
                p.connection == ConnectionState::Connected
            });
            plugin.apply_drive();
            drain_until(&mut plugin, Duration::from_secs(2), |p| {
                p.connection == ConnectionState::Driving && p.contrast.is_some()
            });
            assert!(plugin.contrast.as_ref().unwrap().a > 0.0);
            plugin.disconnect("done");
        }
    }

    /// Drives exceeding the DAC range are refused locally, before any
    /// command reaches a controller.
    #[test]
    fn out_of_range_drive_is_rejected_locally() {
        let mut plugin = StageAFuncGenPlugin::default();
        plugin.center_dac = 3_000;
        plugin.amplitude_dac = 2_000;
        plugin.apply_drive();
        assert!(plugin
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("exceeds the 0–4095 DAC range")));
    }

    /// Re-applying while driving must STOP first (firmware state machine).
    #[test]
    fn reapply_while_driving_reconfigures_cleanly() {
        let mut plugin = StageAFuncGenPlugin::default();
        plugin.connect();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.connection == ConnectionState::Connected
        });
        plugin.apply_drive();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.connection == ConnectionState::Driving
        });
        plugin.frequency_hz = 2_000.0;
        plugin.apply_drive();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.connection == ConnectionState::Driving && p.last_error.is_none()
        });
        plugin.disconnect("done");
    }
}
