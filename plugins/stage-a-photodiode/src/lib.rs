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
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use augur_plugin_api::PathDialogKind;
use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostContext, HostDatasetDescriptor, HostDatasetKind,
    HostOutput, HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry, Plugin,
    PluginFrame, Series1dLine, Series1dPoint, Series1dV1, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1,
    TableSchema, TableValueType,
};
use serde_json::{json, Value};
use stage_a_io::{FrameParser, ParseEvent, PdqWriter, StreamIntegrity};

const SERIES_DATASET_ID: &str = "stage-a-photodiode.series";
const SPECTRUM_DATASET_ID: &str = "stage-a-photodiode.spectrum";
const SPECTRUM_VIEW_ID: &str = "stage-a-photodiode.spectrum.view";
const SERIES_VIEW_ID: &str = "stage-a-photodiode.series.view";
const STATUS_DATASET_ID: &str = "stage-a-photodiode.status";
const STATUS_VIEW_ID: &str = "stage-a-photodiode.status.view";

const ADC_FULL_SCALE_VOLTS: f64 = 3.3;
const ADC_MAX_CODE: f64 = 4_095.0;
/// Default monitor cache, in seconds of samples at the active stream rate
/// (user-settable 1–130 s).
const DEFAULT_CACHE_SECONDS: f64 = 20.0;
const MAX_CACHE_SECONDS: f64 = 130.0;
/// Absolute sample cap: 16 M samples = 32 s at the firmware's 500 kSa/s
/// stream rate (32 MiB of codes + ~2 MiB of summary cells).
const RING_MAX_SAMPLES: usize = 16_000_000;
/// Raw samples per incremental summary cell (min/max/sum), the unit both
/// chart decimation and the moving average combine instead of raw rescans.
const SUMMARY_CELL: usize = 64;
/// Envelope buckets per rendered chart line; keeps the plot payload bounded
/// no matter how many raw samples the window covers.
const MAX_PLOT_BUCKETS: usize = 1_000;
/// Spectrum FFT window bounds: 16384 samples ≈ 0.8 s at 20 kSa/s
/// (Δf ≈ 1.2 Hz); below 256 samples a spectrum is not meaningful.
const SPECTRUM_MIN_SAMPLES: usize = 256;
const SPECTRUM_MAX_SAMPLES: usize = 16_384;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeAxis {
    /// Scrolling view: x = seconds before the newest sample (ends at 0).
    BeforeNow,
    /// Fixed view: x = seconds since the segment start on the device clock —
    /// a frozen plot reads as absolute positions, not implied motion.
    Segment,
}

impl TimeAxis {
    const VARIANTS: [TimeAxis; 2] = [TimeAxis::BeforeNow, TimeAxis::Segment];

    fn name(self) -> &'static str {
        match self {
            Self::BeforeNow => "BEFORE NOW",
            Self::Segment => "SEGMENT TIME",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::VARIANTS.into_iter().find(|axis| axis.name() == name)
    }

    fn label(self) -> &'static str {
        match self {
            Self::BeforeNow => "time before now [s]",
            Self::Segment => "segment time [s]",
        }
    }
}

struct SharedState {
    /// Sample rate of the current segment (from the frame headers).
    rate_hz: u32,
    /// Device sample index of `samples.front()` within the current segment.
    ring_first_index: u64,
    samples: VecDeque<u16>,
    /// Incremental 64:1 summaries: `cells[i]` covers deque offsets
    /// `[i·CELL, (i+1)·CELL)`. Kept aligned by evicting whole cells, so the
    /// chart and moving average never rescan the raw window — at 500 kSa/s a
    /// full-window rescan per repaint would not be viable.
    cells: VecDeque<SummaryCell>,
    latest: Option<u16>,
    /// Cumulative firmware-side drop counter (latest header value).
    device_dropped: u32,
    crc_failures: u64,
    resync_bytes: u64,
    /// Segment restarts observed (rate changes, index jumps, reconnects).
    segments: u64,
    /// Monitor-cache length driving ring eviction (user setting).
    cache_seconds: f64,
    error: Option<String>,
}

/// min/max/sum over exactly [`SUMMARY_CELL`] consecutive raw samples.
#[derive(Clone, Copy)]
struct SummaryCell {
    min: u16,
    max: u16,
    sum: u32,
}

/// Accumulated min/max/sum/count over an arbitrary sample range.
#[derive(Clone, Copy)]
struct RangeSummary {
    min: u16,
    max: u16,
    sum: u64,
    count: usize,
}

impl RangeSummary {
    fn mean(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.sum as f64 / self.count as f64
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self {
            rate_hz: 0,
            ring_first_index: 0,
            samples: VecDeque::new(),
            cells: VecDeque::new(),
            latest: None,
            device_dropped: 0,
            crc_failures: 0,
            resync_bytes: 0,
            segments: 0,
            cache_seconds: DEFAULT_CACHE_SECONDS,
            error: None,
        }
    }
}

impl SharedState {
    fn ring_capacity(&self, rate_hz: u32) -> usize {
        ((f64::from(rate_hz.max(1)) * self.cache_seconds) as usize).clamp(2, RING_MAX_SAMPLES)
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
            self.cells.clear();
            self.ring_first_index = first_index;
            self.rate_hz = rate_hz;
        }
        self.samples.extend(codes.iter().copied());
        self.latest = codes.last().copied();
        self.device_dropped = device_dropped;

        // Summarize every newly completed cell.
        while (self.cells.len() + 1) * SUMMARY_CELL <= self.samples.len() {
            let start = self.cells.len() * SUMMARY_CELL;
            let mut cell = SummaryCell {
                min: u16::MAX,
                max: u16::MIN,
                sum: 0,
            };
            for &code in self.samples.range(start..start + SUMMARY_CELL) {
                cell.min = cell.min.min(code);
                cell.max = cell.max.max(code);
                cell.sum += u32::from(code);
            }
            self.cells.push_back(cell);
        }

        // Evict whole cells only, keeping the cell/offset alignment intact;
        // the ring may exceed its capacity by up to one cell.
        let excess = self
            .samples
            .len()
            .saturating_sub(self.ring_capacity(rate_hz));
        let evict_cells = excess / SUMMARY_CELL;
        if evict_cells > 0 {
            let evict = evict_cells * SUMMARY_CELL;
            self.samples.drain(..evict);
            self.cells.drain(..evict_cells);
            self.ring_first_index += evict as u64;
        }
    }

    /// min/max/sum over deque offsets `[start, end)`, combining whole
    /// summary cells with raw samples at the edges: O(range/64 + 128)
    /// instead of O(range).
    fn range_summary(&self, start: usize, end: usize) -> RangeSummary {
        let end = end.min(self.samples.len());
        let mut summary = RangeSummary {
            min: u16::MAX,
            max: u16::MIN,
            sum: 0,
            count: 0,
        };
        if start >= end {
            return summary;
        }
        summary.count = end - start;
        let covered = self.cells.len() * SUMMARY_CELL;
        let mut i = start;

        // Raw head up to the next cell boundary.
        let head_end = (i.div_ceil(SUMMARY_CELL) * SUMMARY_CELL)
            .min(end)
            .min(covered.max(i));
        if head_end > i {
            for &code in self.samples.range(i..head_end) {
                summary.min = summary.min.min(code);
                summary.max = summary.max.max(code);
                summary.sum += u64::from(code);
            }
            i = head_end;
        }
        // Whole cells.
        while i + SUMMARY_CELL <= end.min(covered) {
            let cell = self.cells[i / SUMMARY_CELL];
            summary.min = summary.min.min(cell.min);
            summary.max = summary.max.max(cell.max);
            summary.sum += u64::from(cell.sum);
            i += SUMMARY_CELL;
        }
        // Raw tail (past the last whole cell in range, or past `covered`).
        for &code in self.samples.range(i..end) {
            summary.min = summary.min.min(code);
            summary.max = summary.max.max(code);
            summary.sum += u64::from(code);
        }
        summary
    }
}

/// One active disk recording: every clean `SamplesU16` frame is appended
/// verbatim to a `.pdq` file; `stop` writes the JSON sidecar next to it.
struct RecordingSink {
    writer: PdqWriter,
    pdq_path: PathBuf,
    started_slug: String,
    samples_written: u64,
    write_error: Option<String>,
    /// Integrity counters at recording start, so the sidecar reports deltas
    /// for exactly the recorded span.
    start_crc_failures: u64,
    start_resync_bytes: u64,
    start_device_dropped: u32,
    start_segments: u64,
}

type SharedRecording = Arc<Mutex<Option<RecordingSink>>>;

fn record_frame(recording: &SharedRecording, frame: &stage_a_io::Frame, samples: usize) {
    let Ok(mut slot) = recording.lock() else {
        return;
    };
    let Some(sink) = slot.as_mut() else {
        return;
    };
    if sink.write_error.is_some() {
        return;
    }
    match sink.writer.write_frame(frame) {
        Ok(()) => sink.samples_written += samples as u64,
        Err(err) => sink.write_error = Some(format!("recording write failed: {err}")),
    }
}

/// `YYYYmmdd_HHMMSS` in UTC without a date-time dependency (Howard Hinnant's
/// civil-from-days algorithm).
fn timestamp_slug() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (seconds / 86_400) as i64;
    let (secs_of_day, z) = ((seconds % 86_400) as u32, days + 719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}{month:02}{day:02}_{:02}{:02}{:02}",
        secs_of_day / 3_600,
        (secs_of_day / 60) % 60,
        secs_of_day % 60
    )
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
        recording: SharedRecording,
    ) -> Result<Self, String> {
        let port = serialport::new(&path, 115_200)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|err| format!("open {path}: {err}"))?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("stage-a-photodiode".into())
            .spawn(move || read_frames(port, &shared, &generation, &recording, &thread_stop))
            .expect("spawning the photodiode reader thread must succeed");
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    /// Hardware-free source: synthesizes a noisy 5 Hz sine around 1 V in
    /// firmware-sized blocks at the firmware's default stream rate.
    fn spawn_mock(
        shared: Arc<Mutex<SharedState>>,
        generation: Arc<AtomicU64>,
        recording: SharedRecording,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = std::thread::Builder::new()
            .name("stage-a-photodiode-mock".into())
            .spawn(move || {
                let start = Instant::now();
                let mut next_index: u64 = 0;
                let mut sequence: u32 = 0;
                while !thread_stop.load(Ordering::Relaxed) {
                    let target = (start.elapsed().as_secs_f64() * f64::from(MOCK_RATE_HZ)) as u64;
                    let mut produced = false;
                    while next_index + MOCK_BLOCK_SAMPLES as u64 <= target {
                        let codes: Vec<u16> = (0..MOCK_BLOCK_SAMPLES)
                            .map(|i| mock_code(next_index + i as u64))
                            .collect();
                        // Recordings capture real wire frames; synthesize the
                        // identical framing so mock recordings parse the same.
                        record_frame(
                            &recording,
                            &mock_sample_frame(sequence, next_index, &codes),
                            codes.len(),
                        );
                        sequence = sequence.wrapping_add(1);
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

fn mock_sample_frame(sequence: u32, first_index: u64, codes: &[u16]) -> stage_a_io::Frame {
    let payload: Vec<u8> = codes.iter().flat_map(|c| c.to_le_bytes()).collect();
    stage_a_io::Frame::build(
        stage_a_io::FrameHeader {
            version: stage_a_io::wire::PROTOCOL_VERSION,
            frame_type: stage_a_io::FrameType::SamplesU16,
            flags: 0,
            sequence,
            payload_bytes: 0,
            first_sample_index: first_index,
            sample_rate_hz: MOCK_RATE_HZ,
            dropped_samples: 0,
            crc32: 0,
        },
        payload,
    )
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
    recording: &SharedRecording,
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
                    record_frame(recording, &frame, codes.len());
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
    recording: SharedRecording,
    last_error: Option<String>,
    /// One-line feedback about the most recent save/recording action.
    last_save_note: Option<String>,
    // -- settings --
    connect_requested: bool,
    port_hint: String,
    mode: Mode,
    reference_volts: f64,
    window_s: f64,
    avg_samples: usize,
    avg_sync_freq_hz: f64,
    time_axis: TimeAxis,
    data_dir: String,
}

impl Default for StageAPhotodiodePlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            reader: None,
            shared: Arc::new(Mutex::new(SharedState::default())),
            generation: Arc::new(AtomicU64::new(1)),
            recording: Arc::new(Mutex::new(None)),
            last_error: None,
            last_save_note: None,
            connect_requested: false,
            port_hint: "auto".into(),
            mode: Mode::Raw,
            reference_volts: 3.3,
            window_s: 10.0,
            avg_samples: 4,
            avg_sync_freq_hz: 0.0,
            time_axis: TimeAxis::BeforeNow,
            data_dir: String::new(),
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
                Arc::clone(&self.recording),
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
        match Reader::spawn_serial(
            path,
            Arc::clone(&self.shared),
            Arc::clone(&self.generation),
            Arc::clone(&self.recording),
        ) {
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

    fn recording_active(&self) -> bool {
        self.recording
            .lock()
            .map(|slot| slot.is_some())
            .unwrap_or(false)
    }

    fn resolved_data_dir(&self) -> Result<PathBuf, String> {
        if self.data_dir.trim().is_empty() {
            return Err("set the data directory first (Data section)".into());
        }
        Ok(PathBuf::from(self.data_dir.trim()))
    }

    fn start_recording(&mut self) -> Result<(), String> {
        if self.recording_active() {
            return Ok(());
        }
        let dir = self.resolved_data_dir()?;
        let slug = timestamp_slug();
        let pdq_path = dir.join(format!("pd_rec_{slug}.pdq"));
        let writer = PdqWriter::create(&pdq_path)
            .map_err(|err| format!("creating {} failed: {err}", pdq_path.display()))?;
        let (crc, resync, dropped, segments) = match self.shared.lock() {
            Ok(state) => (
                state.crc_failures,
                state.resync_bytes,
                state.device_dropped,
                state.segments,
            ),
            Err(_) => (0, 0, 0, 0),
        };
        let sink = RecordingSink {
            writer,
            pdq_path: pdq_path.clone(),
            started_slug: slug,
            samples_written: 0,
            write_error: None,
            start_crc_failures: crc,
            start_resync_bytes: resync,
            start_device_dropped: dropped,
            start_segments: segments,
        };
        if let Ok(mut slot) = self.recording.lock() {
            *slot = Some(sink);
        }
        self.last_save_note = Some(format!("recording → {}", pdq_path.display()));
        Ok(())
    }

    fn stop_recording(&mut self) -> Result<(), String> {
        let Some(sink) = self.recording.lock().ok().and_then(|mut slot| slot.take()) else {
            return Ok(());
        };
        let (rate_hz, crc, resync, dropped, segments) = match self.shared.lock() {
            Ok(state) => (
                state.rate_hz,
                state.crc_failures,
                state.resync_bytes,
                state.device_dropped,
                state.segments,
            ),
            Err(_) => (0, 0, 0, 0, 0),
        };
        let integrity = StreamIntegrity {
            skipped_bytes: resync.saturating_sub(sink.start_resync_bytes),
            crc_failures: crc.saturating_sub(sink.start_crc_failures),
            sequence_gaps: segments.saturating_sub(sink.start_segments),
            dropped_samples: u64::from(dropped.saturating_sub(sink.start_device_dropped)),
        };
        let write_error = sink.write_error.clone();
        let started = sink.started_slug.clone();
        let samples = sink.samples_written;
        let summary = sink
            .writer
            .finish(integrity)
            .map_err(|err| format!("finishing recording failed: {err}"))?;
        let sidecar = json!({
            "kind": "recording",
            "started_utc": started,
            "stopped_utc": timestamp_slug(),
            "port": self.port_hint,
            "sample_rate_hz": rate_hz,
            "samples_written": samples,
            "pdq_path": summary.path,
            "pdq_frames": summary.frames_written,
            "pdq_bytes": summary.bytes_written,
            "pdq_crc32": summary.file_crc32,
            "adc": { "bits": 12, "full_scale_volts": ADC_FULL_SCALE_VOLTS },
            "display_mode": self.mode.name(),
            "reference_volts": self.reference_volts,
            "integrity": {
                "resync_bytes": summary.integrity.skipped_bytes,
                "crc_failures": summary.integrity.crc_failures,
                "segment_restarts": summary.integrity.sequence_gaps,
                "device_dropped_samples": summary.integrity.dropped_samples,
            },
            "valid": summary.valid && write_error.is_none(),
            "write_error": write_error,
        });
        let sidecar_path = sink.pdq_path.with_extension("json");
        write_json(&sidecar_path, &sidecar)?;
        self.last_save_note = Some(format!(
            "saved recording {} ({} samples)",
            sink.pdq_path.display(),
            samples
        ));
        Ok(())
    }

    /// Dumps the current monitor cache (ring) as CSV + JSON sidecar. Raw
    /// codes and raw volts only — mode/reference land in the sidecar so
    /// EXCITATION values stay derivable without baking display state into
    /// the data.
    fn save_cache_snapshot(&mut self) -> Result<(), String> {
        let dir = self.resolved_data_dir()?;
        let slug = timestamp_slug();
        let csv_path = dir.join(format!("pd_cache_{slug}.csv"));
        let state = self
            .shared
            .lock()
            .map_err(|_| "photodiode state lock poisoned".to_owned())?;
        if state.samples.is_empty() || state.rate_hz == 0 {
            return Err("no samples cached yet".into());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("creating {} failed: {err}", dir.display()))?;
        let file = File::create(&csv_path)
            .map_err(|err| format!("creating {} failed: {err}", csv_path.display()))?;
        let mut writer = BufWriter::new(file);
        let rate = f64::from(state.rate_hz);
        writeln!(writer, "sample_index,t_s,code,volts")
            .map_err(|err| format!("writing CSV failed: {err}"))?;
        for (offset, &code) in state.samples.iter().enumerate() {
            let index = state.ring_first_index + offset as u64;
            writeln!(
                writer,
                "{index},{:.9},{code},{:.6}",
                index as f64 / rate,
                code_to_volts(f64::from(code))
            )
            .map_err(|err| format!("writing CSV failed: {err}"))?;
        }
        writer
            .flush()
            .map_err(|err| format!("writing CSV failed: {err}"))?;

        let sidecar = json!({
            "kind": "cache_snapshot",
            "created_utc": slug,
            "port": self.port_hint,
            "sample_rate_hz": state.rate_hz,
            "samples": state.samples.len(),
            "first_sample_index": state.ring_first_index,
            "cache_seconds": state.cache_seconds,
            "csv_path": csv_path,
            "adc": { "bits": 12, "full_scale_volts": ADC_FULL_SCALE_VOLTS },
            "display_mode": self.mode.name(),
            "reference_volts": self.reference_volts,
            "time_base": "t_s = sample_index / sample_rate_hz, device clock, segment-relative",
            "integrity": {
                "resync_bytes": state.resync_bytes,
                "crc_failures": state.crc_failures,
                "segment_restarts": state.segments,
                "device_dropped_samples": state.device_dropped,
            },
        });
        let sample_count = state.samples.len();
        drop(state);
        write_json(&csv_path.with_extension("json"), &sidecar)?;
        self.last_save_note = Some(format!(
            "saved cache {} ({sample_count} samples)",
            csv_path.display()
        ));
        Ok(())
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
        Some(state.range_summary(start, state.samples.len()).mean())
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
        let x_label = self.time_axis.label();
        let empty = |label: &str| Series1dV1 {
            x_label: x_label.into(),
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

        let mut mean_points = Vec::with_capacity(MAX_PLOT_BUCKETS + 1);
        let mut min_points = Vec::with_capacity(if decimating { MAX_PLOT_BUCKETS + 1 } else { 0 });
        let mut max_points = Vec::with_capacity(if decimating { MAX_PLOT_BUCKETS + 1 } else { 0 });
        let mut avg_points = Vec::with_capacity(if avg_enabled { MAX_PLOT_BUCKETS + 1 } else { 0 });

        // Every bucket (and every moving-average window) is combined from
        // the incremental summary cells plus raw edge samples — the cost per
        // rebuild is O(buckets · window/64), independent of the raw rate.
        let mut bucket_start = start;
        while bucket_start < total {
            let bucket_end = (bucket_start + bucket_len).min(total);
            let last = bucket_end - 1;
            let bucket = state.range_summary(bucket_start, bucket_end);
            let device_t = (state.ring_first_index + last as u64) as f64 / rate;
            let x = match self.time_axis {
                TimeAxis::BeforeNow => device_t - latest_x_index as f64 / rate,
                TimeAxis::Segment => device_t,
            };
            mean_points.push(Series1dPoint {
                x,
                y: self.display_volts(bucket.mean()),
            });
            if decimating {
                // EXCITATION inverts the axis, so min/max swap roles.
                let (low, high) = (
                    self.display_volts(f64::from(bucket.min)),
                    self.display_volts(f64::from(bucket.max)),
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
                // Trailing window ending at this bucket's last sample; may
                // reach before the visible slice (fewer while filling).
                let window_start = (last + 1).saturating_sub(avg_window);
                let window = state.range_summary(window_start, last + 1);
                avg_points.push(Series1dPoint {
                    x,
                    y: self.display_volts(window.mean()),
                });
            }
            bucket_start = bucket_end;
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
            x_label: x_label.into(),
            y_label: y_label.into(),
            lines,
        }
    }

    /// Amplitude spectrum of the newest power-of-two window of raw samples
    /// (Hann-windowed radix-2 FFT). Only computed while the spectrum window
    /// is open — it has Window placement, and the host fetches datasets of
    /// closed windows never.
    fn spectrum_dataset(&self) -> Series1dV1 {
        let empty = Series1dV1 {
            x_label: "frequency [Hz]".into(),
            y_label: "amplitude [V]".into(),
            lines: vec![Series1dLine {
                name: "spectrum".into(),
                points: Vec::new(),
            }],
        };
        let Ok(state) = self.shared.lock() else {
            return empty;
        };
        let total = state.samples.len();
        if total < SPECTRUM_MIN_SAMPLES || state.rate_hz == 0 {
            return empty;
        }
        let available = total.min(SPECTRUM_MAX_SAMPLES);
        let n = if available.is_power_of_two() {
            available
        } else {
            available.next_power_of_two() >> 1
        };
        let start = total - n;
        let mut real: Vec<f64> = state
            .samples
            .range(start..)
            .map(|&code| code_to_volts(f64::from(code)))
            .collect();
        let rate = f64::from(state.rate_hz);
        drop(state);

        let mean = real.iter().sum::<f64>() / n as f64;
        // Hann window (coherent gain 0.5) on the demeaned signal.
        for (i, value) in real.iter_mut().enumerate() {
            let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / (n as f64 - 1.0)).cos());
            *value = (*value - mean) * w;
        }
        let mut imag = vec![0.0_f64; n];
        fft_radix2(&mut real, &mut imag);

        // One-sided amplitude: 2·|X|/(N·0.5); decimate bins by max-hold so
        // narrow peaks survive the plot budget.
        let bins = n / 2;
        let bucket = bins.div_ceil(MAX_PLOT_BUCKETS).max(1);
        let mut points = Vec::with_capacity(bins.div_ceil(bucket));
        let mut peak = 0.0_f64;
        let mut peak_freq = 0.0_f64;
        let mut in_bucket = 0_usize;
        for k in 1..bins {
            let amplitude = 2.0 * (real[k] * real[k] + imag[k] * imag[k]).sqrt() / (n as f64 * 0.5);
            let freq = k as f64 * rate / n as f64;
            if amplitude > peak {
                peak = amplitude;
                peak_freq = freq;
            }
            in_bucket += 1;
            if in_bucket == bucket || k == bins - 1 {
                points.push(Series1dPoint {
                    x: peak_freq,
                    y: peak,
                });
                peak = 0.0;
                peak_freq = freq;
                in_bucket = 0;
            }
        }
        Series1dV1 {
            x_label: "frequency [Hz]".into(),
            y_label: "amplitude [V]".into(),
            lines: vec![Series1dLine {
                name: format!("spectrum ({n} spl, Δf {:.2} Hz)", rate / n as f64),
                points,
            }],
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

/// In-place iterative radix-2 Cooley–Tukey FFT. Lengths must be powers of
/// two; sized for the spectrum window (≤ 16384), where it runs in well under
/// a millisecond.
fn fft_radix2(real: &mut [f64], imag: &mut [f64]) {
    let n = real.len();
    debug_assert!(n.is_power_of_two() && imag.len() == n);
    // Bit-reversal permutation.
    let mut j = 0_usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            real.swap(i, j);
            imag.swap(i, j);
        }
    }
    let mut len = 2_usize;
    while len <= n {
        let angle = -2.0 * std::f64::consts::PI / len as f64;
        let (step_r, step_i) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut w_r, mut w_i) = (1.0_f64, 0.0_f64);
            for k in start..start + len / 2 {
                let (even_r, even_i) = (real[k], imag[k]);
                let (odd_r, odd_i) = (
                    real[k + len / 2] * w_r - imag[k + len / 2] * w_i,
                    real[k + len / 2] * w_i + imag[k + len / 2] * w_r,
                );
                real[k] = even_r + odd_r;
                imag[k] = even_i + odd_i;
                real[k + len / 2] = even_r - odd_r;
                imag[k + len / 2] = even_i - odd_i;
                let next_r = w_r * step_r - w_i * step_i;
                w_i = w_r * step_i + w_i * step_r;
                w_r = next_r;
            }
        }
        len <<= 1;
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| format!("serializing sidecar failed: {err}"))?;
    std::fs::write(path, bytes).map_err(|err| format!("writing {} failed: {err}", path.display()))
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
    let mut saw_legacy_ascii = false;
    for path in &candidates {
        match probe_pd_stream(path) {
            ProbeResult::Pda1SampleFrames => return Ok(path.clone()),
            ProbeResult::LegacyAsciiStream => saw_legacy_ascii = true,
            ProbeResult::Nothing => {}
        }
    }
    if saw_legacy_ascii {
        // The pre-0.4.0 firmware emits `PD code=… n=… t_ms=…` ASCII lines
        // instead of PDA1 binary frames. This plugin dropped the ASCII path
        // (three-repo lockstep), so the fix is a firmware flash, not a plugin
        // setting — say so instead of a generic "no frames".
        return Err(format!(
            "found the legacy ASCII photodiode stream (pre-0.4.0 firmware) — flash \
             stage-a-controller 0.4.0+ so the stream port emits PDA1 binary frames \
             (tried {})",
            candidates.join(", ")
        ));
    }
    Err(format!(
        "no port streamed PDA1 sample frames within 500 ms (tried {})",
        candidates.join(", ")
    ))
}

/// What a brief listen on a candidate port revealed.
enum ProbeResult {
    /// CRC-clean PDA1 `SamplesU16` frames — the 0.4.0+ stream port.
    Pda1SampleFrames,
    /// `PD code=… n=… t_ms=…` ASCII lines — the pre-0.4.0 stream port.
    LegacyAsciiStream,
    /// Nothing parsable (busy/command port, wrong device, or no data).
    Nothing,
}

/// Listens on `path` for up to 500 ms and classifies what it emits. The
/// command port emits frames too, but only control replies and acquisition
/// data — unsolicited sample frames identify the stream port.
fn probe_pd_stream(path: &str) -> ProbeResult {
    let Ok(mut port) = serialport::new(path, 115_200)
        .timeout(Duration::from_millis(100))
        .open()
    else {
        return ProbeResult::Nothing;
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut parser = FrameParser::default();
    let mut ascii_tail: Vec<u8> = Vec::with_capacity(256);
    let mut buf = [0_u8; 4_096];
    while Instant::now() < deadline {
        match port.read(&mut buf) {
            Ok(read) if read > 0 => {
                parser.extend(&buf[..read]);
                while let Some(event) = parser.next_event() {
                    if let ParseEvent::Frame(frame) = event {
                        if frame.samples().is_some() {
                            return ProbeResult::Pda1SampleFrames;
                        }
                    }
                }
                // Sniff for the legacy ASCII line format in parallel; a valid
                // `PD code=` prefix never appears inside PDA1 binary framing.
                ascii_tail.extend_from_slice(&buf[..read]);
                if String::from_utf8_lossy(&ascii_tail).contains("PD code=") {
                    return ProbeResult::LegacyAsciiStream;
                }
                if ascii_tail.len() > 512 {
                    ascii_tail.drain(..ascii_tail.len() - 256);
                }
            }
            Ok(_) => {}
            Err(err)
                if err.kind() == std::io::ErrorKind::TimedOut
                    || err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return ProbeResult::Nothing,
        }
    }
    ProbeResult::Nothing
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
            // Finalize an active recording so the .pdq/.json pair is complete
            // even when the plugin is disabled mid-run.
            if let Err(err) = self.stop_recording() {
                self.last_error = Some(err);
            }
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
            sections: vec![
                SettingsSection {
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
                                "Opens/closes the stream port (read-only, no camera required)."
                                    .into(),
                            ),
                            kind: SettingKind::Bool {
                                default: self.connect_requested,
                            },
                        },
                        SettingItem {
                            key: "mode".into(),
                            label: "Mode".into(),
                            tooltip: Some(
                                "RAW: ADC code and volts as measured. EXCITATION: I_tot − I_pd"
                                    .into(),
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
                        SettingItem {
                            key: "time_axis".into(),
                            label: "Time axis".into(),
                            tooltip: Some(
                                "BEFORE NOW scrolls (x ends at 0); SEGMENT TIME shows absolute \
                                 seconds on the device clock — better for frozen plots and \
                                 cursor measurements."
                                    .into(),
                            ),
                            kind: SettingKind::Enum {
                                variants: TimeAxis::VARIANTS
                                    .iter()
                                    .map(|axis| axis.name().to_owned())
                                    .collect(),
                                default: TimeAxis::VARIANTS
                                    .iter()
                                    .position(|axis| *axis == self.time_axis)
                                    .unwrap_or(0),
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Data".into(),
                    description: Some(
                        "Monitor cache and disk recording. The cache always holds the last \
                     N seconds; recording tees every incoming frame to a .pdq file \
                     (+ JSON sidecar) so length is disk-bound. CSV/PDQ store raw codes \
                     and raw volts on the device clock; mode and reference go into the \
                     sidecar."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "data_dir".into(),
                            label: "Data directory".into(),
                            tooltip: Some(
                                "Where recordings and cache snapshots are written.".into(),
                            ),
                            kind: SettingKind::Path {
                                dialog: PathDialogKind::Directory,
                                default: self.data_dir.clone(),
                            },
                        },
                        SettingItem {
                            key: "cache_s".into(),
                            label: "Cache length".into(),
                            tooltip: Some(
                                "Seconds of raw samples kept in memory for the chart and \
                             cache snapshots."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 1.0,
                                max: MAX_CACHE_SECONDS,
                                speed: 1.0,
                                default: self
                                    .shared
                                    .lock()
                                    .map(|state| state.cache_seconds)
                                    .unwrap_or(DEFAULT_CACHE_SECONDS),
                            },
                        },
                        SettingItem {
                            key: "record".into(),
                            label: "Record to disk".into(),
                            tooltip: Some(
                                "Start/stop appending every incoming sample frame to \
                             pd_rec_<timestamp>.pdq; stopping writes the JSON sidecar."
                                    .into(),
                            ),
                            kind: SettingKind::Bool {
                                default: self.recording_active(),
                            },
                        },
                        SettingItem {
                            key: "save_snapshot".into(),
                            label: "Save cache snapshot".into(),
                            tooltip: Some(
                                "Write the current cache as pd_cache_<timestamp>.csv \
                             (+ JSON sidecar)."
                                    .into(),
                            ),
                            kind: SettingKind::Button,
                        },
                    ],
                },
            ],
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
            "time_axis" => {
                let index = TimeAxis::VARIANTS
                    .iter()
                    .position(|axis| *axis == self.time_axis)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "data_dir" => Some(json!(self.data_dir)),
            "cache_s" => Some(json!(self
                .shared
                .lock()
                .map(|state| state.cache_seconds)
                .unwrap_or(DEFAULT_CACHE_SECONDS))),
            "record" => Some(json!(self.recording_active())),
            // Momentary trigger: never reports as pressed.
            "save_snapshot" => Some(json!(false)),
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
            "time_axis" => {
                let names: Vec<String> = TimeAxis::VARIANTS
                    .iter()
                    .map(|axis| axis.name().to_owned())
                    .collect();
                let name = enum_choice(&value, &names)?;
                self.time_axis = TimeAxis::from_name(&name)
                    .ok_or_else(|| format!("unknown time axis: {name}"))?;
                Ok(())
            }
            "data_dir" => {
                self.data_dir = value
                    .as_str()
                    .ok_or("data_dir must be a string")?
                    .to_owned();
                Ok(())
            }
            "cache_s" => {
                let seconds = value.as_f64().ok_or("cache_s must be a number")?;
                if let Ok(mut state) = self.shared.lock() {
                    state.cache_seconds = seconds.clamp(1.0, MAX_CACHE_SECONDS);
                }
                Ok(())
            }
            "record" => {
                let requested = value.as_bool().ok_or("record must be a boolean")?;
                // Failures surface through status entries (like `connect`),
                // so a missing data directory doesn't read as a broken UI.
                let result = if requested {
                    self.start_recording()
                } else {
                    self.stop_recording()
                };
                if let Err(err) = result {
                    self.last_error = Some(err);
                } else {
                    self.last_error = None;
                }
                self.generation.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            "save_snapshot" => {
                match self.save_cache_snapshot() {
                    Ok(()) => self.last_error = None,
                    Err(err) => self.last_error = Some(err),
                }
                self.generation.fetch_add(1, Ordering::Relaxed);
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
        if self.recording_active() {
            let (samples, path) = self
                .recording
                .lock()
                .ok()
                .and_then(|slot| {
                    slot.as_ref()
                        .map(|sink| (sink.samples_written, sink.pdq_path.display().to_string()))
                })
                .unwrap_or((0, String::new()));
            let seconds = if rate_hz > 0 {
                samples as f64 / f64::from(rate_hz)
            } else {
                0.0
            };
            entries.push(StatusEntry::Text(format!("● REC {seconds:.1} s → {path}")));
        } else if let Some(note) = &self.last_save_note {
            entries.push(StatusEntry::Text(note.clone()));
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
                    id: SPECTRUM_DATASET_ID.into(),
                    title: "Photodiode spectrum".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "Not enough samples for a spectrum yet — connect the stream \
                                    port and wait a moment."
                        .into(),
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
                    id: SPECTRUM_VIEW_ID.into(),
                    title: "PD Spectrum".into(),
                    dataset_id: SPECTRUM_DATASET_ID.into(),
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
            SPECTRUM_DATASET_ID => serde_json::to_vec(&self.spectrum_dataset()).ok(),
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            SERIES_DATASET_ID | SPECTRUM_DATASET_ID | STATUS_DATASET_ID => {
                self.generation.load(Ordering::Relaxed).max(1)
            }
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
        let rate = 1_000; // capacity = cache_seconds (20 s default) × rate
        let cap = state.ring_capacity(rate);
        assert_eq!(cap, 20_000, "default cache is 20 s");
        let block: Vec<u16> = (0..1_000).map(|i| (i % 4_096) as u16).collect();
        let mut index = 0_u64;
        for _ in 0..(cap / block.len() + 5) {
            state.ingest(index, rate, 0, &block);
            index += block.len() as u64;
        }
        // Whole-cell eviction may leave up to one summary cell of slack.
        assert!(
            state.samples.len() >= cap && state.samples.len() < cap + SUMMARY_CELL,
            "len {} vs cap {cap}",
            state.samples.len()
        );
        assert_eq!(
            state.ring_first_index + state.samples.len() as u64,
            index,
            "eviction keeps indexes aligned"
        );
        assert_eq!(
            state.ring_first_index % SUMMARY_CELL as u64,
            0,
            "eviction preserves cell alignment"
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

    /// The summary cells must agree exactly with a naive raw scan for
    /// arbitrary ranges, including after whole-cell eviction.
    #[test]
    fn range_summary_matches_naive_scans() {
        let mut state = SharedState {
            cache_seconds: 1.0, // capacity 1000 at rate 1000 → forces eviction
            ..SharedState::default()
        };
        let mut hash: u64 = 0x243F_6A88_85A3_08D3;
        let mut next = || {
            hash ^= hash << 13;
            hash ^= hash >> 7;
            hash ^= hash << 17;
            (hash % 4_096) as u16
        };
        let mut index = 0_u64;
        for _ in 0..7 {
            let block: Vec<u16> = (0..333).map(|_| next()).collect();
            state.ingest(index, 1_000, 0, &block);
            index += block.len() as u64;
        }
        assert!(state.samples.len() <= 1_000 + SUMMARY_CELL, "evicted");
        assert!(!state.cells.is_empty());

        let len = state.samples.len();
        for (start, end) in [
            (0, len),
            (0, 1),
            (1, SUMMARY_CELL),
            (SUMMARY_CELL - 1, SUMMARY_CELL + 1),
            (7, 500),
            (130, 131),
            (len - 3, len),
            (len / 3, 2 * len / 3),
        ] {
            let summary = state.range_summary(start, end);
            let raw: Vec<u16> = state.samples.range(start..end).copied().collect();
            assert_eq!(summary.count, raw.len(), "count for {start}..{end}");
            assert_eq!(
                summary.min,
                raw.iter().copied().min().unwrap(),
                "min for {start}..{end}"
            );
            assert_eq!(
                summary.max,
                raw.iter().copied().max().unwrap(),
                "max for {start}..{end}"
            );
            assert_eq!(
                summary.sum,
                raw.iter().map(|&c| u64::from(c)).sum::<u64>(),
                "sum for {start}..{end}"
            );
        }
    }

    #[test]
    fn spectrum_finds_a_synthesized_tone() {
        let plugin = StageAPhotodiodePlugin::default();
        let rate = 20_000_u32;
        // 1 kHz, 0.4 V amplitude around 1 V — well inside the ADC range.
        let codes: Vec<u16> = (0..16_384_u64)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let volts = 1.0 + 0.4 * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin();
                (volts * ADC_MAX_CODE / ADC_FULL_SCALE_VOLTS) as u16
            })
            .collect();
        plugin.shared.lock().unwrap().ingest(0, rate, 0, &codes);
        let spectrum = plugin.spectrum_dataset();
        let points = &spectrum.lines[0].points;
        assert!(!points.is_empty());
        let peak = points
            .iter()
            .max_by(|a, b| a.y.partial_cmp(&b.y).unwrap())
            .unwrap();
        assert!(
            (peak.x - 1_000.0).abs() < 5.0,
            "peak at {} Hz, expected 1 kHz",
            peak.x
        );
        assert!(
            (peak.y - 0.4).abs() < 0.05,
            "peak amplitude {} V, expected ≈0.4 V",
            peak.y
        );
    }

    #[test]
    fn segment_time_axis_uses_absolute_device_time() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("avg_samples", json!(1)).unwrap();
        plugin
            .set_setting("time_axis", json!("SEGMENT TIME"))
            .unwrap();
        {
            let mut state = plugin.shared.lock().unwrap();
            state.ingest(40_000, 20_000, 0, &[1, 2, 3, 4]);
        }
        let series = plugin.series_dataset();
        assert_eq!(series.x_label, "segment time [s]");
        let first = series.lines[0].points.first().unwrap();
        // Sample index 40_000 at 20 kSa/s = 2 s into the segment.
        assert!((first.x - 2.0).abs() < 1e-6, "got {}", first.x);
        // Default mode still ends at zero.
        plugin
            .set_setting("time_axis", json!("BEFORE NOW"))
            .unwrap();
        let series = plugin.series_dataset();
        assert!(series.lines[0].points.last().unwrap().x.abs() < 1e-9);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stage-a-photodiode-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn cache_snapshot_writes_csv_and_sidecar() {
        let dir = temp_dir("snapshot");
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin
            .set_setting("data_dir", json!(dir.display().to_string()))
            .unwrap();
        {
            let mut state = plugin.shared.lock().unwrap();
            state.ingest(10, 20_000, 0, &[100, 200, 300]);
        }
        plugin.set_setting("save_snapshot", json!(true)).unwrap();
        assert!(plugin.last_error.is_none(), "{:?}", plugin.last_error);

        let mut csv_files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "csv"))
            .collect();
        assert_eq!(csv_files.len(), 1);
        let csv_path = csv_files.pop().unwrap();
        let csv = std::fs::read_to_string(&csv_path).unwrap();
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("sample_index,t_s,code,volts"));
        let first = lines.next().unwrap();
        assert!(first.starts_with("10,0.000500000,100,"), "{first}");
        assert_eq!(csv.lines().count(), 4, "header + 3 samples");

        let sidecar: Value =
            serde_json::from_slice(&std::fs::read(csv_path.with_extension("json")).unwrap())
                .unwrap();
        assert_eq!(sidecar["kind"], "cache_snapshot");
        assert_eq!(sidecar["sample_rate_hz"], 20_000);
        assert_eq!(sidecar["samples"], 3);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_without_data_dir_reports_an_error() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("save_snapshot", json!(true)).unwrap();
        assert!(plugin
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("data directory")));
    }

    #[test]
    fn recording_tees_frames_to_pdq_and_writes_a_sidecar() {
        let dir = temp_dir("recording");
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin
            .set_setting("data_dir", json!(dir.display().to_string()))
            .unwrap();
        plugin.set_setting("record", json!(true)).unwrap();
        assert!(plugin.recording_active());
        assert_eq!(plugin.get_setting("record"), Some(json!(true)));

        // The reader thread path: every parsed frame is teed to the sink.
        let frame = mock_sample_frame(0, 0, &[1, 2, 3, 4]);
        record_frame(&plugin.recording, &frame, 4);
        {
            let mut state = plugin.shared.lock().unwrap();
            state.ingest(0, MOCK_RATE_HZ, 0, &[1, 2, 3, 4]);
        }

        plugin.set_setting("record", json!(false)).unwrap();
        assert!(!plugin.recording_active());
        assert!(plugin.last_error.is_none(), "{:?}", plugin.last_error);

        let pdq_path: std::path::PathBuf = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|ext| ext == "pdq"))
            .expect("pdq written");
        assert_eq!(std::fs::read(&pdq_path).unwrap(), frame.to_bytes());

        let sidecar: Value =
            serde_json::from_slice(&std::fs::read(pdq_path.with_extension("json")).unwrap())
                .unwrap();
        assert_eq!(sidecar["kind"], "recording");
        assert_eq!(sidecar["samples_written"], 4);
        assert_eq!(sidecar["pdq_frames"], 1);
        assert_eq!(sidecar["valid"], true);

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_length_setting_drives_ring_capacity() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("cache_s", json!(2.0)).unwrap();
        assert_eq!(plugin.get_setting("cache_s"), Some(json!(2.0)));
        let mut state = plugin.shared.lock().unwrap();
        assert_eq!(state.ring_capacity(1_000), 2_000);
        let block: Vec<u16> = vec![1; 1_000];
        for i in 0..5_u64 {
            let first = i * 1_000;
            state.ingest(first, 1_000, 0, &block);
        }
        // Whole-cell eviction may leave up to one summary cell of slack.
        assert!(
            state.samples.len() >= 2_000 && state.samples.len() < 2_000 + SUMMARY_CELL,
            "len {}",
            state.samples.len()
        );
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
