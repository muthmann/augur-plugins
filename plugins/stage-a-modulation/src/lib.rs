//! Stage-A laser modulation control.
//!
//! Drives the laser modulation input (Hermit J23, `DAC1.4`/address 3) through
//! the firmware 0.3.0 `MOD` command. One power slider (DAC code) whose upper
//! bound is a user-set safety cap, a mode select (constant / sine / square)
//! with frequency and a lower threshold for the periodic modes — and every
//! accepted change is transferred to the Teensy immediately, no Apply button.
//!
//! **Frame-independent by design.** The host only calls `process_frame()`
//! while camera frames flow, so nothing here depends on it: connecting is a
//! checkbox *setting* (settings arrive from the UI thread at any time), a
//! dedicated device thread owns the serial client, and slider changes are
//! coalesced into a pending-command slot that thread drains. The bench works
//! with no camera attached. `process_frame()` only tears the connection down
//! defensively in replay mode.
//!
//! The plugin owns the Teensy **command port**; the photodiode stream port is
//! owned by `stage-a-photodiode`. The firmware output is set-and-hold
//! (`stage-a-controller` ADR 002): disconnecting does NOT switch the
//! modulation off — drag the power slider to 0 to drive 0 V.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use augur_plugin_api::{
    export_plugin, EventStoreHandle, ExecutionMode, HostContext, HostDatasetDescriptor,
    HostDatasetKind, HostOutput, HostViewDescriptor, HostViewKind, HostViewPlacement,
    HostViewRegistry, Plugin, PluginFrame, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1,
    TableSchema, TableValueType,
};
use serde_json::{json, Value};
use stage_a_io::{Command, MockController, StageAClient, Transport};

const STATUS_DATASET_ID: &str = "stage-a-modulation.status";
const STATUS_VIEW_ID: &str = "stage-a-modulation.status.view";

const MAX_DAC_CODE: i64 = 4_095;
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEVICE_LOOP_TICK: Duration = Duration::from_millis(10);

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

/// State the device thread reports back for the UI (status entries, table).
#[derive(Default)]
struct DeviceState {
    connected: bool,
    firmware: String,
    board_code: Option<i64>,
    board_mod: String,
    last_error: Option<String>,
}

/// Everything shared between the plugin (UI thread) and the device thread.
struct SharedLink {
    state: Mutex<DeviceState>,
    /// Latest not-yet-sent command; newer settings overwrite older ones so
    /// slider drags coalesce instead of queueing.
    pending: Mutex<Option<Command>>,
    stop: AtomicBool,
    generation: AtomicU64,
}

impl SharedLink {
    fn new() -> Self {
        Self {
            state: Mutex::new(DeviceState::default()),
            pending: Mutex::new(None),
            stop: AtomicBool::new(false),
            generation: AtomicU64::new(1),
        }
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
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

/// Handle to the running device thread; dropping it stops the thread.
struct DeviceLink {
    shared: Arc<SharedLink>,
    join: Option<JoinHandle<()>>,
    _mock: Option<MockService>,
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Device thread: HELLO once, then drain the pending command slot and poll
/// STATUS. All serial I/O lives here — the UI thread never blocks.
fn run_device<T: Transport>(mut client: StageAClient<T>, shared: Arc<SharedLink>) {
    match client.request(&Command::new("HELLO").field("protocol", 1)) {
        Ok(fields) => {
            let mut state = shared.state.lock().expect("device state lock");
            state.connected = true;
            state.firmware = fields
                .get("firmware")
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            let has_mod = fields
                .get("capabilities")
                .is_some_and(|caps| caps.split(',').any(|c| c == "MOD"));
            state.last_error = (!has_mod).then(|| {
                "firmware has no MOD capability — flash stage-a-controller 0.3.0+".to_owned()
            });
        }
        Err(err) => {
            let mut state = shared.state.lock().expect("device state lock");
            state.connected = false;
            state.last_error = Some(format!("HELLO failed: {err}"));
            shared.bump();
            return;
        }
    }
    shared.bump();

    let mut last_status = Instant::now() - STATUS_POLL_INTERVAL;
    while !shared.stop.load(Ordering::Relaxed) {
        let pending = shared.pending.lock().expect("pending lock").take();
        if let Some(command) = pending {
            let result = client.request(&command);
            apply_reply(&shared, "MOD", result);
        } else if last_status.elapsed() >= STATUS_POLL_INTERVAL {
            last_status = Instant::now();
            let result = client.request(&Command::new("STATUS"));
            apply_reply(&shared, "STATUS", result);
        } else {
            std::thread::sleep(DEVICE_LOOP_TICK);
        }
    }

    let mut state = shared.state.lock().expect("device state lock");
    state.connected = false;
    shared.bump();
}

fn apply_reply(
    shared: &SharedLink,
    purpose: &str,
    result: Result<BTreeMap<String, String>, stage_a_io::ClientError>,
) {
    let mut state = shared.state.lock().expect("device state lock");
    match result {
        Ok(fields) => {
            if let Some(code) = fields.get("code").and_then(|v| v.parse::<i64>().ok()) {
                state.board_code = Some(code);
            }
            if let Some(wave) = fields.get("mod_wave") {
                let level = fields.get("mod_level").map(String::as_str).unwrap_or("?");
                let min = fields.get("mod_min").map(String::as_str).unwrap_or("?");
                let freq_mhz = fields
                    .get("mod_freq_mhz")
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                state.board_mod = if wave == "SINE" || wave == "SQUARE" {
                    format!("{wave} {min}..{level} @ {:.3} Hz", freq_mhz / 1_000.0)
                } else {
                    format!("{wave} level={level}")
                };
            }
            if purpose == "MOD" {
                state.last_error = None;
            }
        }
        Err(err) => state.last_error = Some(format!("{purpose}: {err}")),
    }
    drop(state);
    shared.bump();
}

pub struct StageAModulationPlugin {
    enabled: bool,
    link: Option<DeviceLink>,
    shared: Arc<SharedLink>,
    // -- settings (every accepted change is sent immediately) --
    connect_requested: bool,
    port_hint: String,
    max_level: i64,
    level: i64,
    min_level: i64,
    mode: Mode,
    frequency_hz: f64,
    last_error: Option<String>,
}

impl Default for StageAModulationPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            link: None,
            shared: Arc::new(SharedLink::new()),
            connect_requested: false,
            port_hint: "auto".into(),
            max_level: MAX_DAC_CODE,
            level: 0,
            min_level: 0,
            mode: Mode::Const,
            frequency_hz: 10.0,
            last_error: None,
        }
    }
}

impl StageAModulationPlugin {
    fn connect(&mut self) {
        if self.link.is_some() {
            return;
        }
        self.last_error = None;
        *self.shared.state.lock().expect("device state lock") = DeviceState::default();
        *self.shared.pending.lock().expect("pending lock") = None;
        self.shared.stop.store(false, Ordering::Relaxed);
        self.shared.bump();

        let shared = Arc::clone(&self.shared);
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name.to_owned())
                .spawn(f)
                .expect("spawning the device thread must succeed")
        };
        if self.port_hint == "mock" {
            let (mock, client) = MockService::spawn();
            let join = spawn(
                "stage-a-modulation-device",
                Box::new(move || run_device(client, shared)),
            );
            self.link = Some(DeviceLink {
                shared: Arc::clone(&self.shared),
                join: Some(join),
                _mock: Some(mock),
            });
        } else {
            match open_serial(&self.port_hint) {
                Ok(client) => {
                    let join = spawn(
                        "stage-a-modulation-device",
                        Box::new(move || run_device(client, shared)),
                    );
                    self.link = Some(DeviceLink {
                        shared: Arc::clone(&self.shared),
                        join: Some(join),
                        _mock: None,
                    });
                }
                Err(err) => {
                    self.last_error = Some(err);
                    self.connect_requested = false;
                }
            }
        }
        // Connecting never drives the output (set-and-hold firmware); only
        // changes made while connected are transferred.
    }

    fn disconnect(&mut self) {
        self.link = None; // Drop stops and joins the device thread.
        self.shared.bump();
    }

    /// Queues one MOD command carrying the complete current drive settings;
    /// newer changes overwrite queued ones (drag coalescing).
    fn send_modulation(&mut self) {
        if self.link.is_none() {
            return;
        }
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
        *self.shared.pending.lock().expect("pending lock") = Some(command);
    }

    #[cfg(test)]
    fn device_connected(&self) -> bool {
        self.shared
            .state
            .lock()
            .map(|state| state.connected)
            .unwrap_or(false)
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
        let state = self.shared.state.lock().expect("device state lock");
        let connection = if state.connected {
            format!("connected ({})", state.firmware)
        } else if self.connect_requested {
            "connecting…".into()
        } else {
            "disconnected".into()
        };
        let board_code = state
            .board_code
            .map_or_else(|| "—".into(), |code| code.to_string());
        let error = state
            .last_error
            .clone()
            .or_else(|| self.last_error.clone())
            .unwrap_or_default();
        let board_mod = if state.board_mod.is_empty() {
            "—".to_owned()
        } else {
            state.board_mod.clone()
        };
        drop(state);
        let text_column = |id: &str, value: String| TableColumnData {
            column_id: id.to_owned(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                text_column("state", connection),
                text_column("commanded", self.commanded_summary()),
                text_column("board_mod", board_mod),
                text_column("board_code", board_code),
                text_column("error", error),
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
/// host exchanges enum settings as indices into this list. Real ports carry
/// their USB label (e.g. "(Teensyduino Dual Serial)") for recognisability;
/// only the leading path is the value.
fn port_variants() -> Vec<String> {
    let mut variants = vec!["auto".to_owned(), "mock".to_owned()];
    for (name, label) in stage_a_io::transport::available_ports_with_labels() {
        if !(name.contains("cu.usbmodem") || name.contains("ttyACM")) {
            continue;
        }
        variants.push(match label {
            Some(label) => format!("{name} ({label})"),
            None => name,
        });
    }
    variants
}

/// The path part of a port variant; the parenthesised USB label is display-only.
fn variant_path(variant: &str) -> &str {
    variant.split_whitespace().next().unwrap_or(variant)
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
            self.connect_requested = false;
            self.disconnect();
        }
    }

    fn reset(&mut self) {}

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        // Control is settings-driven and works without camera frames. The
        // only frame-pass policy: replaying a recording must never keep a
        // hardware connection alive.
        if context.execution().mode == ExecutionMode::Replay && self.link.is_some() {
            self.connect_requested = false;
            self.disconnect();
            self.last_error = Some("disconnected: replay mode".into());
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        let port_variants = port_variants();
        let port_default = port_variants
            .iter()
            .position(|p| variant_path(p) == self.port_hint)
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
                    "Tick Connect, then every change is sent to the Teensy immediately — no \
                     camera required. The output never exceeds the power slider, the slider \
                     never exceeds the max limit. The firmware holds the output when \
                     disconnected; drag the slider to 0 to drive 0 V."
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
                        key: "connect".into(),
                        label: "Connect".into(),
                        tooltip: Some(
                            "Opens/closes the command port. Connecting never changes the \
                             output; disconnecting leaves it held (set-and-hold firmware)."
                                .into(),
                        ),
                        kind: SettingKind::Bool {
                            default: self.connect_requested,
                        },
                    },
                    SettingItem {
                        key: "level".into(),
                        label: "Power (DAC code)".into(),
                        tooltip: Some(
                            "Output level in DAC codes; peak value for sine/square. \
                             Capped by the max limit below. 0 = output off."
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
                    .position(|p| variant_path(p) == self.port_hint)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "connect" => Some(json!(self.connect_requested)),
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
                self.port_hint = variant_path(&enum_choice(&value, &port_variants())?).to_owned();
                Ok(())
            }
            "connect" => {
                let requested = value.as_bool().ok_or("connect must be a boolean")?;
                self.connect_requested = requested;
                if requested {
                    self.connect();
                } else {
                    self.disconnect();
                }
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
                self.send_modulation();
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
                    self.send_modulation();
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
                self.send_modulation();
                Ok(())
            }
            "frequency_hz" => {
                let hz = value.as_f64().ok_or("frequency_hz must be a number")?;
                self.frequency_hz = hz.clamp(0.01, 2_000.0);
                if self.mode.is_periodic() {
                    self.send_modulation();
                }
                Ok(())
            }
            "min_level" => {
                self.min_level = value
                    .as_i64()
                    .ok_or("min_level must be an integer")?
                    .clamp(0, self.level);
                if self.mode.is_periodic() {
                    self.send_modulation();
                }
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        let state = self.shared.state.lock().expect("device state lock");
        entries.push(StatusEntry::Text(if state.connected {
            format!("Modulation: connected ({})", state.firmware)
        } else if self.connect_requested {
            "Modulation: connecting…".into()
        } else {
            "Modulation: disconnected".into()
        }));
        if let Some(code) = state.board_code {
            entries.push(StatusEntry::Text(format!(
                "Board: code={code} ({})",
                state.board_mod
            )));
        }
        if let Some(error) = state.last_error.clone().or_else(|| self.last_error.clone()) {
            entries.push(StatusEntry::Text(format!("Error: {error}")));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
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
            actions: Vec::new(),
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
            STATUS_DATASET_ID => self.shared.generation.load(Ordering::Relaxed).max(1),
            _ => 0,
        }
    }
}

impl Drop for StageAModulationPlugin {
    fn drop(&mut self) {
        self.disconnect();
    }
}

export_plugin!(StageAModulationPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_until<F: Fn(&StageAModulationPlugin) -> bool>(
        plugin: &StageAModulationPlugin,
        timeout: Duration,
        done: F,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done(plugin) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("condition not reached within {timeout:?}");
    }

    fn board_code(plugin: &StageAModulationPlugin) -> Option<i64> {
        plugin.shared.state.lock().unwrap().board_code
    }

    /// Connect checkbox → slider change → MOD sent by the device thread →
    /// board echoes the code. No process_frame involved anywhere.
    #[test]
    fn level_change_transfers_without_frames() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        assert_eq!(
            plugin.shared.state.lock().unwrap().firmware,
            "0.3.0-mock".to_owned()
        );

        plugin.set_setting("level", json!(1234)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(1234)
        });
        assert!(plugin.shared.state.lock().unwrap().last_error.is_none());

        plugin.set_setting("connect", json!(false)).unwrap();
        assert!(!plugin.device_connected());
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

    /// Square drive with min threshold reaches the mock and starts at min;
    /// slider to 0 drives the output to 0.
    #[test]
    fn square_with_min_threshold_round_trips() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());

        plugin.set_setting("level", json!(2000)).unwrap();
        plugin.set_setting("frequency_hz", json!(10.0)).unwrap();
        plugin.set_setting("min_level", json!(500)).unwrap();
        plugin.set_setting("mode", json!("SQUARE")).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(500)
        });
        assert!(plugin
            .shared
            .state
            .lock()
            .unwrap()
            .board_mod
            .contains("SQUARE 500..2000"));

        plugin.set_setting("mode", json!("CONST")).unwrap();
        plugin.set_setting("level", json!(0)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(0)
        });
        plugin.set_setting("connect", json!(false)).unwrap();
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
        // Port: index 1 = "mock" (variants start with auto, mock).
        plugin
            .set_setting("port", json!(1))
            .expect("index accepted");
        assert_eq!(plugin.port_hint, "mock");
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
