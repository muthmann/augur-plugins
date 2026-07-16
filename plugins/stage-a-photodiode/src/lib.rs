//! Stage-A photodiode readout.
//!
//! Reads the free-running ASCII stream the `stage-a-controller` firmware
//! (0.3.0+, `USB_DUAL_SERIAL`) emits on its **second** USB serial port:
//! one `PD code=<mean> n=<reads> t_ms=<millis>` line every 20 ms from the
//! photodiode on board SMA5 → Teensy pin 18 / A4. The port carries no
//! commands, so opening it is side-effect free; the command port is owned by
//! `stage-a-modulation`.
//!
//! Two display modes:
//! - **RAW**: the ADC code and its voltage (`V = code · 3.3 / 4095`);
//! - **EXCITATION**: the photodiode sits behind the PBS in the excitation
//!   path and sees the light removed from the beam, `I_pd = I_tot − I_exc`.
//!   Given the user-set reference `I_tot` (in photodiode volts), the plugin
//!   shows `I_exc = I_tot − V_pd`.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

const SERIES_DATASET_ID: &str = "stage-a-photodiode.series";
const SERIES_VIEW_ID: &str = "stage-a-photodiode.series.view";
const STATUS_DATASET_ID: &str = "stage-a-photodiode.status";
const STATUS_VIEW_ID: &str = "stage-a-photodiode.status.view";

const ACTION_CONNECT: &str = "stage-a-photodiode.connect";
const ACTION_DISCONNECT: &str = "stage-a-photodiode.disconnect";

const ADC_FULL_SCALE_VOLTS: f64 = 3.3;
const ADC_MAX_CODE: f64 = 4_095.0;
/// Ring capacity: > 2.5 minutes at the firmware's 50 lines/s.
const RING_CAPACITY: usize = 8_192;

fn code_to_volts(code: f64) -> f64 {
    code * ADC_FULL_SCALE_VOLTS / ADC_MAX_CODE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Raw,
    Excitation,
}

impl Mode {
    const VARIANTS: [Mode; 2] = [Mode::Raw, Mode::Excitation];

    fn name(self) -> &'static str {
        match self {
            Self::Raw => "RAW",
            Self::Excitation => "EXCITATION",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::VARIANTS.into_iter().find(|m| m.name() == name)
    }
}

#[derive(Debug, Clone, Copy)]
struct PdSample {
    t_ms: u64,
    code: f64,
}

/// Parses one firmware stream line: `PD code=<f> n=<u> t_ms=<u>`.
fn parse_pd_line(line: &str) -> Option<PdSample> {
    let rest = line.trim().strip_prefix("PD ")?;
    let mut code = None;
    let mut t_ms = None;
    for token in rest.split_ascii_whitespace() {
        let (key, value) = token.split_once('=')?;
        match key {
            "code" => code = value.parse::<f64>().ok(),
            "t_ms" => t_ms = value.parse::<u64>().ok(),
            "n" => {}
            _ => return None,
        }
    }
    Some(PdSample {
        t_ms: t_ms?,
        code: code?.clamp(0.0, ADC_MAX_CODE),
    })
}

#[derive(Default)]
struct SharedState {
    samples: VecDeque<PdSample>,
    latest: Option<PdSample>,
    error: Option<String>,
}

impl SharedState {
    fn push(&mut self, sample: PdSample) {
        self.latest = Some(sample);
        self.samples.push_back(sample);
        while self.samples.len() > RING_CAPACITY {
            self.samples.pop_front();
        }
    }
}

/// Background reader owning the stream port (or the mock generator).
struct Reader {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Reader {
    fn spawn_serial(
        path: String,
        shared: Arc<Mutex<SharedState>>,
        generation: Arc<AtomicU64>,
    ) -> Result<Self, String> {
        let port = serialport::new(&path, 115_200)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|err| format!("open {path}: {err}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("stage-a-photodiode".into())
            .spawn(move || read_lines(port, &shared, &generation, &thread_stop))
            .expect("spawning the photodiode reader thread must succeed");
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    /// Hardware-free source: synthesizes a slow sine around 1 V at 50 Hz.
    fn spawn_mock(shared: Arc<Mutex<SharedState>>, generation: Arc<AtomicU64>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("stage-a-photodiode-mock".into())
            .spawn(move || {
                let start = Instant::now();
                while !thread_stop.load(Ordering::Relaxed) {
                    let t = start.elapsed().as_secs_f64();
                    let volts = 1.0 + 0.5 * (2.0 * std::f64::consts::PI * 0.2 * t).sin();
                    let sample = PdSample {
                        t_ms: (t * 1_000.0) as u64,
                        code: volts * ADC_MAX_CODE / ADC_FULL_SCALE_VOLTS,
                    };
                    if let Ok(mut state) = shared.lock() {
                        state.push(sample);
                    }
                    generation.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
            .expect("spawning the mock photodiode thread must succeed");
        Self {
            stop,
            join: Some(join),
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_lines(
    mut port: Box<dyn serialport::SerialPort>,
    shared: &Mutex<SharedState>,
    generation: &AtomicU64,
    stop: &AtomicBool,
) {
    let mut line_buffer: Vec<u8> = Vec::with_capacity(256);
    let mut buf = [0_u8; 512];
    while !stop.load(Ordering::Relaxed) {
        let read = match port.read(&mut buf) {
            Ok(0) => continue,
            Ok(read) => read,
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                if let Ok(mut state) = shared.lock() {
                    state.error = Some(format!("stream read failed: {err}"));
                }
                generation.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        line_buffer.extend_from_slice(&buf[..read]);
        // Never let garbage (e.g. the wrong, binary port) grow the buffer.
        if line_buffer.len() > 4_096 {
            line_buffer.clear();
        }
        while let Some(pos) = line_buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = line_buffer.drain(..=pos).collect();
            let Ok(text) = std::str::from_utf8(&line) else {
                continue;
            };
            if let Some(sample) = parse_pd_line(text) {
                if let Ok(mut state) = shared.lock() {
                    state.push(sample);
                }
                generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub struct StageAPhotodiodePlugin {
    enabled: bool,
    reader: Option<Reader>,
    shared: Arc<Mutex<SharedState>>,
    generation: Arc<AtomicU64>,
    effects_blocked_reason: Option<String>,
    last_error: Option<String>,
    // -- settings --
    port_hint: String,
    mode: Mode,
    reference_volts: f64,
    window_s: f64,
    consumed_action_ids: Vec<u64>,
}

impl Default for StageAPhotodiodePlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            reader: None,
            shared: Arc::new(Mutex::new(SharedState::default())),
            generation: Arc::new(AtomicU64::new(1)),
            effects_blocked_reason: None,
            last_error: None,
            port_hint: "mock".into(),
            mode: Mode::Raw,
            reference_volts: 3.3,
            window_s: 10.0,
            consumed_action_ids: Vec::new(),
        }
    }
}

impl StageAPhotodiodePlugin {
    fn connected(&self) -> bool {
        self.reader.is_some()
    }

    fn connect(&mut self) {
        if self.reader.is_some() {
            return;
        }
        if let Ok(mut state) = self.shared.lock() {
            *state = SharedState::default();
        }
        self.last_error = None;
        if self.port_hint == "mock" {
            self.reader = Some(Reader::spawn_mock(
                Arc::clone(&self.shared),
                Arc::clone(&self.generation),
            ));
            return;
        }
        let path = if self.port_hint == "auto" {
            match serial_ports().into_iter().next() {
                Some(path) => path,
                None => {
                    self.last_error =
                        Some("no USB serial device found (looked for usbmodem/ttyACM)".into());
                    return;
                }
            }
        } else {
            self.port_hint.clone()
        };
        match Reader::spawn_serial(path, Arc::clone(&self.shared), Arc::clone(&self.generation)) {
            Ok(reader) => self.reader = Some(reader),
            Err(err) => self.last_error = Some(err),
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn disconnect(&mut self) {
        self.reader = None; // Drop joins the thread.
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Value shown for one sample under the current mode, in volts.
    fn display_volts(&self, code: f64) -> f64 {
        match self.mode {
            Mode::Raw => code_to_volts(code),
            Mode::Excitation => self.reference_volts - code_to_volts(code),
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
            if !request.action_id.starts_with("stage-a-photodiode.") {
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

    fn series_dataset(&self) -> Series1dV1 {
        let (points, y_label) = match self.shared.lock() {
            Ok(state) => {
                let latest_ms = state.latest.map_or(0, |s| s.t_ms);
                let window_ms = (self.window_s.max(0.5) * 1_000.0) as u64;
                let cutoff = latest_ms.saturating_sub(window_ms);
                let points: Vec<Series1dPoint> = state
                    .samples
                    .iter()
                    .filter(|s| s.t_ms >= cutoff)
                    .map(|s| Series1dPoint {
                        x: (s.t_ms as f64 - latest_ms as f64) / 1_000.0,
                        y: self.display_volts(s.code),
                    })
                    .collect();
                let label = match self.mode {
                    Mode::Raw => "photodiode [V]",
                    Mode::Excitation => "excitation I_tot − I_pd [V]",
                };
                (points, label)
            }
            Err(_) => (Vec::new(), "photodiode [V]"),
        };
        Series1dV1 {
            x_label: "time before now [s]".into(),
            y_label: y_label.into(),
            lines: vec![Series1dLine {
                name: match self.mode {
                    Mode::Raw => "photodiode".into(),
                    Mode::Excitation => "excitation".into(),
                },
                points,
            }],
        }
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let (latest, stream_error) = match self.shared.lock() {
            Ok(state) => (state.latest, state.error.clone()),
            Err(_) => (None, None),
        };
        let state = match (&self.effects_blocked_reason, self.connected()) {
            (Some(reason), _) => format!("locked ({reason})"),
            (None, false) => "disconnected".into(),
            (None, true) => format!("reading ({})", self.port_hint),
        };
        let (code_text, value_text) = match latest {
            Some(sample) => (
                format!("{:.1}", sample.code),
                format!("{:.4} V", self.display_volts(sample.code)),
            ),
            None => ("—".into(), "—".into()),
        };
        let error = stream_error
            .or_else(|| self.last_error.clone())
            .unwrap_or_default();
        let text_column = |id: &str, value: String| TableColumnData {
            column_id: id.to_owned(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                text_column("state", state),
                text_column("mode", self.mode.name().to_owned()),
                text_column("code", code_text),
                text_column("value", value_text),
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
                column("mode", "Mode"),
                column("code", "ADC code"),
                column("value", "Value"),
                column("error", "Last error"),
            ],
            ..TableSchema::default()
        }
    }
}

fn serial_ports() -> Vec<String> {
    serialport::available_ports()
        .map(|ports| {
            ports
                .into_iter()
                .map(|p| p.port_name)
                .filter(|name| name.contains("usbmodem") || name.contains("ttyACM"))
                .collect()
        })
        .unwrap_or_default()
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

impl Plugin for StageAPhotodiodePlugin {
    fn name(&self) -> &'static str {
        "Stage-A Photodiode"
    }

    fn description(&self) -> &'static str {
        "Live photodiode readout (SMA5/pin 18/A4) from the Teensy stream port: raw values or excitation power I_exc = I_tot − I_pd with a user-set reference."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.disconnect();
        }
    }

    fn reset(&mut self) {
        if let Ok(mut state) = self.shared.lock() {
            state.samples.clear();
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        // The stream port is read-only, but device access still follows the
        // same fail-closed gate as every stage-a plugin.
        let execution = context.execution();
        if !execution.hardware_effects_allowed() {
            self.effects_blocked_reason = Some(format!(
                "hardware effects not allowed in {:?}",
                execution.mode
            ));
            if self.reader.is_some() {
                self.disconnect();
            }
            return;
        }
        self.effects_blocked_reason = None;

        for action_id in self.consume_actions(context) {
            match action_id.as_str() {
                ACTION_CONNECT => self.connect(),
                ACTION_DISCONNECT => self.disconnect(),
                _ => {}
            }
        }
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
                label: "Photodiode readout".into(),
                description: Some(
                    "Reads the free-running PD stream on the Teensy's SECOND serial port. \
                     EXCITATION shows I_exc = I_tot − I_pd: the diode sits behind the PBS and \
                     sees the light removed from the excitation beam."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "port".into(),
                        label: "Port".into(),
                        tooltip: Some(
                            "Teensy stream port (the SECOND usbmodem port); mock = synthetic \
                             data, auto = first device"
                                .into(),
                        ),
                        kind: SettingKind::Enum {
                            variants: port_variants,
                            default: port_default,
                        },
                    },
                    SettingItem {
                        key: "mode".into(),
                        label: "Mode".into(),
                        tooltip: Some(
                            "RAW: ADC code and volts as measured. EXCITATION: I_tot − I_pd".into(),
                        ),
                        kind: SettingKind::Enum {
                            variants: mode_variants,
                            default: mode_default,
                        },
                    },
                    SettingItem {
                        key: "reference_volts".into(),
                        label: "Reference I_tot".into(),
                        tooltip: Some(
                            "Total power reference for EXCITATION mode, in photodiode volts: \
                             the PD reading with the full beam diverted into the diode"
                                .into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 0.0,
                            max: ADC_FULL_SCALE_VOLTS,
                            speed: 0.01,
                            default: self.reference_volts,
                        },
                    },
                    SettingItem {
                        key: "window_s".into(),
                        label: "Chart window".into(),
                        tooltip: Some("Seconds of history shown in the live chart".into()),
                        kind: SettingKind::F64Drag {
                            min: 1.0,
                            max: 120.0,
                            speed: 1.0,
                            default: self.window_s,
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
            "mode" => {
                let index = Mode::VARIANTS
                    .iter()
                    .position(|m| *m == self.mode)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "reference_volts" => Some(json!(self.reference_volts)),
            "window_s" => Some(json!(self.window_s)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "port" => {
                self.port_hint = enum_choice(&value, &port_variants())?;
                Ok(())
            }
            "mode" => {
                let mode_names: Vec<String> =
                    Mode::VARIANTS.iter().map(|m| m.name().to_owned()).collect();
                let name = enum_choice(&value, &mode_names)?;
                self.mode = Mode::from_name(&name)
                    .ok_or_else(|| format!("unknown mode: {name} (RAW/EXCITATION)"))?;
                Ok(())
            }
            "reference_volts" => {
                let volts = value.as_f64().ok_or("reference_volts must be a number")?;
                self.reference_volts = volts.clamp(0.0, ADC_FULL_SCALE_VOLTS);
                Ok(())
            }
            "window_s" => {
                let seconds = value.as_f64().ok_or("window_s must be a number")?;
                self.window_s = seconds.clamp(1.0, 120.0);
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
        let (latest, stream_error) = match self.shared.lock() {
            Ok(state) => (state.latest, state.error.clone()),
            Err(_) => (None, None),
        };
        entries.push(StatusEntry::Text(if self.connected() {
            format!("Photodiode: reading ({})", self.port_hint)
        } else {
            "Photodiode: disconnected".into()
        }));
        if let Some(sample) = latest {
            match self.mode {
                Mode::Raw => entries.push(StatusEntry::Text(format!(
                    "PD: code={:.1} ({:.4} V)",
                    sample.code,
                    code_to_volts(sample.code)
                ))),
                Mode::Excitation => entries.push(StatusEntry::Text(format!(
                    "Excitation: {:.4} V (I_tot={:.3} V, PD={:.4} V)",
                    self.display_volts(sample.code),
                    self.reference_volts,
                    code_to_volts(sample.code)
                ))),
            }
        }
        if let Some(error) = stream_error.or_else(|| self.last_error.clone()) {
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
            datasets: vec![
                HostDatasetDescriptor {
                    id: SERIES_DATASET_ID.into(),
                    title: "Photodiode trace".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "No photodiode samples yet — connect the stream port.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: STATUS_DATASET_ID.into(),
                    title: "Photodiode readout".into(),
                    kind: HostDatasetKind::TableV1(self.status_schema()),
                    empty_message: "Photodiode readout idle.".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: SERIES_VIEW_ID.into(),
                    title: "Photodiode".into(),
                    dataset_id: SERIES_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: STATUS_VIEW_ID.into(),
                    title: "Photodiode readout".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
            ],
            actions: vec![
                action(ACTION_CONNECT, "Connect"),
                action(ACTION_DISCONNECT, "Disconnect"),
            ],
        }
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            SERIES_DATASET_ID => serde_json::to_vec(&self.series_dataset()).ok(),
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            SERIES_DATASET_ID | STATUS_DATASET_ID => self.generation.load(Ordering::Relaxed).max(1),
            _ => 0,
        }
    }
}

export_plugin!(StageAPhotodiodePlugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_firmware_stream_lines() {
        let sample = parse_pd_line("PD code=1042.3 n=16 t_ms=123456\n").expect("valid line");
        assert!((sample.code - 1042.3).abs() < 1e-9);
        assert_eq!(sample.t_ms, 123_456);

        assert!(parse_pd_line("garbage").is_none());
        assert!(parse_pd_line("PD code=abc n=16 t_ms=1").is_none());
        assert!(parse_pd_line("PD code=10 n=16").is_none(), "t_ms required");
        // Codes are clamped into the 12-bit range.
        let clamped = parse_pd_line("PD code=9999 n=1 t_ms=5").expect("parses");
        assert_eq!(clamped.code, ADC_MAX_CODE);
    }

    #[test]
    fn excitation_mode_inverts_against_the_reference() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("mode", json!("EXCITATION")).unwrap();
        plugin.set_setting("reference_volts", json!(2.0)).unwrap();
        // I_pd = 0.5 V → I_exc = I_tot − I_pd = 1.5 V.
        let code = 0.5 * ADC_MAX_CODE / ADC_FULL_SCALE_VOLTS;
        assert!((plugin.display_volts(code) - 1.5).abs() < 1e-9);
        // RAW mode shows the measured voltage itself.
        plugin.set_setting("mode", json!("RAW")).unwrap();
        assert!((plugin.display_volts(code) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mock_reader_fills_the_ring_and_series() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.connect();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count = plugin.shared.lock().unwrap().samples.len();
            if count >= 5 {
                break;
            }
            assert!(Instant::now() < deadline, "mock reader produced no data");
            std::thread::sleep(Duration::from_millis(5));
        }
        let series = plugin.series_dataset();
        assert!(!series.lines[0].points.is_empty());
        let generation = plugin.generation.load(Ordering::Relaxed);
        assert!(generation > 1);
        plugin.disconnect();
    }

    /// The host settings UI exchanges enum values as indices into the
    /// schema's variant list (radio buttons send `json!(index)`).
    #[test]
    fn enum_settings_round_trip_as_indices() {
        let mut plugin = StageAPhotodiodePlugin::default();
        // Mode: index 1 = EXCITATION.
        plugin
            .set_setting("mode", json!(1))
            .expect("index accepted");
        assert_eq!(plugin.mode, Mode::Excitation);
        assert_eq!(plugin.get_setting("mode"), Some(json!(1)));
        // Port: index 1 = "auto" (variants start with mock, auto).
        plugin
            .set_setting("port", json!(1))
            .expect("index accepted");
        assert_eq!(plugin.port_hint, "auto");
        assert_eq!(plugin.get_setting("port"), Some(json!(1)));
        assert!(plugin.set_setting("mode", json!(99)).is_err());
        // String names keep working (tests, saved configs).
        plugin
            .set_setting("mode", json!("RAW"))
            .expect("name accepted");
        assert_eq!(plugin.mode, Mode::Raw);
    }

    #[test]
    fn ring_is_bounded() {
        let mut state = SharedState::default();
        for i in 0..(RING_CAPACITY + 100) {
            state.push(PdSample {
                t_ms: i as u64,
                code: 1.0,
            });
        }
        assert_eq!(state.samples.len(), RING_CAPACITY);
        assert_eq!(state.latest.unwrap().t_ms, (RING_CAPACITY + 99) as u64);
    }
}
