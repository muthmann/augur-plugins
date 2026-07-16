//! Stage-A laser modulation control.
//!
//! Drives the laser modulation input (Hermit J23, `DAC1.4`/address 3) through
//! the firmware 0.3.0 `MOD` command. One power slider (DAC code) whose upper
//! bound is a user-set safety cap, a mode select (constant / sine / square)
//! with frequency and a lower threshold for the periodic modes — and every
//! accepted change is transferred to the Teensy immediately, no Apply button.
//!
//! The plugin owns the Teensy **command port** (the first of the two CDC
//! ports the dual-serial firmware enumerates; the photodiode stream port is
//! owned by `stage-a-photodiode`). The firmware output is set-and-hold:
//! disconnecting does NOT switch the modulation off — use the "Output OFF"
//! action (ADR 002 in `stage-a-controller`).
//!
//! Safety contract:
//! - devices open only when the execution context allows hardware effects;
//!   anything else tears the connection down (fail closed);
//! - the level slider cannot exceed the max-level cap, and the firmware
//!   output can never exceed the slider (square/sine peak at `level`);
//! - `process_frame()` only drains the bounded I/O worker queues.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostActionDescriptor, HostActionRequestQueue, HostActionScope,
    HostContext, HostDatasetDescriptor, HostDatasetKind, HostOutput, HostViewDescriptor,
    HostViewKind, HostViewPlacement, HostViewRegistry, Plugin, PluginFrame, SettingItem,
    SettingKind, SettingsSchema, SettingsSection, StatusEntry, TableColumn, TableColumnData,
    TableColumnValues, TableDatasetV1, TableSchema, TableValueType,
    CTX_INVESTIGATION_ACTION_REQUESTS,
};
use serde_json::{json, Value};
use stage_a_io::{Command, IoWorker, MockController, StageAClient, WorkerOutput, WorkerRequest};

const STATUS_DATASET_ID: &str = "stage-a-modulation.status";
const STATUS_VIEW_ID: &str = "stage-a-modulation.status.view";

const ACTION_CONNECT: &str = "stage-a-modulation.connect";
const ACTION_DISCONNECT: &str = "stage-a-modulation.disconnect";
const ACTION_OUTPUT_OFF: &str = "stage-a-modulation.output-off";

const MAX_DAC_CODE: i64 = 4_095;
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Const,
    Sine,
    Square,
}

impl Mode {
    const VARIANTS: [Mode; 3] = [Mode::Const, Mode::Sine, Mode::Square];

    fn name(self) -> &'static str {
        match self {
            Self::Const => "CONST",
            Self::Sine => "SINE",
            Self::Square => "SQUARE",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::VARIANTS.into_iter().find(|m| m.name() == name)
    }

    fn is_periodic(self) -> bool {
        !matches!(self, Self::Const)
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
        let mut controller = MockController::new(link.device_end());
        let join = std::thread::Builder::new()
            .name("stage-a-modulation-mock".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    controller.poll_commands();
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

pub struct StageAModulationPlugin {
    enabled: bool,
    // -- device --
    worker: Option<IoWorker>,
    mock_service: Option<MockService>,
    connected: bool,
    firmware: String,
    next_tag: u64,
    in_flight: BTreeMap<u64, String>,
    last_error: Option<String>,
    effects_blocked_reason: Option<String>,
    last_status_poll: Instant,
    // -- settings (every accepted change is sent immediately) --
    port_hint: String,
    max_level: i64,
    level: i64,
    min_level: i64,
    mode: Mode,
    frequency_hz: f64,
    dirty: bool,
    // -- board-reported state (from MOD replies and STATUS polls) --
    board_code: Option<i64>,
    board_mod: String,
    dataset_generation: u64,
    consumed_action_ids: Vec<u64>,
}

impl Default for StageAModulationPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            worker: None,
            mock_service: None,
            connected: false,
            firmware: String::new(),
            next_tag: 1,
            in_flight: BTreeMap::new(),
            last_error: None,
            effects_blocked_reason: None,
            last_status_poll: Instant::now(),
            port_hint: "mock".into(),
            max_level: MAX_DAC_CODE,
            level: 0,
            min_level: 0,
            mode: Mode::Const,
            frequency_hz: 10.0,
            dirty: false,
            board_code: None,
            board_mod: "—".into(),
            dataset_generation: 0,
            consumed_action_ids: Vec::new(),
        }
    }
}

impl StageAModulationPlugin {
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
        } else {
            match open_serial(&self.port_hint) {
                Ok(client) => {
                    self.worker = Some(IoWorker::spawn(client));
                    self.last_error = None;
                }
                Err(err) => {
                    self.last_error = Some(err);
                    return;
                }
            }
        }
        // Connecting never drives the output: only changes made while
        // connected are transferred.
        self.dirty = false;
        self.queue_command("hello", Command::new("HELLO").field("protocol", 1));
        self.bump_generation();
    }

    fn disconnect(&mut self, reason: &str) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown(reason);
        }
        self.mock_service = None;
        self.connected = false;
        self.firmware.clear();
        self.in_flight.clear();
        self.board_code = None;
        self.board_mod = "—".into();
        self.bump_generation();
    }

    /// One MOD command carrying the complete current drive settings.
    fn send_modulation(&mut self) {
        self.dirty = false;
        let level = self.level.clamp(0, self.max_level);
        let mut command = Command::new("MOD")
            .field("wave", self.mode.name())
            .field("level", level);
        if self.mode.is_periodic() {
            let freq_mhz = (self.frequency_hz.clamp(0.01, 2_000.0) * 1_000.0).round() as i64;
            command = command
                .field("min", self.min_level.clamp(0, level))
                .field("freq_mhz", freq_mhz);
        }
        self.queue_command("mod", command);
    }

    fn output_off(&mut self) {
        self.dirty = false;
        self.queue_command("mod", Command::new("MOD").field("wave", "OFF"));
    }

    fn drain_worker(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        let outputs = worker.drain_outputs();
        if outputs.is_empty() {
            return;
        }
        let mut stopped: Option<String> = None;
        for output in outputs {
            match output {
                WorkerOutput::Reply { tag, result } => {
                    let purpose = self.in_flight.remove(&tag).unwrap_or_default();
                    match result {
                        Ok(fields) => self.handle_reply(&purpose, &fields),
                        Err(err) => self.last_error = Some(format!("{purpose}: {err}")),
                    }
                }
                WorkerOutput::Event(_) | WorkerOutput::Integrity(_) => {}
                WorkerOutput::Stopped { reason } => stopped = Some(reason),
            }
        }
        if let Some(reason) = stopped {
            self.worker = None;
            self.mock_service = None;
            self.connected = false;
            self.last_error = Some(format!("device connection ended: {reason}"));
        }
        self.bump_generation();
    }

    fn handle_reply(&mut self, purpose: &str, fields: &BTreeMap<String, String>) {
        if purpose == "hello" {
            self.firmware = fields
                .get("firmware")
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            self.connected = true;
            let has_mod = fields
                .get("capabilities")
                .is_some_and(|caps| caps.split(',').any(|c| c == "MOD"));
            if !has_mod {
                self.last_error =
                    Some("firmware has no MOD capability — flash stage-a-controller 0.3.0+".into());
            }
        }
        // MOD replies and STATUS polls both carry code= and mod_* fields.
        if let Some(code) = fields.get("code").and_then(|v| v.parse::<i64>().ok()) {
            self.board_code = Some(code);
        }
        if let Some(wave) = fields.get("mod_wave") {
            let level = fields.get("mod_level").map(String::as_str).unwrap_or("?");
            let min = fields.get("mod_min").map(String::as_str).unwrap_or("?");
            let freq_mhz = fields
                .get("mod_freq_mhz")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            self.board_mod = if wave == "SINE" || wave == "SQUARE" {
                format!("{wave} {min}..{level} @ {:.3} Hz", freq_mhz / 1_000.0)
            } else {
                format!("{wave} level={level}")
            };
        }
        if purpose == "mod" {
            self.last_error = None;
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
            if !request.action_id.starts_with("stage-a-modulation.") {
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

    fn commanded_summary(&self) -> String {
        if self.mode.is_periodic() {
            format!(
                "{} {}..{} @ {:.3} Hz",
                self.mode.name(),
                self.min_level,
                self.level,
                self.frequency_hz
            )
        } else {
            format!("{} level={}", self.mode.name(), self.level)
        }
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let state = match (&self.effects_blocked_reason, self.connected) {
            (Some(reason), _) => format!("locked ({reason})"),
            (None, false) => "disconnected".into(),
            (None, true) => format!("connected ({})", self.firmware),
        };
        let board_code = self
            .board_code
            .map_or_else(|| "—".into(), |code| code.to_string());
        let text_column = |id: &str, value: String| TableColumnData {
            column_id: id.to_owned(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                text_column("state", state),
                text_column("commanded", self.commanded_summary()),
                text_column("board_mod", self.board_mod.clone()),
                text_column("board_code", board_code),
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
                column("commanded", "Commanded drive"),
                column("board_mod", "Board modulation"),
                column("board_code", "Board DAC code"),
                column("error", "Last error"),
            ],
            ..TableSchema::default()
        }
    }
}

fn open_serial(port_hint: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    if port_hint == "auto" {
        // The dual-serial Teensy enumerates two ports and only the command
        // port answers HELLO — probe until one does.
        let candidates = serial_ports();
        if candidates.is_empty() {
            return Err("no USB serial device found (looked for usbmodem/ttyACM)".to_owned());
        }
        let mut failures = Vec::new();
        for path in &candidates {
            match probe_command_port(path) {
                // Restore the client's default reply timeout after probing.
                Ok(client) => return Ok(client.with_reply_timeout(Duration::from_millis(500))),
                Err(err) => failures.push(format!("{path}: {err}")),
            }
        }
        return Err(format!(
            "no Teensy command port answered HELLO ({})",
            failures.join("; ")
        ));
    }
    open_path(port_hint)
}

fn open_path(path: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let transport =
        stage_a_io::SerialTransport::open(path, 115_200, std::time::Duration::from_millis(20))
            .map_err(|err| err.to_string())?;
    Ok(StageAClient::new(transport))
}

/// Opens `path` and sends HELLO with a short timeout: only the Teensy
/// command port replies (the photodiode stream port never answers).
fn probe_command_port(path: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let mut client = open_path(path)?.with_reply_timeout(Duration::from_millis(300));
    client
        .request(&Command::new("HELLO").field("protocol", 1))
        .map_err(|err| err.to_string())?;
    Ok(client)
}

fn serial_ports() -> Vec<String> {
    stage_a_io::transport::available_port_names()
        .into_iter()
        // macOS lists each device twice; use the callout (cu.*) node only.
        .filter(|name| name.contains("cu.usbmodem") || name.contains("ttyACM"))
        .collect()
}

/// The exact variant list the settings schema shows for the port enum — the
/// host exchanges enum settings as indices into this list.
fn port_variants() -> Vec<String> {
    let mut variants = vec!["mock".to_owned(), "auto".to_owned()];
    variants.extend(serial_ports());
    variants
}

/// Host enum widgets send the selected index; string names are also accepted
/// (tests, saved configs).
fn enum_choice(value: &Value, variants: &[String]) -> Result<String, String> {
    if let Some(index) = value.as_u64() {
        return variants
            .get(usize::try_from(index).map_err(|_| "index out of range".to_owned())?)
            .cloned()
            .ok_or_else(|| format!("enum index {index} out of range"));
    }
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "expected an enum index or name".to_owned())
}

impl Plugin for StageAModulationPlugin {
    fn name(&self) -> &'static str {
        "Stage-A Modulation"
    }

    fn description(&self) -> &'static str {
        "Laser modulation control on the Teensy command port: capped power slider, constant/sine/square with frequency, applied immediately; shows the DAC code the board reports."
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
        self.bump_generation();
    }

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        // Fail closed: without live-capture effects the connection is torn
        // down and no command leaves the plugin.
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
                ACTION_OUTPUT_OFF => self.output_off(),
                _ => {}
            }
        }

        if self.dirty && self.connected {
            self.send_modulation();
        }
        if self.connected && self.last_status_poll.elapsed() >= STATUS_POLL_INTERVAL {
            self.last_status_poll = Instant::now();
            self.queue_command("status", Command::new("STATUS"));
        }
        self.drain_worker();
    }

    fn settings_schema(&self) -> SettingsSchema {
        let port_variants = port_variants();
        let port_default = port_variants
            .iter()
            .position(|p| *p == self.port_hint)
            .unwrap_or(0);
        let mode_variants: Vec<String> =
            Mode::VARIANTS.iter().map(|m| m.name().to_owned()).collect();
        let mode_default = Mode::VARIANTS
            .iter()
            .position(|m| *m == self.mode)
            .unwrap_or(0);
        SettingsSchema {
            sections: vec![SettingsSection {
                label: "Laser modulation".into(),
                description: Some(
                    "Every change is sent to the Teensy immediately. The output never exceeds \
                     the power slider, and the slider never exceeds the max limit. The firmware \
                     holds the output when the plugin disconnects — use Output OFF to drive 0."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "port".into(),
                        label: "Port".into(),
                        tooltip: Some(
                            "auto (recommended) probes the attached usbmodem ports and picks \
                             the one that answers HELLO — the Teensy command port; \
                             mock = in-process simulated controller"
                                .into(),
                        ),
                        kind: SettingKind::Enum {
                            variants: port_variants,
                            default: port_default,
                        },
                    },
                    SettingItem {
                        key: "level".into(),
                        label: "Power (DAC code)".into(),
                        tooltip: Some(
                            "Output level in DAC codes; peak value for sine/square. \
                             Capped by the max limit below."
                                .into(),
                        ),
                        kind: SettingKind::I64Slider {
                            min: 0,
                            max: self.max_level,
                            default: self.level,
                            suffix: None,
                        },
                    },
                    SettingItem {
                        key: "max_level".into(),
                        label: "Max limit (DAC code)".into(),
                        tooltip: Some(
                            "Safety cap: the slider cannot go above this. Set it to the \
                             highest code the connected device tolerates at J23."
                                .into(),
                        ),
                        kind: SettingKind::I64Drag {
                            min: 0,
                            max: MAX_DAC_CODE,
                            default: self.max_level,
                        },
                    },
                    SettingItem {
                        key: "mode".into(),
                        label: "Mode".into(),
                        tooltip: Some("CONST holds the level; SINE/SQUARE modulate".into()),
                        kind: SettingKind::Enum {
                            variants: mode_variants,
                            default: mode_default,
                        },
                    },
                    SettingItem {
                        key: "frequency_hz".into(),
                        label: "Frequency".into(),
                        tooltip: Some("Sine/square frequency, 0.01–2000 Hz".into()),
                        kind: SettingKind::F64Drag {
                            min: 0.01,
                            max: 2_000.0,
                            speed: 1.0,
                            default: self.frequency_hz,
                        },
                    },
                    SettingItem {
                        key: "min_level".into(),
                        label: "Min threshold (DAC code)".into(),
                        tooltip: Some(
                            "Lower bound for sine/square: the waveform swings between this \
                             and the power slider. Ignored in CONST mode."
                                .into(),
                        ),
                        kind: SettingKind::I64Slider {
                            min: 0,
                            max: self.max_level,
                            default: self.min_level,
                            suffix: None,
                        },
                    },
                ],
            }],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            // Enum settings are exchanged as indices into the schema's
            // variant list (see the host settings UI).
            "port" => {
                let index = port_variants()
                    .iter()
                    .position(|p| *p == self.port_hint)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "level" => Some(json!(self.level)),
            "max_level" => Some(json!(self.max_level)),
            "mode" => {
                let index = Mode::VARIANTS
                    .iter()
                    .position(|m| *m == self.mode)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "frequency_hz" => Some(json!(self.frequency_hz)),
            "min_level" => Some(json!(self.min_level)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "port" => {
                self.port_hint = enum_choice(&value, &port_variants())?;
                Ok(())
            }
            "level" => {
                self.level = value
                    .as_i64()
                    .ok_or("level must be an integer")?
                    .clamp(0, self.max_level);
                if self.min_level > self.level {
                    self.min_level = self.level;
                }
                self.dirty = true;
                Ok(())
            }
            "max_level" => {
                self.max_level = value
                    .as_i64()
                    .ok_or("max_level must be an integer")?
                    .clamp(0, MAX_DAC_CODE);
                // Lowering the cap below the current level lowers the output.
                if self.level > self.max_level {
                    self.level = self.max_level;
                    self.dirty = true;
                }
                if self.min_level > self.max_level {
                    self.min_level = self.max_level;
                }
                Ok(())
            }
            "mode" => {
                let mode_names: Vec<String> =
                    Mode::VARIANTS.iter().map(|m| m.name().to_owned()).collect();
                let name = enum_choice(&value, &mode_names)?;
                self.mode = Mode::from_name(&name)
                    .ok_or_else(|| format!("unknown mode: {name} (CONST/SINE/SQUARE)"))?;
                self.dirty = true;
                Ok(())
            }
            "frequency_hz" => {
                let hz = value.as_f64().ok_or("frequency_hz must be a number")?;
                self.frequency_hz = hz.clamp(0.01, 2_000.0);
                if self.mode.is_periodic() {
                    self.dirty = true;
                }
                Ok(())
            }
            "min_level" => {
                self.min_level = value
                    .as_i64()
                    .ok_or("min_level must be an integer")?
                    .clamp(0, self.level);
                if self.mode.is_periodic() {
                    self.dirty = true;
                }
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
        entries.push(StatusEntry::Text(if self.connected {
            format!("Modulation: connected ({})", self.firmware)
        } else {
            "Modulation: disconnected".into()
        }));
        if let Some(code) = self.board_code {
            entries.push(StatusEntry::Text(format!(
                "Board: code={code} ({})",
                self.board_mod
            )));
        }
        if let Some(error) = &self.last_error {
            entries.push(StatusEntry::Text(format!("Error: {error}")));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        let action = |id: &str, title: &str| HostActionDescriptor {
            id: id.into(),
            title: title.into(),
            scope: HostActionScope::Dataset {
                dataset_id: STATUS_DATASET_ID.into(),
            },
            param_schema: None,
        };
        HostViewRegistry {
            datasets: vec![HostDatasetDescriptor {
                id: STATUS_DATASET_ID.into(),
                title: "Laser modulation".into(),
                kind: HostDatasetKind::TableV1(self.status_schema()),
                empty_message: "Modulation control idle.".into(),
                display: None,
                relations: Vec::new(),
            }],
            views: vec![HostViewDescriptor {
                id: STATUS_VIEW_ID.into(),
                title: "Laser modulation".into(),
                dataset_id: STATUS_DATASET_ID.into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            }],
            actions: vec![
                action(ACTION_CONNECT, "Connect"),
                action(ACTION_DISCONNECT, "Disconnect"),
                action(ACTION_OUTPUT_OFF, "Output OFF"),
            ],
        }
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            STATUS_DATASET_ID => self.dataset_generation.max(1),
            _ => 0,
        }
    }
}

impl Drop for StageAModulationPlugin {
    fn drop(&mut self) {
        self.disconnect("plugin destroyed");
    }
}

export_plugin!(StageAModulationPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_until<F: FnMut(&mut StageAModulationPlugin) -> bool>(
        plugin: &mut StageAModulationPlugin,
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

    /// Slider change → MOD sent immediately → board echoes the code.
    #[test]
    fn level_change_transfers_immediately_and_board_code_is_shown() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.connect();
        drain_until(&mut plugin, Duration::from_secs(2), |p| p.connected);
        assert_eq!(plugin.firmware, "0.3.0-mock");

        plugin
            .set_setting("level", json!(1234))
            .expect("level accepted");
        assert!(plugin.dirty);
        plugin.send_modulation();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.board_code == Some(1234)
        });
        assert!(!plugin.dirty);
        assert!(plugin.last_error.is_none(), "{:?}", plugin.last_error);
        plugin.disconnect("test done");
    }

    /// The max cap bounds the slider, and lowering it re-sends a lower level.
    #[test]
    fn max_level_caps_the_slider() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.set_setting("max_level", json!(1000)).unwrap();
        plugin.set_setting("level", json!(4095)).unwrap();
        assert_eq!(plugin.level, 1000, "slider clamps to the cap");

        plugin.set_setting("max_level", json!(500)).unwrap();
        assert_eq!(plugin.level, 500, "lowering the cap lowers the level");
        assert!(plugin.dirty, "the lowered level must be transferred");

        let schema = plugin.settings_schema();
        let level_item = schema.sections[0]
            .items
            .iter()
            .find(|item| item.key == "level")
            .expect("level setting exists");
        match &level_item.kind {
            SettingKind::I64Slider { max, .. } => assert_eq!(*max, 500),
            other => panic!("level must stay a slider, got {other:?}"),
        }
    }

    /// Square drive with min threshold reaches the mock and starts at min.
    #[test]
    fn square_with_min_threshold_round_trips() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.connect();
        drain_until(&mut plugin, Duration::from_secs(2), |p| p.connected);

        plugin.set_setting("level", json!(2000)).unwrap();
        plugin.set_setting("mode", json!("SQUARE")).unwrap();
        plugin.set_setting("frequency_hz", json!(10.0)).unwrap();
        plugin.set_setting("min_level", json!(500)).unwrap();
        plugin.send_modulation();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.board_code == Some(500)
        });
        assert!(plugin.board_mod.contains("SQUARE 500..2000"));

        plugin.output_off();
        drain_until(&mut plugin, Duration::from_secs(2), |p| {
            p.board_code == Some(0)
        });
        plugin.disconnect("test done");
    }

    /// The host settings UI exchanges enum values as indices into the
    /// schema's variant list (radio buttons send `json!(index)`).
    #[test]
    fn enum_settings_round_trip_as_indices() {
        let mut plugin = StageAModulationPlugin::default();
        // Mode: index 2 = SQUARE in the schema's variant order.
        plugin
            .set_setting("mode", json!(2))
            .expect("index accepted");
        assert_eq!(plugin.mode, Mode::Square);
        assert_eq!(plugin.get_setting("mode"), Some(json!(2)));
        // Port: index 1 = "auto" (variants start with mock, auto).
        plugin
            .set_setting("port", json!(1))
            .expect("index accepted");
        assert_eq!(plugin.port_hint, "auto");
        assert_eq!(plugin.get_setting("port"), Some(json!(1)));
        // Out-of-range indices are visible errors, not silent no-ops.
        assert!(plugin.set_setting("mode", json!(99)).is_err());
        // String names keep working (tests, saved configs).
        plugin
            .set_setting("mode", json!("SINE"))
            .expect("name accepted");
        assert_eq!(plugin.mode, Mode::Sine);
    }

    /// min_level can never exceed the level.
    #[test]
    fn min_threshold_is_clamped_to_level() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.set_setting("level", json!(1000)).unwrap();
        plugin.set_setting("min_level", json!(3000)).unwrap();
        assert_eq!(plugin.min_level, 1000);
        plugin.set_setting("level", json!(200)).unwrap();
        assert_eq!(plugin.min_level, 200, "lowering level drags min down");
    }
}
