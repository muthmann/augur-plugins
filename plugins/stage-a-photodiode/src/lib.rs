//! Stage-A photodiode readout.
//!
//! Reads the free-running PDA1 binary frame stream the `stage-a-controller`
//! firmware (0.4.0+, `USB_DUAL_SERIAL`) emits on its **second** USB serial
//! port: `SamplesU16` frames at `pd_stream_rate_hz` (20 kSa/s default) from
//! the photodiode on board SMA5 → Teensy pin 18 / A4. The port carries no
//! commands, so opening it is side-effect free; the command port is owned by
//! `stage-a-modulation`. While a command-port acquisition runs the firmware
//! mirrors its blocks here (flag 0x0001) — every rate change or sample-index
//! jump is treated as a segment restart.
//!
//! Two display modes:
//! - **RAW**: the ADC code and its voltage (`V = code · 3.3 / 4095`);
//! - **EXCITATION**: the photodiode sits behind the PBS in the excitation
//!   path and sees the light removed from the beam, `I_pd = I_tot − I_exc`.
//!   Given the user-set reference `I_tot` (in photodiode volts), the plugin
//!   shows `I_exc = I_tot − V_pd`.
//!
//! The chart decimates the visible window into min/mean/max envelope buckets
//! and overlays a moving average whose window is either a fixed sample count
//! or — for modulated signals — one full period of a user-given frequency,
//! which makes the mean independent of the modulation phase.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostContext, HostDatasetDescriptor, HostDatasetKind,
    HostOutput, HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry, Plugin,
    PluginFrame, Series1dLine, Series1dPoint, Series1dV1, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1,
    TableSchema, TableValueType,
};
use serde_json::{json, Value};
use stage_a_io::{FrameParser, ParseEvent};

const SERIES_DATASET_ID: &str = "stage-a-photodiode.series";
const SERIES_VIEW_ID: &str = "stage-a-photodiode.series.view";
const STATUS_DATASET_ID: &str = "stage-a-photodiode.status";
const STATUS_VIEW_ID: &str = "stage-a-photodiode.status.view";

const ADC_FULL_SCALE_VOLTS: f64 = 3.3;
const ADC_MAX_CODE: f64 = 4_095.0;
/// Longest raw history kept, in seconds of samples at the active stream rate.
const RING_SECONDS: f64 = 130.0;
/// Absolute sample cap guarding against absurd advertised rates (8 MiB of
/// codes at most).
const RING_MAX_SAMPLES: usize = 4_000_000;
/// Envelope buckets per rendered chart line; keeps the plot payload bounded
/// no matter how many raw samples the window covers.
const MAX_PLOT_BUCKETS: usize = 1_000;
/// The firmware's default stream rate; the mock mirrors it.
const MOCK_RATE_HZ: u32 = 20_000;
const MOCK_BLOCK_SAMPLES: usize = 256;

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

#[derive(Default)]
struct SharedState {
    /// Sample rate of the current segment (from the frame headers).
    rate_hz: u32,
    /// Device sample index of `samples.front()` within the current segment.
    ring_first_index: u64,
    samples: VecDeque<u16>,
    latest: Option<u16>,
    /// Cumulative firmware-side drop counter (latest header value).
    device_dropped: u32,
    crc_failures: u64,
    resync_bytes: u64,
    /// Segment restarts observed (rate changes, index jumps, reconnects).
    segments: u64,
    error: Option<String>,
}

impl SharedState {
    fn ring_capacity(rate_hz: u32) -> usize {
        ((f64::from(rate_hz.max(1)) * RING_SECONDS) as usize).min(RING_MAX_SAMPLES)
    }

    /// Ingests one `SamplesU16` frame. Any discontinuity — rate change,
    /// sample-index jump (drops, acquisition handover), reconnect — restarts
    /// the ring: within a segment `index / rate` is a consistent time base.
    fn ingest(&mut self, first_index: u64, rate_hz: u32, device_dropped: u32, codes: &[u16]) {
        if codes.is_empty() {
            return;
        }
        let expected = self.ring_first_index + self.samples.len() as u64;
        let continuous =
            !self.samples.is_empty() && rate_hz == self.rate_hz && first_index == expected;
        if !continuous {
            if !self.samples.is_empty() {
                self.segments += 1;
            }
            self.samples.clear();
            self.ring_first_index = first_index;
            self.rate_hz = rate_hz;
        }
        self.samples.extend(codes.iter().copied());
        self.latest = codes.last().copied();
        self.device_dropped = device_dropped;
        let excess = self
            .samples
            .len()
            .saturating_sub(Self::ring_capacity(rate_hz));
        if excess > 0 {
            self.samples.drain(..excess);
            self.ring_first_index += excess as u64;
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
            .spawn(move || read_frames(port, &shared, &generation, &thread_stop))
            .expect("spawning the photodiode reader thread must succeed");
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    /// Hardware-free source: synthesizes a noisy 5 Hz sine around 1 V in
    /// firmware-sized blocks at the firmware's default stream rate.
    fn spawn_mock(shared: Arc<Mutex<SharedState>>, generation: Arc<AtomicU64>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("stage-a-photodiode-mock".into())
            .spawn(move || {
                let start = Instant::now();
                let mut next_index: u64 = 0;
                while !thread_stop.load(Ordering::Relaxed) {
                    let target = (start.elapsed().as_secs_f64() * f64::from(MOCK_RATE_HZ)) as u64;
                    let mut produced = false;
                    while next_index + MOCK_BLOCK_SAMPLES as u64 <= target {
                        let codes: Vec<u16> = (0..MOCK_BLOCK_SAMPLES)
                            .map(|i| mock_code(next_index + i as u64))
                            .collect();
                        if let Ok(mut state) = shared.lock() {
                            state.ingest(next_index, MOCK_RATE_HZ, 0, &codes);
                        }
                        next_index += MOCK_BLOCK_SAMPLES as u64;
                        produced = true;
                    }
                    if produced {
                        generation.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .expect("spawning the mock photodiode thread must succeed");
        Self {
            stop,
            join: Some(join),
        }
    }
}

/// Deterministic mock sample: 1 V ± 0.5 V sine at 5 Hz plus ~20 mV of hash
/// noise, so the moving-average indicator has something to smooth.
fn mock_code(index: u64) -> u16 {
    let t = index as f64 / f64::from(MOCK_RATE_HZ);
    let mut hash = index.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    hash ^= hash >> 33;
    let noise = (hash as f64 / u64::MAX as f64) - 0.5;
    let volts = 1.0 + 0.5 * (2.0 * std::f64::consts::PI * 5.0 * t).sin() + 0.04 * noise;
    (volts * ADC_MAX_CODE / ADC_FULL_SCALE_VOLTS).clamp(0.0, ADC_MAX_CODE) as u16
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn read_frames(
    mut port: Box<dyn serialport::SerialPort>,
    shared: &Mutex<SharedState>,
    generation: &AtomicU64,
    stop: &AtomicBool,
) {
    let mut parser = FrameParser::default();
    let mut buf = [0_u8; 4_096];
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
        parser.extend(&buf[..read]);
        let mut changed = false;
        while let Some(event) = parser.next_event() {
            match event {
                ParseEvent::Frame(frame) => {
                    let Some(codes) = frame.samples() else {
                        continue; // Control/summary frames are not expected here.
                    };
                    if let Ok(mut state) = shared.lock() {
                        state.ingest(
                            frame.header.first_sample_index,
                            frame.header.sample_rate_hz,
                            frame.header.dropped_samples,
                            &codes,
                        );
                    }
                    changed = true;
                }
                ParseEvent::Corruption {
                    skipped_bytes,
                    crc_failures,
                } => {
                    if let Ok(mut state) = shared.lock() {
                        state.resync_bytes += skipped_bytes as u64;
                        state.crc_failures += crc_failures as u64;
                    }
                    changed = true;
                }
            }
        }
        if changed {
            generation.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub struct StageAPhotodiodePlugin {
    enabled: bool,
    reader: Option<Reader>,
    shared: Arc<Mutex<SharedState>>,
    generation: Arc<AtomicU64>,
    last_error: Option<String>,
    // -- settings --
    connect_requested: bool,
    port_hint: String,
    mode: Mode,
    reference_volts: f64,
    window_s: f64,
    avg_samples: usize,
    avg_sync_freq_hz: f64,
}

impl Default for StageAPhotodiodePlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            reader: None,
            shared: Arc::new(Mutex::new(SharedState::default())),
            generation: Arc::new(AtomicU64::new(1)),
            last_error: None,
            connect_requested: false,
            port_hint: "auto".into(),
            mode: Mode::Raw,
            reference_volts: 3.3,
            window_s: 10.0,
            avg_samples: 4,
            avg_sync_freq_hz: 0.0,
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
            match resolve_auto_port() {
                Ok(path) => path,
                Err(err) => {
                    self.last_error = Some(err);
                    return;
                }
            }
        } else {
            self.port_hint.clone()
        };
        match Reader::spawn_serial(path, Arc::clone(&self.shared), Arc::clone(&self.generation)) {
            Ok(reader) => self.reader = Some(reader),
            Err(err) => {
                self.last_error = Some(err);
                self.connect_requested = false;
            }
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

    /// Moving-average window in samples: either the fixed sample count or,
    /// when a sync frequency is set, one full period of that frequency —
    /// which makes the mean independent of the modulation phase.
    fn avg_window_samples(&self, rate_hz: u32) -> usize {
        if self.avg_sync_freq_hz > 0.0 && rate_hz > 0 {
            (f64::from(rate_hz) / self.avg_sync_freq_hz)
                .round()
                .max(1.0) as usize
        } else {
            self.avg_samples.max(1)
        }
    }

    /// Mean of the newest `avg_window_samples` codes (fewer while filling).
    fn current_average_code(&self, state: &SharedState) -> Option<f64> {
        if state.samples.is_empty() {
            return None;
        }
        let window = self
            .avg_window_samples(state.rate_hz)
            .min(state.samples.len());
        let start = state.samples.len() - window;
        let sum: u64 = state.samples.range(start..).map(|&c| u64::from(c)).sum();
        Some(sum as f64 / window as f64)
    }

    fn series_dataset(&self) -> Series1dV1 {
        let y_label = match self.mode {
            Mode::Raw => "photodiode [V]",
            Mode::Excitation => "excitation I_tot − I_pd [V]",
        };
        let trace_name = match self.mode {
            Mode::Raw => "photodiode",
            Mode::Excitation => "excitation",
        };
        let empty = |label: &str| Series1dV1 {
            x_label: "time before now [s]".into(),
            y_label: label.into(),
            lines: vec![Series1dLine {
                name: trace_name.into(),
                points: Vec::new(),
            }],
        };
        let Ok(state) = self.shared.lock() else {
            return empty(y_label);
        };
        let total = state.samples.len();
        if total == 0 || state.rate_hz == 0 {
            return empty(y_label);
        }
        let rate = f64::from(state.rate_hz);

        let visible = ((self.window_s.max(0.001) * rate) as usize)
            .max(2)
            .min(total);
        let start = total - visible;
        let latest_x_index = state.ring_first_index + total as u64 - 1;
        let bucket_len = visible.div_ceil(MAX_PLOT_BUCKETS).max(1);
        let decimating = bucket_len > 1;

        let avg_window = self.avg_window_samples(state.rate_hz);
        let avg_enabled = avg_window > 1;
        // Prime the running sum with up to `avg_window − 1` samples that
        // precede the visible slice, so the average is correct from the
        // first visible point on.
        let prime_start = start.saturating_sub(avg_window - 1);
        let mut avg_sum: u64 = 0;
        let mut avg_count: usize = 0;
        for &code in state.samples.range(prime_start..start) {
            avg_sum += u64::from(code);
            avg_count += 1;
        }

        let mut mean_points = Vec::with_capacity(MAX_PLOT_BUCKETS + 1);
        let mut min_points = Vec::with_capacity(if decimating { MAX_PLOT_BUCKETS + 1 } else { 0 });
        let mut max_points = Vec::with_capacity(if decimating { MAX_PLOT_BUCKETS + 1 } else { 0 });
        let mut avg_points = Vec::with_capacity(if avg_enabled { MAX_PLOT_BUCKETS + 1 } else { 0 });

        let mut bucket_min = u16::MAX;
        let mut bucket_max = u16::MIN;
        let mut bucket_sum: u64 = 0;
        let mut bucket_n: usize = 0;
        for (offset, &code) in state.samples.range(start..).enumerate() {
            let i = start + offset;
            bucket_min = bucket_min.min(code);
            bucket_max = bucket_max.max(code);
            bucket_sum += u64::from(code);
            bucket_n += 1;
            if avg_enabled {
                avg_sum += u64::from(code);
                avg_count += 1;
                if avg_count > avg_window {
                    avg_sum -= u64::from(state.samples[i - avg_window]);
                    avg_count -= 1;
                }
            }
            if bucket_n == bucket_len || i == total - 1 {
                let x = (state.ring_first_index + i as u64) as f64 / rate
                    - latest_x_index as f64 / rate;
                mean_points.push(Series1dPoint {
                    x,
                    y: self.display_volts(bucket_sum as f64 / bucket_n as f64),
                });
                if decimating {
                    // EXCITATION inverts the axis, so min/max swap roles.
                    let (low, high) = (
                        self.display_volts(f64::from(bucket_min)),
                        self.display_volts(f64::from(bucket_max)),
                    );
                    min_points.push(Series1dPoint {
                        x,
                        y: low.min(high),
                    });
                    max_points.push(Series1dPoint {
                        x,
                        y: low.max(high),
                    });
                }
                if avg_enabled {
                    avg_points.push(Series1dPoint {
                        x,
                        y: self.display_volts(avg_sum as f64 / avg_count as f64),
                    });
                }
                bucket_min = u16::MAX;
                bucket_max = u16::MIN;
                bucket_sum = 0;
                bucket_n = 0;
            }
        }

        let mut lines = vec![Series1dLine {
            name: trace_name.into(),
            points: mean_points,
        }];
        if decimating {
            lines.push(Series1dLine {
                name: "min".into(),
                points: min_points,
            });
            lines.push(Series1dLine {
                name: "max".into(),
                points: max_points,
            });
        }
        if avg_enabled {
            lines.push(Series1dLine {
                name: format!("avg ({avg_window} spl)"),
                points: avg_points,
            });
        }
        Series1dV1 {
            x_label: "time before now [s]".into(),
            y_label: y_label.into(),
            lines,
        }
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let (latest, rate_hz, average, integrity, stream_error) = match self.shared.lock() {
            Ok(state) => (
                state.latest,
                state.rate_hz,
                self.current_average_code(&state),
                format!(
                    "drops={} crc={} resync={} segments={}",
                    state.device_dropped, state.crc_failures, state.resync_bytes, state.segments
                ),
                state.error.clone(),
            ),
            Err(_) => (None, 0, None, String::new(), None),
        };
        let state_text = if self.connected() {
            format!("reading ({})", self.port_hint)
        } else {
            "disconnected".into()
        };
        let rate_text = if rate_hz > 0 {
            format!("{rate_hz} Sa/s")
        } else {
            "—".into()
        };
        let (code_text, value_text) = match latest {
            Some(sample) => (
                format!("{sample}"),
                format!("{:.4} V", self.display_volts(f64::from(sample))),
            ),
            None => ("—".into(), "—".into()),
        };
        let avg_text = match average {
            Some(code) => {
                let window = self.avg_window_samples(rate_hz);
                format!(
                    "{:.4} V ({} spl ≈ {:.2} ms)",
                    self.display_volts(code),
                    window,
                    if rate_hz > 0 {
                        window as f64 * 1_000.0 / f64::from(rate_hz)
                    } else {
                        0.0
                    }
                )
            }
            None => "—".into(),
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
                text_column("state", state_text),
                text_column("mode", self.mode.name().to_owned()),
                text_column("rate", rate_text),
                text_column("code", code_text),
                text_column("value", value_text),
                text_column("avg", avg_text),
                text_column("integrity", integrity),
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
                column("rate", "Rate"),
                column("code", "ADC code"),
                column("value", "Value"),
                column("avg", "Moving avg"),
                column("integrity", "Integrity"),
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
                // macOS lists each device twice; use the callout (cu.*) node only.
                .filter(|name| name.contains("cu.usbmodem") || name.contains("ttyACM"))
                .collect()
        })
        .unwrap_or_default()
}

/// Finds the Teensy stream port: the dual-serial firmware free-runs PDA1
/// `SamplesU16` frames on exactly one of the enumerated ports, so listen
/// briefly on each.
fn resolve_auto_port() -> Result<String, String> {
    let candidates = serial_ports();
    if candidates.is_empty() {
        return Err("no USB serial device found (looked for usbmodem/ttyACM)".to_owned());
    }
    for path in &candidates {
        if probe_pd_stream(path) {
            return Ok(path.clone());
        }
    }
    Err(format!(
        "no port streamed PDA1 sample frames within 500 ms (tried {})",
        candidates.join(", ")
    ))
}

/// True when `path` produces a CRC-clean `SamplesU16` frame within the probe
/// window. The command port emits frames too, but only control replies and
/// acquisition data — unsolicited sample frames identify the stream port.
fn probe_pd_stream(path: &str) -> bool {
    let Ok(mut port) = serialport::new(path, 115_200)
        .timeout(Duration::from_millis(100))
        .open()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut parser = FrameParser::default();
    let mut buf = [0_u8; 4_096];
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(read) if read > 0 => {
                parser.extend(&buf[..read]);
                while let Some(event) = parser.next_event() {
                    if let ParseEvent::Frame(frame) = event {
                        if frame.samples().is_some() {
                            return true;
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
    }
    false
}

/// The exact variant list the settings schema shows for the port enum — the
/// host exchanges enum settings as indices into this list.
fn port_variants() -> Vec<String> {
    let mut variants = vec!["mock".to_owned(), "auto".to_owned()];
    for port in serialport::available_ports().unwrap_or_default() {
        if !(port.port_name.contains("cu.usbmodem") || port.port_name.contains("ttyACM")) {
            continue;
        }
        let label = match port.port_type {
            serialport::SerialPortType::UsbPort(info) => match (info.manufacturer, info.product) {
                (Some(manufacturer), Some(product)) if !product.starts_with(&manufacturer) => {
                    Some(format!("{manufacturer} {product}"))
                }
                (_, Some(product)) => Some(product),
                (Some(manufacturer), None) => Some(manufacturer),
                (None, None) => None,
            },
            _ => None,
        };
        variants.push(match label {
            Some(label) => format!("{} ({label})", port.port_name),
            None => port.port_name,
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

impl Plugin for StageAPhotodiodePlugin {
    fn name(&self) -> &'static str {
        "Stage-A Photodiode"
    }

    fn description(&self) -> &'static str {
        "Live photodiode readout (SMA5/pin 18/A4) from the Teensy PDA1 stream port at the full stream rate: raw values or excitation power I_exc = I_tot − I_pd with a user-set reference."
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
        _context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        // Reading is settings-driven (connect checkbox) and works without
        // camera frames; the stream port carries no commands, so no replay
        // teardown is needed either.
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
                label: "Photodiode readout".into(),
                description: Some(
                    "Reads the free-running PDA1 sample stream on the Teensy's SECOND serial \
                     port (firmware 0.4.0+, 20 kSa/s default). EXCITATION shows \
                     I_exc = I_tot − I_pd: the diode sits behind the PBS and sees the light \
                     removed from the excitation beam."
                        .into(),
                ),
                default_open: true,
                items: vec![
                    SettingItem {
                        key: "port".into(),
                        label: "Port".into(),
                        tooltip: Some(
                            "auto (recommended) listens on the attached usbmodem ports and \
                             picks the one streaming PDA1 sample frames — the Teensy stream \
                             port; mock = synthetic data"
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
                            "Opens/closes the stream port (read-only, no camera required).".into(),
                        ),
                        kind: SettingKind::Bool {
                            default: self.connect_requested,
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
                        tooltip: Some(
                            "Seconds of history shown in the live chart. Short windows \
                             (≤ 50 ms) resolve individual modulation cycles at 20 kSa/s."
                                .into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 0.01,
                            max: 120.0,
                            speed: 0.05,
                            default: self.window_s,
                        },
                    },
                    SettingItem {
                        key: "avg_samples".into(),
                        label: "Average window".into(),
                        tooltip: Some(
                            "Moving-average window in samples (1 = off). Ignored while \
                              'Average sync frequency' is set."
                                .into(),
                        ),
                        kind: SettingKind::I64Drag {
                            min: 1,
                            max: 1_000_000,
                            default: self.avg_samples as i64,
                        },
                    },
                    SettingItem {
                        key: "avg_sync_freq_hz".into(),
                        label: "Average sync frequency".into(),
                        tooltip: Some(
                            "0 = off. When set to the modulation frequency (Hz), the moving \
                             average spans exactly one full period (window = rate / f), so the \
                             mean level no longer depends on the modulation phase."
                                .into(),
                        ),
                        kind: SettingKind::F64Drag {
                            min: 0.0,
                            max: 100_000.0,
                            speed: 1.0,
                            default: self.avg_sync_freq_hz,
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
            "mode" => {
                let index = Mode::VARIANTS
                    .iter()
                    .position(|m| *m == self.mode)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "reference_volts" => Some(json!(self.reference_volts)),
            "window_s" => Some(json!(self.window_s)),
            "avg_samples" => Some(json!(self.avg_samples)),
            "avg_sync_freq_hz" => Some(json!(self.avg_sync_freq_hz)),
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
                self.window_s = seconds.clamp(0.01, 120.0);
                Ok(())
            }
            "avg_samples" => {
                let samples = value.as_i64().ok_or("avg_samples must be an integer")?;
                self.avg_samples = samples.clamp(1, 1_000_000) as usize;
                Ok(())
            }
            "avg_sync_freq_hz" => {
                let freq = value.as_f64().ok_or("avg_sync_freq_hz must be a number")?;
                self.avg_sync_freq_hz = freq.clamp(0.0, 100_000.0);
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        let (latest, rate_hz, average, stream_error) = match self.shared.lock() {
            Ok(state) => (
                state.latest,
                state.rate_hz,
                self.current_average_code(&state),
                state.error.clone(),
            ),
            Err(_) => (None, 0, None, None),
        };
        entries.push(StatusEntry::Text(if self.connected() {
            if rate_hz > 0 {
                format!("Photodiode: reading ({}) @ {rate_hz} Sa/s", self.port_hint)
            } else {
                format!("Photodiode: reading ({})", self.port_hint)
            }
        } else {
            "Photodiode: disconnected".into()
        }));
        if let Some(sample) = latest {
            match self.mode {
                Mode::Raw => entries.push(StatusEntry::Text(format!(
                    "PD: code={sample} ({:.4} V)",
                    code_to_volts(f64::from(sample))
                ))),
                Mode::Excitation => entries.push(StatusEntry::Text(format!(
                    "Excitation: {:.4} V (I_tot={:.3} V, PD={:.4} V)",
                    self.display_volts(f64::from(sample)),
                    self.reference_volts,
                    code_to_volts(f64::from(sample))
                ))),
            }
        }
        if let Some(average) = average {
            let window = self.avg_window_samples(rate_hz);
            if window > 1 {
                entries.push(StatusEntry::Text(format!(
                    "Avg ({window} spl): {:.4} V",
                    self.display_volts(average)
                )));
            }
        }
        if let Some(error) = stream_error.or_else(|| self.last_error.clone()) {
            entries.push(StatusEntry::Text(format!("Error: {error}")));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
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
            actions: Vec::new(),
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
    use stage_a_io::{Frame, FrameHeader, FrameType};

    fn sample_frame(sequence: u32, first_index: u64, rate_hz: u32, codes: &[u16]) -> Vec<u8> {
        let payload: Vec<u8> = codes.iter().flat_map(|c| c.to_le_bytes()).collect();
        Frame::build(
            FrameHeader {
                version: stage_a_io::wire::PROTOCOL_VERSION,
                frame_type: FrameType::SamplesU16,
                flags: 0,
                sequence,
                payload_bytes: 0,
                first_sample_index: first_index,
                sample_rate_hz: rate_hz,
                dropped_samples: 0,
                crc32: 0,
            },
            payload,
        )
        .to_bytes()
    }

    fn ingest_bytes(state: &mut SharedState, bytes: &[u8]) {
        let mut parser = FrameParser::default();
        parser.extend(bytes);
        while let Some(event) = parser.next_event() {
            match event {
                ParseEvent::Frame(frame) => {
                    let codes = frame.samples().expect("sample frame");
                    state.ingest(
                        frame.header.first_sample_index,
                        frame.header.sample_rate_hz,
                        frame.header.dropped_samples,
                        &codes,
                    );
                }
                ParseEvent::Corruption { .. } => panic!("clean test stream"),
            }
        }
    }

    #[test]
    fn ingests_contiguous_frames_and_restarts_on_gaps() {
        let mut state = SharedState::default();
        ingest_bytes(&mut state, &sample_frame(0, 0, 20_000, &[1, 2, 3, 4]));
        ingest_bytes(&mut state, &sample_frame(1, 4, 20_000, &[5, 6]));
        assert_eq!(state.samples.len(), 6);
        assert_eq!(state.ring_first_index, 0);
        assert_eq!(state.segments, 0);
        assert_eq!(state.latest, Some(6));

        // A sample-index jump (dropped block, acquisition handover) restarts
        // the segment instead of silently misaligning the time base.
        ingest_bytes(&mut state, &sample_frame(2, 100, 20_000, &[7, 8]));
        assert_eq!(state.samples.len(), 2);
        assert_eq!(state.ring_first_index, 100);
        assert_eq!(state.segments, 1);

        // So does a rate change (mirrored acquisition at another rate).
        ingest_bytes(&mut state, &sample_frame(3, 102, 50_000, &[9]));
        assert_eq!(state.samples.len(), 1);
        assert_eq!(state.rate_hz, 50_000);
        assert_eq!(state.segments, 2);
    }

    #[test]
    fn ring_is_bounded_by_duration() {
        let mut state = SharedState::default();
        let rate = 1_000; // capacity = 130_000 samples
        let cap = SharedState::ring_capacity(rate);
        let block: Vec<u16> = (0..1_000).map(|i| (i % 4_096) as u16).collect();
        let mut index = 0_u64;
        for _ in 0..(cap / block.len() + 5) {
            state.ingest(index, rate, 0, &block);
            index += block.len() as u64;
        }
        assert_eq!(state.samples.len(), cap);
        assert_eq!(
            state.ring_first_index + state.samples.len() as u64,
            index,
            "eviction keeps indexes aligned"
        );
        assert_eq!(state.segments, 0, "eviction is not a discontinuity");
    }

    #[test]
    fn moving_average_window_follows_the_sync_frequency() {
        let mut plugin = StageAPhotodiodePlugin::default();
        assert_eq!(plugin.avg_window_samples(20_000), 4, "sample default");
        plugin
            .set_setting("avg_samples", json!(16))
            .expect("valid setting");
        assert_eq!(plugin.avg_window_samples(20_000), 16);
        // One full period of a 2 kHz modulation at 20 kSa/s = 10 samples.
        plugin
            .set_setting("avg_sync_freq_hz", json!(2_000.0))
            .expect("valid setting");
        assert_eq!(plugin.avg_window_samples(20_000), 10);
        // Faster than the sample rate clamps to a single sample.
        plugin
            .set_setting("avg_sync_freq_hz", json!(50_000.0))
            .expect("valid setting");
        assert_eq!(plugin.avg_window_samples(20_000), 1);
    }

    #[test]
    fn current_average_uses_the_newest_window() {
        let plugin = StageAPhotodiodePlugin::default(); // window = 4 samples
        let mut state = SharedState::default();
        state.ingest(0, 20_000, 0, &[0, 0, 0, 0, 100, 200, 300, 400]);
        let average = plugin.current_average_code(&state).expect("has samples");
        assert!((average - 250.0).abs() < 1e-9);
    }

    #[test]
    fn series_dataset_decimates_with_envelope_and_average() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("window_s", json!(120.0)).unwrap();
        plugin.set_setting("avg_samples", json!(50)).unwrap();
        {
            let mut state = plugin.shared.lock().unwrap();
            let codes: Vec<u16> = (0..40_000_u32).map(|i| (i % 4_000) as u16).collect();
            state.ingest(0, 20_000, 0, &codes);
        }
        let series = plugin.series_dataset();
        let names: Vec<&str> = series.lines.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["photodiode", "min", "max", "avg (50 spl)"]);
        for line in &series.lines {
            assert!(
                line.points.len() <= MAX_PLOT_BUCKETS + 1,
                "{} has {} points",
                line.name,
                line.points.len()
            );
            assert!(!line.points.is_empty());
        }
        // min ≤ mean ≤ max, and x is "seconds before now" ending at 0.
        let (mean, min, max) = (&series.lines[0], &series.lines[1], &series.lines[2]);
        for ((m, lo), hi) in mean.points.iter().zip(&min.points).zip(&max.points) {
            assert!(lo.y <= m.y + 1e-9 && m.y <= hi.y + 1e-9);
        }
        let last_x = mean.points.last().unwrap().x;
        assert!(last_x.abs() < 1e-9, "trace ends at now, got {last_x}");
    }

    #[test]
    fn short_windows_render_raw_samples_without_envelope() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("window_s", json!(0.01)).unwrap(); // 200 samples at 20 kSa/s
        plugin.set_setting("avg_samples", json!(1)).unwrap(); // average off
        {
            let mut state = plugin.shared.lock().unwrap();
            let codes: Vec<u16> = (0..1_000_u32).map(|i| (i % 4_000) as u16).collect();
            state.ingest(0, 20_000, 0, &codes);
        }
        let series = plugin.series_dataset();
        let names: Vec<&str> = series.lines.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["photodiode"], "no envelope, no average");
        assert_eq!(series.lines[0].points.len(), 200);
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
        let mut plugin = StageAPhotodiodePlugin {
            port_hint: "mock".into(),
            ..Default::default()
        };
        plugin.connect();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let count = plugin.shared.lock().unwrap().samples.len();
            if count >= MOCK_BLOCK_SAMPLES {
                break;
            }
            assert!(Instant::now() < deadline, "mock reader produced no data");
            std::thread::sleep(Duration::from_millis(5));
        }
        let series = plugin.series_dataset();
        assert!(!series.lines[0].points.is_empty());
        assert_eq!(plugin.shared.lock().unwrap().rate_hz, MOCK_RATE_HZ);
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
}
