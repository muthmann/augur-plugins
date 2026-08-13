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

#[cfg(test)]
mod protocol_validation_tests;

use std::collections::{BTreeMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use augur_plugin_api::PathDialogKind;
use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostContext, HostDatasetDescriptor, HostDatasetKind,
    HostOutput, HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry, Plugin,
    PluginControlContext, PluginControlSnapshot, PluginFrame, PluginRuntimeRole,
    PluginServiceOutcome, PluginServiceReply, PluginServiceRequest, Series1dLine, Series1dPoint,
    Series1dV1, SettingItem, SettingKind, SettingsSchema, SettingsSection, StatusEntry,
    TableColumn, TableColumnData, TableColumnValues, TableDatasetV1, TableSchema, TableValueType,
};
use serde_json::{json, Value};
use stage_a_io::{
    estimate_contrast, near_rail_margin, AdcCalibration, ContrastGeometry, EstimateError,
    FrameParser, ParseEvent, PdqWriter, StreamIntegrity,
};
use stage_a_plugin_contract::{
    ClientId, ConnectionStateV1, FreshnessV1, LeaseId, LeaseSnapshotV1, OwnerInstanceId,
    PdqFinalizedReceiptV1, PdqReceiptV1, PdqStartSpecV1, PdqStartedReceiptV1, PdqTerminationV1,
    PhotodiodeCalibrationV1, PhotodiodeCommandV1, PhotodiodeDarkReferenceV1,
    PhotodiodeDarkSourceV1, PhotodiodeLevelV1, PhotodiodeOpticalSummaryV1, PhotodiodePlacementV1,
    PhotodiodeRequestV1, PhotodiodeResponseV1, PhotodiodeStreamV1, PhotodiodeSummaryV1,
    RequestOutcomeV1, ResponseCommonV1, RunId, SampleRangeV1, SemanticRevision, ServiceErrorCodeV1,
    ServiceErrorV1, Sha256V1, StreamIntegrityV1, SynchronizationV1, UnsyncedReasonV1,
    CONTRACT_VERSION_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1, PLUGIN_ID_STAGE_A_PHOTODIODE,
    SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
};

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
/// Floor on the trailing samples used for the live optical log-contrast `a`,
/// and the fallback window when no phase-0 markers give a period. Sized like
/// the spectrum window: 16 384 samples ≈ 0.82 s at 20 kSa/s.
#[cfg(test)]
const CONTRAST_WINDOW_SAMPLES: usize = 16_384;
/// Whole modulation cycles the contrast window is sized to cover.
///
/// `a` is a *peak-to-peak* quantity, so a window shorter than one cycle sees
/// only an arc of the waveform and under-reports it — and a consumer that
/// divides by the measured `a` (A1's `a₀` lock) then inflates its drive against
/// that bias. A fixed 0.82 s window is below one cycle for every `f < 1.2 Hz`,
/// i.e. exactly the sub-hertz plateau reference the A1 protocol needs. The
/// markers give the period on the same sample clock, so size the window from
/// them instead.
const CONTRAST_WINDOW_CYCLES: f64 = 8.0;
const MOCK_BLOCK_SAMPLES: usize = 256;
/// Cap on retained phase-0 markers (bounds the overlay + frequency window).
const MAX_MARKERS: usize = 4_096;
/// Mock phase-0 marker period in samples (20 kSa/s / 40 = 500 Hz modulation).
const MOCK_MARKER_PERIOD_SAMPLES: u64 = 40;
/// Seconds of samples the **published** level averages over, independent of the
/// chart's averaging setting. One full mains period: a boxcar of exactly this
/// length nulls 50 Hz and every harmonic of it. See
/// [`StageAPhotodiodePlugin::level_window_samples`] and ADR 019.
const LEVEL_WINDOW_SECONDS: f64 = 0.020;
const REQUEST_CACHE_LIMIT: usize = 256;
const MIN_LEASE_TTL_MS: u64 = 1_000;
const MAX_LEASE_TTL_MS: u64 = 60_000;
const SNAPSHOT_VALID_FOR_MS: u64 = 2_000;
static OWNER_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn code_to_volts(code: f64) -> f64 {
    code * ADC_FULL_SCALE_VOLTS / ADC_MAX_CODE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Raw,
    Excitation,
}

const PHOTODIODE_PLACEMENTS: [PhotodiodePlacementV1; 3] = [
    PhotodiodePlacementV1::RejectedPort,
    PhotodiodePlacementV1::CameraPath,
    PhotodiodePlacementV1::EmissionPath,
];

fn placement_name(placement: PhotodiodePlacementV1) -> &'static str {
    match placement {
        PhotodiodePlacementV1::RejectedPort => "PBS rejected port",
        PhotodiodePlacementV1::CameraPath => "camera path (direct)",
        PhotodiodePlacementV1::EmissionPath => "emission path (direct fluorescence)",
    }
}

fn placement_from_name(name: &str) -> Option<PhotodiodePlacementV1> {
    PHOTODIODE_PLACEMENTS
        .into_iter()
        .find(|placement| placement_name(*placement) == name)
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
    /// Phase-0 marker sample indices (device clock) still inside the ring, from
    /// `Marker` stream frames. Used for the opt-in trigger overlay and to derive
    /// the modulation frequency.
    markers: VecDeque<u64>,
    /// A2 optical-comparator crossings (`MarkerPayload::source == 2`). Kept
    /// separate from A1 phase-0 markers so neither experiment can silently
    /// derive a frequency from the other experiment's fiducials.
    comparator_markers: VecDeque<(u64, u8)>,
    /// Newest phase-0 marker index seen, retained or already evicted, and the
    /// spacing to the one before it.
    ///
    /// The retained markers alone cannot measure a period longer than the ring:
    /// once the ring holds less than one cycle it holds at most one marker, so
    /// the mean spacing is undefined exactly where knowing the period matters
    /// most. Markers arrive one at a time, so remember the interval as it goes
    /// past instead of trying to recover it from what survived eviction.
    last_marker_index: Option<u64>,
    marker_period_estimate: Option<f64>,
    latest: Option<u16>,
    /// Cumulative firmware-side drop counter (latest header value).
    device_dropped: u32,
    crc_failures: u64,
    resync_bytes: u64,
    /// Segment restarts observed (rate changes, index jumps, reconnects).
    segments: u64,
    /// Monitor-cache length driving ring eviction (user setting).
    cache_seconds: f64,
    /// Highest smoothed detector level observed since the port was opened, in
    /// raw ADC codes. **This is the total-power anchor `I_tot`.**
    ///
    /// The detector sits on the PBS reject port and reads the complement
    /// `I_pd = I_tot − I_exc`, so it is brightest exactly where the excitation
    /// is fully extinguished — and there `I_pd = I_tot`. Nothing has to be
    /// typed in: the Pockels transfer sweep walks the DAC across the whole
    /// lobe, which lands on the excitation null by construction, so the anchor
    /// is learned by the calibration the operator already runs (ADR 024).
    ///
    /// Latched over completed [`SUMMARY_CELL`]-sample cells, never over raw
    /// samples: one noise spike must not become the anchor every later `a` is
    /// divided against.
    observed_peak_code: Option<f64>,
    error: Option<String>,
    last_update_unix_ms: u64,
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
            markers: VecDeque::new(),
            comparator_markers: VecDeque::new(),
            last_marker_index: None,
            marker_period_estimate: None,
            latest: None,
            device_dropped: 0,
            crc_failures: 0,
            resync_bytes: 0,
            segments: 0,
            cache_seconds: DEFAULT_CACHE_SECONDS,
            observed_peak_code: None,
            error: None,
            last_update_unix_ms: 0,
        }
    }
}

impl SharedState {
    /// Samples the ring retains: the operator's cache length, or the whole
    /// modulation cycles the optical estimator needs at the period the drive is
    /// running — whichever is longer, capped by [`RING_MAX_SAMPLES`].
    ///
    /// `a` is only measurable between phase-0 markers, so at low `f` the
    /// retained window *is* the gate on whether it can be published at all: two
    /// cycles at 0.075 Hz are 26.7 s, which the 20 s default never covers. Left
    /// to a setting, that turns into a precondition an operator has to work out
    /// per file and set by hand before pressing Start — and an A1 survey whose
    /// lowest rung is sub-hertz otherwise records its full duration and only
    /// then discovers it has no `a` to write. The markers give the period on the
    /// same sample clock the ring is indexed by, so size the ring from them and
    /// the precondition disappears.
    ///
    /// Sizing follows the drive both ways: the ring shrinks back on the next
    /// ingest when the frequency goes up, because eviction re-reads the capacity
    /// every frame.
    fn ring_capacity(&self, rate_hz: u32) -> usize {
        let requested = f64::from(rate_hz.max(1)) * self.cache_seconds;
        // One cycle beyond the estimator's window, so a whole window still fits
        // once the oldest marker ages out of it.
        let needed = self
            .contrast_period_samples()
            .map_or(0.0, |period| period * (CONTRAST_WINDOW_CYCLES + 1.0));
        (requested.max(needed) as usize).clamp(2, RING_MAX_SAMPLES)
    }

    /// The modulation period in samples the ring sizes itself against.
    ///
    /// The newest marker interval first: it moves to the new period on the first
    /// marker after a retarget, where the mean over the retained markers still
    /// carries the previous rung and would grow the ring a cycle at a time. It
    /// also survives eviction, so a period longer than the ring itself — the
    /// case this exists for — is still known.
    fn contrast_period_samples(&self) -> Option<f64> {
        self.marker_period_estimate
            .filter(|period| *period > 0.0)
            .or_else(|| self.marker_period_samples())
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
            self.markers.clear();
            self.comparator_markers.clear();
            // The sample clock restarts with the segment, so a spacing
            // measured across the discontinuity is meaningless.
            self.last_marker_index = None;
            self.marker_period_estimate = None;
            self.ring_first_index = first_index;
            self.rate_hz = rate_hz;
        }
        self.samples.extend(codes.iter().copied());
        self.latest = codes.last().copied();
        self.device_dropped = device_dropped;
        self.last_update_unix_ms = now_unix_ms();

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
            // Learn the total-power anchor as we go. The latch survives segment
            // restarts on purpose: a rate change, a drop or an acquisition
            // handover does not move the optics, and the calibration sweep that
            // teaches the anchor is followed by exactly such a handover.
            let cell_mean = f64::from(cell.sum) / SUMMARY_CELL as f64;
            if self.observed_peak_code.is_none_or(|peak| cell_mean > peak) {
                self.observed_peak_code = Some(cell_mean);
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
        // Drop markers that fell out of the retained ring window.
        while self
            .markers
            .front()
            .is_some_and(|&index| index < self.ring_first_index)
        {
            self.markers.pop_front();
        }
        while self
            .comparator_markers
            .front()
            .is_some_and(|&(index, _)| index < self.ring_first_index)
        {
            self.comparator_markers.pop_front();
        }
    }

    /// Records a phase-0 marker (device sample index) if it sits inside the
    /// current ring window. Bounded so a marker storm cannot grow unbounded.
    fn push_marker(&mut self, sample_index: u64) {
        if sample_index < self.ring_first_index {
            return;
        }
        if self
            .markers
            .back()
            .is_some_and(|&last| last == sample_index)
        {
            return; // ignore duplicate stamps
        }
        if let Some(previous) = self.last_marker_index {
            if sample_index > previous {
                self.marker_period_estimate = Some((sample_index - previous) as f64);
            }
        }
        self.last_marker_index = Some(sample_index);
        self.markers.push_back(sample_index);
        while self.markers.len() > MAX_MARKERS {
            self.markers.pop_front();
        }
        self.last_update_unix_ms = now_unix_ms();
    }

    fn push_comparator_marker(&mut self, sample_index: u64, level: u8) {
        if sample_index < self.ring_first_index {
            return;
        }
        if self
            .comparator_markers
            .back()
            .is_some_and(|&(last, _)| last == sample_index)
        {
            return;
        }
        self.comparator_markers.push_back((sample_index, level));
        while self.comparator_markers.len() > MAX_MARKERS {
            self.comparator_markers.pop_front();
        }
        self.last_update_unix_ms = now_unix_ms();
    }

    /// How many trailing samples the optical log-contrast is estimated over,
    /// with the whole modulation cycles that window covers.
    ///
    /// `a` is peak-to-peak, so the window has to span whole cycles: sized to
    /// [`CONTRAST_WINDOW_CYCLES`] of the marker-measured period, floored at
    /// [`CONTRAST_WINDOW_SAMPLES`] so nothing gets shorter than today at high
    /// `f`, and capped by what the ring actually retains. `covered_cycles` is
    /// `None` when there is no period to measure against — then the caller can
    /// only fall back to the fixed window and say so.
    #[cfg(test)]
    fn contrast_window(&self) -> (usize, Option<f64>) {
        let available = self.samples.len();
        let Some(period) = self.marker_period_samples() else {
            return (available.min(CONTRAST_WINDOW_SAMPLES), None);
        };
        let wanted = (period * CONTRAST_WINDOW_CYCLES).ceil() as usize;
        let window = wanted.max(CONTRAST_WINDOW_SAMPLES).min(available);
        (window, Some(window as f64 / period))
    }

    /// Mean marker spacing in samples, i.e. the modulation period on the device
    /// clock — the trigger *defining* the frequency. `None` with < 2 markers.
    fn marker_period_samples(&self) -> Option<f64> {
        if self.markers.len() < 2 {
            // Below one retained cycle only the remembered interval is left.
            return self.marker_period_estimate;
        }
        let first = *self.markers.front()?;
        let last = *self.markers.back()?;
        let spans = (self.markers.len() - 1) as f64;
        let period = last.saturating_sub(first) as f64 / spans;
        (period > 0.0).then_some(period)
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
    sidecar_path: PathBuf,
    pdq_path_label: String,
    sidecar_path_label: String,
    run_id: RunId,
    opened_at_unix_ms: u64,
    stream_epoch: u64,
    first_sample_index: Option<u64>,
    metadata: BTreeMap<String, String>,
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

impl RecordingSink {
    fn started_receipt(&self) -> PdqStartedReceiptV1 {
        PdqStartedReceiptV1 {
            run_id: self.run_id.clone(),
            pdq_path: self.pdq_path_label.clone(),
            sidecar_path: self.sidecar_path_label.clone(),
            opened_at_unix_ms: self.opened_at_unix_ms,
            stream_epoch: self.stream_epoch,
            first_sample_index: self.first_sample_index,
        }
    }
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
            // Windows opens with DTR deasserted; see ADR 032.
            .dtr_on_open(true)
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
                            // Synthesize phase-0 markers on the device clock so the
                            // trigger overlay and frequency work without hardware.
                            let block_end = next_index + MOCK_BLOCK_SAMPLES as u64;
                            let mut marker =
                                next_index.next_multiple_of(MOCK_MARKER_PERIOD_SAMPLES);
                            while marker < block_end {
                                state.push_marker(marker);
                                marker += MOCK_MARKER_PERIOD_SAMPLES;
                            }
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
            // A 0-byte read is EOF (e.g. a yanked USB device before the OS
            // surfaces an error). Spinning here burns a core while the UI
            // still says "reading", so back off and let the timeout path
            // report the stall.
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
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
            changed |= ingest_parse_event(event, shared, recording);
        }
        if changed {
            generation.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Applies one parsed stream event to the ring and to any active recording.
/// Split out of [`read_frames`] so the recording/ingest contract is testable
/// without a serial port. Returns whether anything observable changed.
fn ingest_parse_event(
    event: ParseEvent,
    shared: &Mutex<SharedState>,
    recording: &SharedRecording,
) -> bool {
    match event {
        ParseEvent::Frame(frame) => {
            if let Some(marker) = frame.marker() {
                // Record before the early return: the phase-0 marker is what
                // makes a recorded run phase-attributable offline, so it has
                // to reach the .pdq as well as the live ring. It carries no
                // samples, hence a sample count of 0.
                record_frame(recording, &frame, 0);
                if let Ok(mut state) = shared.lock() {
                    match marker.source {
                        stage_a_io::wire::MARKER_SOURCE_PHASE0 => {
                            state.push_marker(marker.sample_index);
                        }
                        stage_a_io::wire::MARKER_SOURCE_COMPARATOR => {
                            state.push_comparator_marker(marker.sample_index, marker.level);
                        }
                        _ => {}
                    }
                }
                return true;
            }
            let Some(codes) = frame.samples() else {
                return false; // Control/summary frames are not expected here.
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
            true
        }
        ParseEvent::Corruption {
            skipped_bytes,
            crc_failures,
        } => {
            if let Ok(mut state) = shared.lock() {
                state.resync_bytes += skipped_bytes as u64;
                state.crc_failures += crc_failures as u64;
            }
            true
        }
    }
}

pub struct StageAPhotodiodePlugin {
    enabled: bool,
    runtime_role: PluginRuntimeRole,
    effects_allowed: bool,
    owner_instance: OwnerInstanceId,
    lease: Option<ControlLease>,
    request_cache: VecDeque<(PluginServiceRequest, PluginServiceReply)>,
    requested_revision: Option<SemanticRevision>,
    acknowledged_revision: Option<SemanticRevision>,
    last_response: Option<PhotodiodeResponseV1>,
    last_finalized_recording: Option<PdqFinalizedReceiptV1>,
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
    /// Physical detector geometry. This changes the scientific contrast
    /// transform, unlike `mode`, which is display-only.
    placement: PhotodiodePlacementV1,
    /// Fraction of the local beam delivered to the detector. Used only as
    /// provenance because a fixed factor cancels from log contrast.
    splitter_fraction: f64,
    /// Session-local lamp-off reading for direct camera/emission paths. `None`
    /// is a scientific gate, not an implicit zero-dark calibration.
    direct_dark_reference: Option<StoredDirectDarkReference>,
    /// UI-synchronized draft. It becomes a calibration only through the
    /// explicit `Use manual dark` button, so settings replay cannot overwrite
    /// a measured reference.
    direct_dark_manual_volts: f64,
    window_s: f64,
    avg_samples: usize,
    avg_sync_freq_hz: f64,
    time_axis: TimeAxis,
    /// Overlay the phase-0 trigger markers on the chart (opt-in).
    show_markers: bool,
    data_dir: String,
    // -- momentary-button press forwarding (see PressLatch) --
    press_save_snapshot: PressLatch,
    press_capture_direct_dark: PressLatch,
    press_use_manual_direct_dark: PressLatch,
    press_record_start: PressLatch,
    press_record_stop: PressLatch,
}

/// Forwards momentary button presses across the host's UI-mirror → live-worker
/// settings snapshot. A click arrives as `true` on the clicked instance; the
/// other instance only ever sees the snapshot value from `get_setting`, so the
/// press is transported as a monotonic counter and a counter advance counts as
/// one press edge. The first counter a fresh instance sees is adopted silently
/// so a reloaded worker does not replay old presses. Without this, an
/// unguarded button `set_setting` fires on every settings sync — the
/// "snapshot files kept appearing" bug.
#[derive(Debug, Default, Clone, Copy)]
struct PressLatch {
    counter: u64,
    seen: Option<u64>,
}

impl PressLatch {
    /// Interprets a settings write to this button; returns true on a press edge.
    fn accept(&mut self, value: &Value) -> bool {
        if value.as_bool() == Some(true) {
            self.counter += 1;
            self.seen = Some(self.counter);
            return true;
        }
        let Some(incoming) = value.as_u64() else {
            return false;
        };
        match self.seen {
            None => {
                self.seen = Some(incoming);
                self.counter = self.counter.max(incoming);
                false
            }
            Some(seen) if incoming > seen => {
                self.seen = Some(incoming);
                self.counter = self.counter.max(incoming);
                true
            }
            Some(_) => false,
        }
    }

    fn value(&self) -> Value {
        json!(self.counter)
    }
}

#[derive(Clone)]
struct ControlLease {
    lease_id: LeaseId,
    holder: ClientId,
    run_id: Option<RunId>,
    expires_at_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct StoredDirectDarkReference {
    dark_id: String,
    source: PhotodiodeDarkSourceV1,
    dark_volts: f64,
    captured_at_unix_ms: u64,
}

impl StoredDirectDarkReference {
    fn published(&self, observed_at_unix_ms: u64) -> PhotodiodeDarkReferenceV1 {
        PhotodiodeDarkReferenceV1 {
            dark_id: self.dark_id.clone(),
            source: self.source,
            dark_volts: self.dark_volts,
            captured_at_unix_ms: self.captured_at_unix_ms,
            age_s: observed_at_unix_ms.saturating_sub(self.captured_at_unix_ms) as f64 / 1_000.0,
        }
    }
}

impl Default for StageAPhotodiodePlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            runtime_role: PluginRuntimeRole::UiMirror,
            effects_allowed: false,
            owner_instance: OwnerInstanceId::new(format!(
                "photodiode-{}-{}-{}",
                std::process::id(),
                now_unix_ms(),
                OWNER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )),
            lease: None,
            request_cache: VecDeque::new(),
            requested_revision: None,
            acknowledged_revision: None,
            last_response: None,
            last_finalized_recording: None,
            reader: None,
            shared: Arc::new(Mutex::new(SharedState::default())),
            generation: Arc::new(AtomicU64::new(1)),
            recording: Arc::new(Mutex::new(None)),
            last_error: None,
            last_save_note: None,
            connect_requested: false,
            port_hint: "auto".into(),
            mode: Mode::Raw,
            placement: PhotodiodePlacementV1::RejectedPort,
            splitter_fraction: 0.5,
            direct_dark_reference: None,
            direct_dark_manual_volts: 0.0,
            window_s: 10.0,
            avg_samples: 4,
            avg_sync_freq_hz: 0.0,
            time_axis: TimeAxis::BeforeNow,
            show_markers: false,
            data_dir: String::new(),
            press_save_snapshot: PressLatch::default(),
            press_capture_direct_dark: PressLatch::default(),
            press_use_manual_direct_dark: PressLatch::default(),
            press_record_start: PressLatch::default(),
            press_record_stop: PressLatch::default(),
        }
    }
}

impl StageAPhotodiodePlugin {
    fn connected(&self) -> bool {
        self.reader.is_some()
    }

    /// The ADC calibration handed to the contrast estimator.
    ///
    /// `dark_volts` is deliberately zero. A DC dark offset `D` cancels
    /// *exactly* out of the rejected-complement contrast once the total-power
    /// anchor is read from the same detector: the excitation is
    /// `(I_tot,obs − D) − (v − D) = I_tot,obs − v`, with no `D` left in it.
    /// Subtracting a separately entered dark from only one of the two sides is
    /// what would bias `a` — which is why there is no dark setting any more
    /// (ADR 024).
    fn adc_calibration(&self) -> AdcCalibration {
        AdcCalibration {
            volts_per_code: ADC_FULL_SCALE_VOLTS / ADC_MAX_CODE,
            offset_volts: 0.0,
            dark_volts: 0.0,
            full_scale_code: ADC_MAX_CODE as u16,
        }
    }

    fn published_direct_dark(&self) -> Option<PhotodiodeDarkReferenceV1> {
        self.direct_dark_reference
            .as_ref()
            .map(|reference| reference.published(now_unix_ms()))
    }

    fn direct_dark_volts(&self) -> Option<f64> {
        self.direct_dark_reference
            .as_ref()
            .map(|reference| reference.dark_volts)
    }

    /// The learned total-power anchor `I_tot` in volts, if the stream has run
    /// long enough to complete one summary cell.
    fn total_power_volts(&self, state: &SharedState) -> Option<f64> {
        state.observed_peak_code.map(code_to_volts)
    }

    /// [`Self::total_power_volts`] for callers that do not already hold the ring
    /// lock (sidecars, status entries, the chart transform).
    fn learned_anchor_volts(&self) -> Option<f64> {
        let state = self.shared.lock().ok()?;
        self.total_power_volts(&state)
    }

    fn connect(&mut self) {
        if self.reader.is_some() {
            return;
        }
        if self.runtime_role != PluginRuntimeRole::LiveWorker || !self.effects_allowed {
            self.last_error = Some("connection deferred: hardware effects are not allowed".into());
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

    /// Resolves a workflow-owned relative evidence path beneath `root_override`
    /// when the client named one, else beneath the configured data directory.
    /// Existing or newly created parent components must be real directories,
    /// never symlinks — that holds for either root.
    fn resolve_control_path(
        &self,
        label: &str,
        extension: &str,
        root_override: Option<&str>,
    ) -> Result<PathBuf, String> {
        let relative = Path::new(label);
        if relative.as_os_str().is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(
                "workflow recording paths must be non-empty relative paths without '..'".into(),
            );
        }
        if relative.extension().and_then(|value| value.to_str()) != Some(extension) {
            return Err(format!("workflow path must use the .{extension} extension"));
        }

        // A client-named root replaces the data directory entirely: a
        // coordinated run keeps every file of one measurement together, and
        // the owner's own Data section then has no bearing on it.
        let root = match root_override.map(str::trim).filter(|root| !root.is_empty()) {
            Some(root) => {
                let root = PathBuf::from(root);
                if !root.is_absolute() {
                    return Err("workflow recording root must be an absolute path".into());
                }
                root
            }
            None => self.resolved_data_dir()?,
        };
        std::fs::create_dir_all(&root)
            .map_err(|err| format!("creating {} failed: {err}", root.display()))?;
        let root = root
            .canonicalize()
            .map_err(|err| format!("resolving recording directory failed: {err}"))?;
        let mut parent = root.clone();
        if let Some(relative_parent) = relative.parent() {
            for component in relative_parent.components() {
                let Component::Normal(name) = component else {
                    return Err("invalid workflow recording path".into());
                };
                parent.push(name);
                match std::fs::symlink_metadata(&parent) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(format!(
                            "workflow path crosses symlink {}",
                            parent.display()
                        ));
                    }
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(format!("{} is not a directory", parent.display()));
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir(&parent).map_err(|err| {
                            format!("creating {} failed: {err}", parent.display())
                        })?;
                    }
                    Err(err) => {
                        return Err(format!("checking {} failed: {err}", parent.display()));
                    }
                }
                let canonical = parent
                    .canonicalize()
                    .map_err(|err| format!("resolving {} failed: {err}", parent.display()))?;
                if !canonical.starts_with(&root) {
                    return Err("workflow path escapes the recording directory".into());
                }
            }
        }
        let candidate = root.join(relative);
        if let Ok(metadata) = std::fs::symlink_metadata(&candidate) {
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "workflow target is a symlink: {}",
                    candidate.display()
                ));
            }
        }
        Ok(candidate)
    }

    fn begin_named_recording(
        &mut self,
        run_id: RunId,
        specification: &PdqStartSpecV1,
    ) -> Result<PdqStartedReceiptV1, ServiceErrorV1> {
        if !self.connected() {
            return Err(service_error(
                ServiceErrorCodeV1::NotConnected,
                "photodiode stream is not connected",
                true,
            ));
        }
        if specification.metadata.len() > 64
            || specification
                .metadata
                .iter()
                .any(|(key, value)| key.len() > 128 || value.len() > 1_024)
        {
            return Err(service_error(
                ServiceErrorCodeV1::InvalidCommand,
                "recording metadata exceeds owner bounds",
                false,
            ));
        }
        let (rate_hz, stream_epoch) = self
            .shared
            .lock()
            .map(|state| (state.rate_hz, state.segments))
            .unwrap_or((0, 0));
        if specification
            .expected_sample_rate_hz
            .is_some_and(|expected| rate_hz != 0 && expected != rate_hz)
            || specification
                .expected_stream_epoch
                .is_some_and(|expected| expected != stream_epoch)
        {
            return Err(service_error(
                ServiceErrorCodeV1::Integrity,
                "live photodiode stream does not match the requested epoch or sample rate",
                true,
            ));
        }
        let root = specification.root_dir.as_deref();
        let pdq_path = self
            .resolve_control_path(&specification.pdq_path, "pdq", root)
            .map_err(|message| service_error(ServiceErrorCodeV1::InvalidPath, message, false))?;
        let sidecar_path = self
            .resolve_control_path(&specification.sidecar_path, "json", root)
            .map_err(|message| service_error(ServiceErrorCodeV1::InvalidPath, message, false))?;
        if pdq_path == sidecar_path {
            return Err(service_error(
                ServiceErrorCodeV1::InvalidPath,
                "PDQ and sidecar paths must differ",
                false,
            ));
        }
        self.open_recording(
            run_id,
            pdq_path,
            sidecar_path,
            specification.pdq_path.clone(),
            specification.sidecar_path.clone(),
            specification.metadata.clone(),
            true,
        )
        .map_err(|message| service_error(ServiceErrorCodeV1::Io, message, false))
    }

    fn start_recording(&mut self) -> Result<(), String> {
        if self.runtime_role != PluginRuntimeRole::LiveWorker || !self.effects_allowed {
            return Err("recording is allowed only on the active live worker".into());
        }
        if self.lease.is_some() {
            return Err("manual recording is locked while a workflow lease is active".into());
        }
        let dir = self.resolved_data_dir()?;
        let slug = timestamp_slug();
        let pdq_path = dir.join(format!("pd_rec_{slug}.pdq"));
        let sidecar_path = pdq_path.with_extension("json");
        self.open_recording(
            RunId::new(format!("manual-{slug}")),
            pdq_path.clone(),
            sidecar_path.clone(),
            pdq_path.to_string_lossy().into_owned(),
            sidecar_path.to_string_lossy().into_owned(),
            BTreeMap::new(),
            false,
        )?;
        self.last_save_note = Some(format!("recording → {}", pdq_path.display()));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn open_recording(
        &mut self,
        run_id: RunId,
        pdq_path: PathBuf,
        sidecar_path: PathBuf,
        pdq_path_label: String,
        sidecar_path_label: String,
        metadata: BTreeMap<String, String>,
        exclusive: bool,
    ) -> Result<PdqStartedReceiptV1, String> {
        if self.recording_active() {
            return Err("a photodiode recording is already active".into());
        }
        let writer = if exclusive {
            PdqWriter::create_new(&pdq_path)
        } else {
            PdqWriter::create(&pdq_path)
        }
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
        let (stream_epoch, first_sample_index) = self
            .shared
            .lock()
            .map(|state| {
                (
                    state.segments,
                    (!state.samples.is_empty())
                        .then_some(state.ring_first_index + state.samples.len() as u64),
                )
            })
            .unwrap_or((0, None));
        let opened_at_unix_ms = now_unix_ms();
        let started_slug = timestamp_slug();
        if exclusive {
            let started = json!({
                "kind": "recording_in_progress",
                "run_id": run_id.as_str(),
                "opened_at_unix_ms": opened_at_unix_ms,
                "pdq_path": pdq_path_label,
                "metadata": metadata,
            });
            if let Err(err) = write_json_new(&sidecar_path, &started) {
                drop(writer);
                let _ = std::fs::remove_file(&pdq_path);
                return Err(err);
            }
        }
        let sink = RecordingSink {
            writer,
            pdq_path: pdq_path.clone(),
            sidecar_path,
            pdq_path_label,
            sidecar_path_label,
            run_id,
            opened_at_unix_ms,
            stream_epoch,
            first_sample_index,
            metadata,
            started_slug,
            samples_written: 0,
            write_error: None,
            start_crc_failures: crc,
            start_resync_bytes: resync,
            start_device_dropped: dropped,
            start_segments: segments,
        };
        let receipt = sink.started_receipt();
        if let Ok(mut slot) = self.recording.lock() {
            *slot = Some(sink);
        }
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(receipt)
    }

    fn stop_recording(&mut self) -> Result<(), String> {
        self.finalize_recording(PdqTerminationV1::OperatorStopped)
            .map(|_| ())
    }

    fn finalize_recording(
        &mut self,
        termination: PdqTerminationV1,
    ) -> Result<Option<PdqFinalizedReceiptV1>, String> {
        let Some(sink) = self.recording.lock().ok().and_then(|mut slot| slot.take()) else {
            return Ok(None);
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
        let pdq_path = sink.pdq_path.clone();
        let sidecar_path = sink.sidecar_path.clone();
        let run_id = sink.run_id.clone();
        let opened_at_unix_ms = sink.opened_at_unix_ms;
        let pdq_path_label = sink.pdq_path_label.clone();
        let sidecar_path_label = sink.sidecar_path_label.clone();
        let metadata = sink.metadata.clone();
        let summary = sink
            .writer
            .finish(integrity)
            .map_err(|err| format!("finishing recording failed: {err}"))?;
        let contract_integrity = contract_integrity(summary.integrity, summary.sample_segments);
        let receipt = PdqFinalizedReceiptV1 {
            run_id: run_id.clone(),
            pdq_path: pdq_path_label,
            sidecar_path: sidecar_path_label,
            opened_at_unix_ms,
            finalized_at_unix_ms: now_unix_ms(),
            file_size_bytes: summary.bytes_written,
            sha256: Sha256V1::parse(summary.file_sha256_hex())
                .map_err(|err| format!("invalid recording digest: {err}"))?,
            frames_written: summary.frames_written,
            sample_frames_written: summary.sample_frames_written,
            sample_range: summary.sample_range.map(|range| SampleRangeV1 {
                first_sample_index: range.first_sample_index,
                end_sample_index_exclusive: range.end_sample_index_exclusive,
                sample_count: range.sample_count,
            }),
            sample_rate_hz: summary.sample_rate_hz,
            segment_count: summary.sample_segments,
            integrity: contract_integrity,
            termination,
            valid: summary.valid && write_error.is_none(),
        };
        let sidecar = json!({
            "kind": "recording",
            "run_id": run_id,
            "started_utc": started,
            "stopped_utc": timestamp_slug(),
            "port": self.port_hint,
            "sample_rate_hz": rate_hz,
            "samples_written": samples,
            "pdq_path": summary.path,
            "pdq_frames": summary.frames_written,
            "pdq_bytes": summary.bytes_written,
            "pdq_crc32": summary.file_crc32,
            "pdq_sha256": receipt.sha256.as_str(),
            "metadata": metadata,
            "termination": receipt.termination,
            "adc": { "bits": 12, "full_scale_volts": ADC_FULL_SCALE_VOLTS },
            "display_mode": self.mode.name(),
            "photodiode_placement": self.placement,
            "splitter_fraction": (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then_some(self.splitter_fraction),
            "direct_dark_volts": (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then(|| self.direct_dark_volts()).flatten(),
            "direct_dark_reference": (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then(|| self.published_direct_dark()).flatten(),
            "total_power_volts": (self.placement == PhotodiodePlacementV1::RejectedPort)
                .then(|| self.learned_anchor_volts()).flatten(),
            "total_power_source": (self.placement == PhotodiodePlacementV1::RejectedPort)
                .then_some("observed-peak"),
            "integrity": {
                "resync_bytes": summary.integrity.skipped_bytes,
                "crc_failures": summary.integrity.crc_failures,
                "segment_restarts": summary.integrity.sequence_gaps,
                "device_dropped_samples": summary.integrity.dropped_samples,
            },
            "valid": summary.valid && write_error.is_none(),
            "write_error": write_error,
        });
        write_json(&sidecar_path, &sidecar)?;
        self.last_save_note = Some(format!(
            "saved recording {} ({} samples)",
            pdq_path.display(),
            samples
        ));
        self.last_finalized_recording = Some(receipt.clone());
        self.generation.fetch_add(1, Ordering::Relaxed);
        Ok(Some(receipt))
    }

    fn lease_snapshot(&self) -> Option<LeaseSnapshotV1> {
        self.lease.as_ref().map(|lease| LeaseSnapshotV1 {
            lease_id: lease.lease_id.clone(),
            holder: lease.holder.clone(),
            expires_at_unix_ms: lease.expires_at_unix_ms,
            run_id: lease.run_id.clone(),
        })
    }

    fn require_lease(&self, request: &PhotodiodeRequestV1) -> Result<(), ServiceErrorV1> {
        let lease = self.lease.as_ref().ok_or_else(|| {
            service_error(
                ServiceErrorCodeV1::LeaseRequired,
                "the photodiode owner requires an active automation lease",
                false,
            )
        })?;
        if now_unix_ms() > lease.expires_at_unix_ms {
            return Err(service_error(
                ServiceErrorCodeV1::LeaseExpired,
                "the photodiode automation lease expired",
                false,
            ));
        }
        if request.lease_id.as_ref() != Some(&lease.lease_id)
            || request.requester != lease.holder
            || request.run_id != lease.run_id
        {
            return Err(service_error(
                ServiceErrorCodeV1::LeaseMismatch,
                "request lease, holder, or run does not match the active lease",
                false,
            ));
        }
        Ok(())
    }

    fn require_new_revision(
        &self,
        request: &PhotodiodeRequestV1,
    ) -> Result<SemanticRevision, ServiceErrorV1> {
        let revision = request.requested_revision.ok_or_else(|| {
            service_error(
                ServiceErrorCodeV1::InvalidCommand,
                "recording transitions require requested_revision",
                false,
            )
        })?;
        if self
            .requested_revision
            .is_some_and(|current| revision <= current)
        {
            return Err(service_error(
                ServiceErrorCodeV1::StaleRequest,
                "requested_revision must be newer than the current photodiode state",
                false,
            ));
        }
        Ok(revision)
    }

    fn immediate_response(
        &mut self,
        request: &PhotodiodeRequestV1,
        receipt: Option<PdqReceiptV1>,
    ) -> PhotodiodeResponseV1 {
        let response = PhotodiodeResponseV1 {
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: request.request_id,
                owner_instance: self.owner_instance.clone(),
                run_id: request.run_id.clone(),
                requested_revision: request.requested_revision,
                acknowledged_revision: self.acknowledged_revision,
                outcome: RequestOutcomeV1::Applied,
                completed_at_unix_ms: Some(now_unix_ms()),
                error: None,
            },
            receipt,
        };
        self.last_response = Some(response.clone());
        self.generation.fetch_add(1, Ordering::Relaxed);
        response
    }

    fn handle_photodiode_command(
        &mut self,
        request: &PhotodiodeRequestV1,
    ) -> Result<PhotodiodeResponseV1, ServiceErrorV1> {
        match &request.command {
            PhotodiodeCommandV1::Connect => {
                if self.lease.is_some() {
                    return Err(service_error(
                        ServiceErrorCodeV1::LeaseBusy,
                        "connection cannot be changed while leased",
                        false,
                    ));
                }
                self.connect_requested = true;
                self.connect();
                if !self.connected() {
                    return Err(service_error(
                        ServiceErrorCodeV1::Transport,
                        self.last_error
                            .clone()
                            .unwrap_or_else(|| "photodiode connection failed".into()),
                        true,
                    ));
                }
                Ok(self.immediate_response(request, None))
            }
            PhotodiodeCommandV1::Disconnect {
                finalize_recording,
                reason,
            } => {
                if self.lease.is_some() {
                    return Err(service_error(
                        ServiceErrorCodeV1::LeaseBusy,
                        "use ReleaseLease while the owner is leased",
                        false,
                    ));
                }
                let receipt = if *finalize_recording {
                    self.finalize_recording(PdqTerminationV1::OperatorStopped)
                        .map_err(|message| service_error(ServiceErrorCodeV1::Io, message, false))?
                        .map(PdqReceiptV1::Finalized)
                } else {
                    None
                };
                self.connect_requested = false;
                self.disconnect();
                self.last_error = Some(format!("disconnected by service: {reason}"));
                Ok(self.immediate_response(request, receipt))
            }
            PhotodiodeCommandV1::AcquireLease { ttl_ms } => {
                let lease_id = request.lease_id.clone().ok_or_else(|| {
                    service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "AcquireLease requires lease_id",
                        false,
                    )
                })?;
                if let Some(active) = &self.lease {
                    if active.lease_id != lease_id || active.holder != request.requester {
                        return Err(service_error(
                            ServiceErrorCodeV1::LeaseBusy,
                            "the photodiode owner is already leased",
                            true,
                        ));
                    }
                }
                self.lease = Some(ControlLease {
                    lease_id,
                    holder: request.requester.clone(),
                    run_id: request.run_id.clone(),
                    expires_at_unix_ms: lease_deadline(*ttl_ms),
                });
                Ok(self.immediate_response(request, None))
            }
            PhotodiodeCommandV1::RenewLease { ttl_ms } => {
                self.require_lease(request)?;
                if let Some(lease) = &mut self.lease {
                    lease.expires_at_unix_ms = lease_deadline(*ttl_ms);
                }
                Ok(self.immediate_response(request, None))
            }
            PhotodiodeCommandV1::ReleaseLease {
                finalize_recording,
                reason,
            } => {
                self.require_lease(request)?;
                let receipt = if *finalize_recording {
                    self.finalize_recording(PdqTerminationV1::OperatorStopped)
                        .map_err(|message| service_error(ServiceErrorCodeV1::Io, message, false))?
                        .map(PdqReceiptV1::Finalized)
                } else if self.recording_active() {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "cannot release a lease with an active recording unless it is finalized",
                        false,
                    ));
                } else {
                    None
                };
                self.lease = None;
                self.last_error = Some(format!("automation lease released: {reason}"));
                Ok(self.immediate_response(request, receipt))
            }
            PhotodiodeCommandV1::BeginRecording { specification } => {
                self.require_lease(request)?;
                let revision = self.require_new_revision(request)?;
                let run_id = request.run_id.clone().ok_or_else(|| {
                    service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "BeginRecording requires run_id",
                        false,
                    )
                })?;
                let started = self.begin_named_recording(run_id, specification)?;
                self.requested_revision = Some(revision);
                self.acknowledged_revision = Some(revision);
                Ok(self.immediate_response(request, Some(PdqReceiptV1::Started(started))))
            }
            PhotodiodeCommandV1::FinalizeRecording { termination } => {
                self.require_lease(request)?;
                let revision = self.require_new_revision(request)?;
                let finalized = self
                    .finalize_recording(*termination)
                    .map_err(|message| service_error(ServiceErrorCodeV1::Io, message, false))?
                    .ok_or_else(|| {
                        service_error(
                            ServiceErrorCodeV1::InvalidCommand,
                            "no photodiode recording is active",
                            false,
                        )
                    })?;
                self.requested_revision = Some(revision);
                self.acknowledged_revision = Some(revision);
                Ok(self.immediate_response(request, Some(PdqReceiptV1::Finalized(finalized))))
            }
            PhotodiodeCommandV1::AbortRecording { reason } => {
                self.require_lease(request)?;
                let revision = self.require_new_revision(request)?;
                let finalized = self
                    .finalize_recording(PdqTerminationV1::Aborted)
                    .map_err(|message| service_error(ServiceErrorCodeV1::Io, message, false))?
                    .ok_or_else(|| {
                        service_error(
                            ServiceErrorCodeV1::InvalidCommand,
                            "no photodiode recording is active",
                            false,
                        )
                    })?;
                self.requested_revision = Some(revision);
                self.acknowledged_revision = Some(revision);
                self.last_error = Some(format!("recording aborted: {reason}"));
                Ok(self.immediate_response(request, Some(PdqReceiptV1::Finalized(finalized))))
            }
        }
    }

    /// Live optical log-contrast `a` from a marker-bounded ring window.
    ///
    /// The physical placement selects the transform. The historical PBS
    /// rejected port measures a complement and therefore needs its observed
    /// full-extinction anchor. Camera/emission-path placements measure their
    /// local beam directly and use the lamp-off dark reference; `I_tot` is not
    /// defined or consulted in those geometries.
    ///
    /// The display [`Mode`] is presentational only. It must never reach this
    /// function: A1's amplitude sweep settles on this value against a target
    /// `a`, so letting a display toggle change its meaning would silently
    /// retarget the sweep and write a wrong `measured_a` into every sidecar.
    ///
    /// `Err` — never a silent `None` — when there is no valid whole-cycle
    /// window or no learned total-power anchor: the rejection reason is what
    /// the status readout and the A1 panel render instead of `a`, so a withheld
    /// `a` names the gate the operator has to fix.
    fn optical_summary_result(
        &self,
        state: &SharedState,
    ) -> Result<PhotodiodeOpticalSummaryV1, EstimateError> {
        let total_power_volts = match self.placement {
            PhotodiodePlacementV1::RejectedPort => Some(
                self.total_power_volts(state)
                    .ok_or(EstimateError::MissingTotalPowerAnchor)?,
            ),
            PhotodiodePlacementV1::CameraPath | PhotodiodePlacementV1::EmissionPath => None,
        };
        let direct_dark = match self.placement {
            PhotodiodePlacementV1::RejectedPort => None,
            PhotodiodePlacementV1::CameraPath | PhotodiodePlacementV1::EmissionPath => Some(
                self.direct_dark_reference
                    .as_ref()
                    .ok_or(EstimateError::MissingDirectDarkReference)?,
            ),
        };

        let ring_end = state.ring_first_index + state.samples.len() as u64;
        let markers: Vec<u64> = state
            .markers
            .iter()
            .copied()
            .filter(|index| *index >= state.ring_first_index && *index <= ring_end)
            .collect();
        if markers.len() < 3 {
            return Err(EstimateError::IncompleteModulationCycles {
                marker_count: markers.len(),
                max_samples: state.samples.len(),
            });
        }

        // End on the newest complete phase-0 boundary. Start at least two
        // complete cycles earlier, then include up to the configured number of
        // older complete cycles. At low frequency this grows with the measured
        // period instead of truncating the waveform to a fixed sample count.
        let end_index = *markers.last().expect("three markers checked");
        let desired_cycles = CONTRAST_WINDOW_CYCLES as usize;
        let start_marker = markers.len().saturating_sub(desired_cycles + 1);
        let start_index = markers[start_marker];
        let start = start_index.saturating_sub(state.ring_first_index) as usize;
        let end = end_index.saturating_sub(state.ring_first_index) as usize;
        let window: Vec<u16> = state.samples.range(start..end).copied().collect();
        let rate_hz = f64::from(state.rate_hz.max(1));
        let covered_cycles = Some((markers.len() - 1 - start_marker) as f64);
        let window_seconds = end_index.saturating_sub(start_index) as f64 / rate_hz;
        let mut calibration = self.adc_calibration();
        let geometry = match total_power_volts {
            Some(total_power_volts) => ContrastGeometry::RejectedComplement { total_power_volts },
            None => {
                calibration.dark_volts = direct_dark
                    .expect("direct placement checked above")
                    .dark_volts;
                ContrastGeometry::Direct
            }
        };
        let estimate = estimate_contrast(&window, &calibration, geometry)?;
        let run_id = self
            .lease
            .as_ref()
            .and_then(|lease| lease.run_id.clone())
            .unwrap_or_else(|| RunId::from("live"));
        Ok(PhotodiodeOpticalSummaryV1 {
            run_id,
            calibration: PhotodiodeCalibrationV1 {
                adc_calibration_id: "adc-default".into(),
                // The complement is dark-invariant, so there is no dark level
                // to name — say that rather than imply an unmeasured zero.
                dark_id: match self.placement {
                    PhotodiodePlacementV1::RejectedPort => "dark-cancels".into(),
                    _ => direct_dark
                        .expect("direct placement checked above")
                        .dark_id
                        .clone(),
                },
                // Provenance for an anchor nobody typed: the detector sample
                // index the learned peak was still valid at.
                anchor_id: total_power_volts.map(|_| format!("observed-peak@{ring_end}")),
                dark_volts: calibration.dark_volts,
                dark_reference: direct_dark.map(|reference| reference.published(now_unix_ms())),
                total_power_volts,
            },
            placement: self.placement,
            splitter_fraction: (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then_some(self.splitter_fraction),
            measured_log_contrast: estimate.a,
            log_contrast_stddev: None,
            excitation_min_volts: estimate.v_min_volts,
            excitation_max_volts: estimate.v_max_volts,
            // The excitation minimum is measured from the same anchor the
            // detector samples are, so it *is* the margin above the floor.
            // Same number as `excitation_min_volts` by construction; kept
            // because the contract publishes both.
            excitation_headroom_volts: estimate.v_min_volts,
            low_clip_fraction: estimate.low_clip_fraction,
            high_clip_fraction: estimate.high_clip_fraction,
            measured_frequency_hz: (state.rate_hz > 0).then(|| {
                let cycles = markers.len() - 1 - start_marker;
                let period_samples = end_index.saturating_sub(start_index) as f64 / cycles as f64;
                f64::from(state.rate_hz) / period_samples
            }),
            fundamental_phase_rad: None,
            total_harmonic_distortion: None,
            window_seconds: Some(window_seconds),
            covered_cycles,
        })
    }

    /// [`Self::optical_summary_result`] reduced to the accepted value, for tests
    /// that assert on `a` itself rather than on which gate refused it.
    #[cfg(test)]
    fn optical_summary(&self, state: &SharedState) -> Option<PhotodiodeOpticalSummaryV1> {
        self.optical_summary_result(state).ok()
    }

    /// Locks the ring and returns the current optical log-contrast summary,
    /// keeping the rejection reason so the caller can explain a withheld `a`.
    fn latest_optical_result(&self) -> Option<Result<PhotodiodeOpticalSummaryV1, EstimateError>> {
        let state = self.shared.lock().ok()?;
        (!state.samples.is_empty()).then(|| self.optical_summary_result(&state))
    }

    fn control_summary(&self) -> PhotodiodeSummaryV1 {
        let (stream, connection, observed_at, optical_summary, optical_unavailable) =
            match self.shared.lock() {
                Ok(state) => {
                    let sample_range = (!state.samples.is_empty()).then_some(SampleRangeV1 {
                        first_sample_index: state.ring_first_index,
                        end_sample_index_exclusive: state.ring_first_index
                            + state.samples.len() as u64,
                        sample_count: state.samples.len() as u64,
                    });
                    // Publish the refusal reason alongside the absent summary: A1
                    // gates the a₀ lock and the frequency sweep on `a`, and without
                    // this the operator only learns that `a` is missing, not which
                    // gate to fix.
                    let (optical_summary, optical_unavailable) = match (!state.samples.is_empty())
                        .then(|| self.optical_summary_result(&state))
                    {
                        Some(Ok(summary)) => (Some(summary), None),
                        Some(Err(error)) => (None, Some(error.to_string())),
                        None => (None, None),
                    };
                    let level = self.current_level(&state);
                    let connection = if self.connected() {
                        ConnectionStateV1::Connected {
                            port_label: self.port_hint.clone(),
                            firmware_version: None,
                        }
                    } else if let Some(message) =
                        state.error.clone().or_else(|| self.last_error.clone())
                    {
                        ConnectionStateV1::Faulted { message }
                    } else if self.connect_requested {
                        ConnectionStateV1::Connecting
                    } else {
                        ConnectionStateV1::Disconnected
                    };
                    (
                        PhotodiodeStreamV1 {
                            stream_epoch: state.segments,
                            sample_range,
                            sample_rate_hz: (state.rate_hz != 0).then_some(state.rate_hz),
                            latest_adc_code: state.latest,
                            integrity: StreamIntegrityV1 {
                                skipped_bytes: state.resync_bytes,
                                crc_failures: state.crc_failures,
                                sequence_gaps: state.segments,
                                dropped_samples: u64::from(state.device_dropped),
                                segment_restarts: state.segments,
                                truncated_bytes: 0,
                            },
                            level,
                        },
                        connection,
                        state.last_update_unix_ms,
                        optical_summary,
                        optical_unavailable,
                    )
                }
                Err(_) => (
                    PhotodiodeStreamV1 {
                        stream_epoch: 0,
                        sample_range: None,
                        sample_rate_hz: None,
                        latest_adc_code: None,
                        integrity: StreamIntegrityV1::default(),
                        level: None,
                    },
                    ConnectionStateV1::Faulted {
                        message: "photodiode state lock poisoned".into(),
                    },
                    0,
                    None,
                    Some("photodiode state lock poisoned".to_owned()),
                ),
            };
        let active_recording = self
            .recording
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().map(RecordingSink::started_receipt));
        let synchronization = match (
            self.lease.as_ref().and_then(|lease| lease.run_id.clone()),
            self.requested_revision,
            self.acknowledged_revision,
        ) {
            (Some(run_id), Some(requested), Some(acknowledged)) if requested == acknowledged => {
                SynchronizationV1::Synced {
                    run_id,
                    acknowledged_revision: acknowledged,
                    stream_epoch: Some(stream.stream_epoch),
                }
            }
            (None, _, _) => SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::NoLease,
                detail: None,
            },
            _ => SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::RequestedRevisionNotAcknowledged,
                detail: None,
            },
        };
        PhotodiodeSummaryV1 {
            contract_version: CONTRACT_VERSION_V1,
            owner_instance: self.owner_instance.clone(),
            service_revision: self.generation.load(Ordering::Relaxed),
            connection,
            lease: self.lease_snapshot(),
            active_run_id: self.lease.as_ref().and_then(|lease| lease.run_id.clone()),
            requested_revision: self.requested_revision,
            acknowledged_revision: self.acknowledged_revision,
            stream,
            // Automation clients need this to refuse a coordinated run before
            // it starts the camera, instead of failing at BeginRecording.
            data_dir: Some(self.data_dir.trim().to_owned()).filter(|folder| !folder.is_empty()),
            active_recording,
            last_finalized_recording: self.last_finalized_recording.clone(),
            optical_summary,
            placement: self.placement,
            splitter_fraction: (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then_some(self.splitter_fraction),
            dark_reference: (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then(|| self.published_direct_dark())
                .flatten(),
            optical_unavailable,
            synchronization,
            last_response: self.last_response.clone(),
            freshness: FreshnessV1 {
                observed_at_unix_ms: if observed_at == 0 {
                    now_unix_ms()
                } else {
                    observed_at
                },
                valid_for_ms: SNAPSHOT_VALID_FOR_MS,
            },
        }
    }

    fn expire_lease_if_needed(&mut self) {
        if self
            .lease
            .as_ref()
            .is_none_or(|lease| now_unix_ms() <= lease.expires_at_unix_ms)
        {
            return;
        }
        if let Err(error) = self.finalize_recording(PdqTerminationV1::LeaseExpired) {
            self.last_error = Some(error);
        } else {
            self.last_error = Some("automation lease expired; recording finalized".into());
        }
        self.lease = None;
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    fn apply_execution_context(&mut self, execution: &augur_plugin_api::ExecutionContext) {
        let allowed = self.runtime_role == PluginRuntimeRole::LiveWorker
            && execution.hardware_effects_allowed();
        self.effects_allowed = allowed;
        if !allowed {
            if let Err(error) = self.finalize_recording(PdqTerminationV1::Aborted) {
                self.last_error = Some(error);
            }
            // Deliberately keep `connect_requested`: it is the operator's
            // *intent*, and this branch is what the UI mirror runs on every
            // control tick. Clearing it there resets the checkbox before the
            // host can sample it, so the live worker never sees the request.
            // `connect()` is already guarded on the role, so the intent alone
            // is inert here; the worker acts on it below.
            self.disconnect();
            self.lease = None;
            return;
        }
        self.expire_lease_if_needed();
        if self.connect_requested && self.reader.is_none() {
            self.connect();
        }
    }

    /// Dumps the current monitor cache (ring) as CSV + JSON sidecar. Raw
    /// codes and raw volts only — mode/reference land in the sidecar so
    /// EXCITATION values stay derivable without baking display state into
    /// the data.
    fn save_cache_snapshot(&mut self) -> Result<(), String> {
        if self.runtime_role != PluginRuntimeRole::LiveWorker || !self.effects_allowed {
            return Err("saving is allowed only on the active live worker".into());
        }
        let dir = self.resolved_data_dir()?;
        let slug = timestamp_slug();
        let csv_path = dir.join(format!("pd_cache_{slug}.csv"));
        // Copy the ring out under the lock and release it before touching the
        // filesystem: holding it across up to RING_MAX_SAMPLES writeln! calls
        // blocks the reader thread, overruns the serial input buffer and shows
        // up as dropped samples plus a segment restart in any recording that is
        // in flight.
        let (samples, rate_hz, ring_first_index, cache_seconds, integrity) = {
            let state = self
                .shared
                .lock()
                .map_err(|_| "photodiode state lock poisoned".to_owned())?;
            if state.samples.is_empty() || state.rate_hz == 0 {
                return Err("no samples cached yet".into());
            }
            let samples: Vec<u16> = state.samples.iter().copied().collect();
            let integrity = json!({
                "resync_bytes": state.resync_bytes,
                "crc_failures": state.crc_failures,
                "segment_restarts": state.segments,
                "device_dropped_samples": state.device_dropped,
            });
            (
                samples,
                state.rate_hz,
                state.ring_first_index,
                state.cache_seconds,
                integrity,
            )
        };

        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("creating {} failed: {err}", dir.display()))?;
        let file = File::create(&csv_path)
            .map_err(|err| format!("creating {} failed: {err}", csv_path.display()))?;
        let mut writer = BufWriter::new(file);
        let rate = f64::from(rate_hz);
        writeln!(writer, "sample_index,t_s,code,volts")
            .map_err(|err| format!("writing CSV failed: {err}"))?;
        for (offset, &code) in samples.iter().enumerate() {
            let index = ring_first_index + offset as u64;
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

        let sample_count = samples.len();
        let sidecar = json!({
            "kind": "cache_snapshot",
            "created_utc": slug,
            "port": self.port_hint,
            "sample_rate_hz": rate_hz,
            "samples": sample_count,
            "first_sample_index": ring_first_index,
            "cache_seconds": cache_seconds,
            "csv_path": csv_path,
            "adc": { "bits": 12, "full_scale_volts": ADC_FULL_SCALE_VOLTS },
            "display_mode": self.mode.name(),
            "photodiode_placement": self.placement,
            "splitter_fraction": (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then_some(self.splitter_fraction),
            "direct_dark_volts": (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then(|| self.direct_dark_volts()).flatten(),
            "direct_dark_reference": (self.placement != PhotodiodePlacementV1::RejectedPort)
                .then(|| self.published_direct_dark()).flatten(),
            "total_power_volts": (self.placement == PhotodiodePlacementV1::RejectedPort)
                .then(|| self.learned_anchor_volts()).flatten(),
            "total_power_source": (self.placement == PhotodiodePlacementV1::RejectedPort)
                .then_some("observed-peak"),
            "time_base": "t_s = sample_index / sample_rate_hz, device clock, segment-relative",
            "integrity": integrity,
        });
        write_json(&csv_path.with_extension("json"), &sidecar)?;
        self.last_save_note = Some(format!(
            "saved cache {} ({sample_count} samples)",
            csv_path.display()
        ));
        Ok(())
    }

    /// Value shown for one sample under the current mode, in volts.
    ///
    /// EXCITATION needs the learned total-power anchor to take the complement.
    /// The anchor is latched from the first completed summary cell — a few
    /// milliseconds after the stream opens — so the `None` arm is only ever the
    /// very first repaint; it shows the raw reading rather than a trace
    /// referenced to a number that does not exist yet.
    fn display_volts(&self, code: f64, anchor_volts: Option<f64>) -> f64 {
        match (self.mode, self.placement, anchor_volts) {
            (Mode::Raw, _, _) => code_to_volts(code),
            (Mode::Excitation, PhotodiodePlacementV1::RejectedPort, Some(anchor)) => {
                anchor - code_to_volts(code)
            }
            (Mode::Excitation, PhotodiodePlacementV1::RejectedPort, None) => code_to_volts(code),
            (Mode::Excitation, _, _) => self
                .direct_dark_volts()
                .map_or(code_to_volts(code), |dark| code_to_volts(code) - dark),
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

    /// Samples the published level averages over: a fixed duration, deliberately
    /// **not** [`Self::avg_window_samples`].
    ///
    /// The chart's averaging is an operator preference; this window is a
    /// measurement. Tying the two together made a display knob set the precision
    /// of the Pockels calibration: at the bench's 500 kSa/s the default of four
    /// samples published 8 µs of signal per settled `CONST` code, so every sweep
    /// point carried ~12 mV of scatter against a 51 mV lobe and the fit reported
    /// a perfect curve as 22 % residual (ADR 019).
    ///
    /// [`LEVEL_WINDOW_SECONDS`] is a duration rather than a sample count because
    /// what averages noise down is time × bandwidth, not samples — and 20 ms in
    /// particular is one full mains period, so a boxcar of that length has a null
    /// at 50 Hz and every harmonic of it.
    fn level_window_samples(&self, rate_hz: u32) -> usize {
        if rate_hz == 0 {
            return self.avg_window_samples(rate_hz);
        }
        ((f64::from(rate_hz) * LEVEL_WINDOW_SECONDS).round() as usize).max(1)
    }

    /// Settled detector level over the measurement window, published on the
    /// contract in **raw** detector volts — never `display_volts`, so a consumer
    /// does not have to know the display mode, and never the optical geometry
    /// transform, which needs an anchor this reading must not depend on.
    ///
    /// Deliberately fail-open where [`Self::optical_summary_result`] is
    /// fail-closed:
    /// a transfer-curve sweep needs a level exactly at the excitation null,
    /// where the reject-port detector is brightest and may rail. Clipping is
    /// reported rather than refused.
    fn current_level(&self, state: &SharedState) -> Option<PhotodiodeLevelV1> {
        if state.samples.is_empty() {
            return None;
        }
        let window = self
            .level_window_samples(state.rate_hz)
            .min(state.samples.len());
        let start = state.samples.len() - window;
        let summary = state.range_summary(start, state.samples.len());
        if summary.count == 0 {
            return None;
        }
        let full_scale = ADC_MAX_CODE as u16;
        // Span-relative, like the contrast estimator: this detector runs a few
        // codes above zero, and a fixed margin calls every one of its windows
        // truncated.
        let margin = near_rail_margin(summary.max.saturating_sub(summary.min));
        Some(PhotodiodeLevelV1 {
            mean_volts: code_to_volts(summary.mean()),
            // `code_to_volts` is a pure scale, so it maps a code difference to
            // a voltage difference directly.
            peak_to_peak_volts: code_to_volts(f64::from(summary.max - summary.min)),
            sample_count: summary.count as u64,
            end_sample_index: state.ring_first_index + state.samples.len() as u64,
            clipped: summary.min <= margin || summary.max >= full_scale.saturating_sub(margin),
        })
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
        let anchor = (self.placement == PhotodiodePlacementV1::RejectedPort)
            .then(|| self.total_power_volts(&state))
            .flatten();

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
                y: self.display_volts(bucket.mean(), anchor),
            });
            if decimating {
                // EXCITATION inverts the axis, so min/max swap roles.
                let (low, high) = (
                    self.display_volts(f64::from(bucket.min), anchor),
                    self.display_volts(f64::from(bucket.max), anchor),
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
                    y: self.display_volts(window.mean(), anchor),
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
        // Opt-in phase-0 trigger overlay: one toggleable line drawing a vertical
        // spike at each marker (up then back to a flat baseline between markers).
        if self.show_markers && !state.markers.is_empty() {
            let first_visible = state.ring_first_index + start as u64;
            let y_range = lines
                .iter()
                .flat_map(|line| line.points.iter())
                .map(|point| point.y)
                .fold(None::<(f64, f64)>, |acc, y| {
                    Some(acc.map_or((y, y), |(lo, hi)| (lo.min(y), hi.max(y))))
                });
            if let Some((y_lo, y_hi)) = y_range {
                let x_for = |index: u64| -> f64 {
                    let device_t = index as f64 / rate;
                    match self.time_axis {
                        TimeAxis::BeforeNow => device_t - latest_x_index as f64 / rate,
                        TimeAxis::Segment => device_t,
                    }
                };
                let mut points = Vec::with_capacity(state.markers.len() * 3);
                for &index in &state.markers {
                    if index < first_visible || index > latest_x_index {
                        continue;
                    }
                    let x = x_for(index);
                    points.push(Series1dPoint { x, y: y_lo });
                    points.push(Series1dPoint { x, y: y_hi });
                    points.push(Series1dPoint { x, y: y_lo });
                }
                if !points.is_empty() {
                    lines.push(Series1dLine {
                        name: "phase-0 trigger".into(),
                        points,
                    });
                }
            }
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
                // Seed the *next* bucket with its own first bin. Seeding with
                // `freq` (the bin that just closed this bucket) put a flat
                // bucket's point one bucket to the left.
                peak_freq = (k + 1) as f64 * rate / n as f64;
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
        let (latest, rate_hz, average, integrity, stream_error, anchor) = match self.shared.lock() {
            Ok(state) => (
                state.latest,
                state.rate_hz,
                self.current_average_code(&state),
                format!(
                    "drops={} crc={} resync={} segments={}",
                    state.device_dropped, state.crc_failures, state.resync_bytes, state.segments
                ),
                state.error.clone(),
                (self.placement == PhotodiodePlacementV1::RejectedPort)
                    .then(|| self.total_power_volts(&state))
                    .flatten(),
            ),
            Err(_) => (None, 0, None, String::new(), None, None),
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
                format!("{:.4} V", self.display_volts(f64::from(sample), anchor)),
            ),
            None => ("—".into(), "—".into()),
        };
        let avg_text = match average {
            Some(code) => {
                let window = self.avg_window_samples(rate_hz);
                format!(
                    "{:.4} V ({} spl ≈ {:.2} ms)",
                    self.display_volts(code, anchor),
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

fn write_json_new(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|err| format!("serializing sidecar failed: {err}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| format!("creating {} failed: {err}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .map_err(|err| format!("writing {} failed: {err}", path.display()))
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn lease_deadline(ttl_ms: u64) -> u64 {
    now_unix_ms().saturating_add(ttl_ms.clamp(MIN_LEASE_TTL_MS, MAX_LEASE_TTL_MS))
}

fn service_error(
    code: ServiceErrorCodeV1,
    message: impl Into<String>,
    retryable: bool,
) -> ServiceErrorV1 {
    ServiceErrorV1 {
        code,
        message: message.into(),
        retryable,
    }
}

fn contract_integrity(integrity: StreamIntegrity, segments: u64) -> StreamIntegrityV1 {
    StreamIntegrityV1 {
        skipped_bytes: integrity.skipped_bytes,
        crc_failures: integrity.crc_failures,
        sequence_gaps: integrity.sequence_gaps,
        dropped_samples: integrity.dropped_samples,
        segment_restarts: segments.saturating_sub(1),
        truncated_bytes: 0,
    }
}

fn accepted_service_reply(
    request: &PluginServiceRequest,
    response: &PhotodiodeResponseV1,
) -> PluginServiceReply {
    PluginServiceReply {
        request_id: request.request_id,
        source_plugin_id: request.source_plugin_id.clone(),
        target_plugin_id: request.target_plugin_id.clone(),
        service: request.service.clone(),
        outcome: PluginServiceOutcome::Accepted {
            payload: serde_json::to_value(response).unwrap_or(Value::Null),
        },
    }
}

fn rejected_service_reply(
    request: &PluginServiceRequest,
    code: impl Into<String>,
    message: impl Into<String>,
) -> PluginServiceReply {
    PluginServiceReply {
        request_id: request.request_id,
        source_plugin_id: request.source_plugin_id.clone(),
        target_plugin_id: request.target_plugin_id.clone(),
        service: request.service.clone(),
        outcome: PluginServiceOutcome::Rejected {
            code: code.into(),
            message: message.into(),
        },
    }
}

fn serial_ports() -> Vec<String> {
    stage_a_io::transport::candidate_ports()
        .into_iter()
        .map(|port| port.name)
        .collect()
}

/// Finds the Teensy stream port: the dual-serial firmware free-runs PDA1
/// `SamplesU16` frames on exactly one of the enumerated ports, so listen
/// briefly on each.
fn resolve_auto_port() -> Result<String, String> {
    let candidates = serial_ports();
    if candidates.is_empty() {
        return Err(stage_a_io::transport::no_candidate_ports_message());
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
        // Windows opens with DTR deasserted; see ADR 032.
        .dtr_on_open(true)
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
    variants.extend(
        stage_a_io::transport::candidate_ports()
            .iter()
            .map(stage_a_io::transport::PortInfo::variant),
    );
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
        "Live photodiode readout with explicit rejected-port, camera-path or emission-path optical geometry and coordinated PDQ recording."
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
            let termination = if self.lease.is_some() {
                PdqTerminationV1::Aborted
            } else {
                PdqTerminationV1::OperatorStopped
            };
            if let Err(err) = self.finalize_recording(termination) {
                self.last_error = Some(err);
            }
            self.disconnect();
            self.lease = None;
        }
    }

    fn set_runtime_role(&mut self, role: PluginRuntimeRole) {
        self.runtime_role = role;
        if role != PluginRuntimeRole::LiveWorker {
            if let Err(error) = self.finalize_recording(PdqTerminationV1::Aborted) {
                self.last_error = Some(error);
            }
            // Demoting to the UI mirror drops the hardware, not the operator's
            // connect intent — see `apply_execution_context`.
            self.disconnect();
            self.lease = None;
            self.effects_allowed = false;
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
        if context.execution().mode == augur_plugin_api::ExecutionMode::Replay {
            if let Err(error) = self.finalize_recording(PdqTerminationV1::Aborted) {
                self.last_error = Some(error);
            }
            // Runs every replayed frame, so it must not clear the intent
            // either — the port stays closed because `connect()` is guarded.
            self.disconnect();
            self.lease = None;
        }
    }

    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        let execution = context.execution();
        self.apply_execution_context(&execution);
    }

    fn handle_service_request(
        &mut self,
        request: &PluginServiceRequest,
        execution: &augur_plugin_api::ExecutionContext,
    ) -> PluginServiceReply {
        if let Some((previous, reply)) = self.request_cache.iter().find(|(previous, _)| {
            previous.source_plugin_id == request.source_plugin_id
                && previous.request_id == request.request_id
        }) {
            return if previous == request {
                reply.clone()
            } else {
                rejected_service_reply(
                    request,
                    "request_id_conflict",
                    "request ID was reused for a different photodiode payload",
                )
            };
        }

        let reply = if request.target_plugin_id != PLUGIN_ID_STAGE_A_PHOTODIODE {
            rejected_service_reply(request, "wrong_target", "wrong photodiode owner target")
        } else if request.service != SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1 {
            rejected_service_reply(
                request,
                "unsupported_service",
                format!("unsupported photodiode service '{}'", request.service),
            )
        } else if self.runtime_role != PluginRuntimeRole::LiveWorker
            || !execution.hardware_effects_allowed()
        {
            rejected_service_reply(
                request,
                "effects_not_allowed",
                "photodiode effects are allowed only on the active live worker",
            )
        } else {
            self.effects_allowed = true;
            match serde_json::from_value::<PhotodiodeRequestV1>(request.payload.clone()) {
                Err(error) => rejected_service_reply(
                    request,
                    "invalid_payload",
                    format!("invalid photodiode request: {error}"),
                ),
                Ok(payload)
                    if payload.contract_version != CONTRACT_VERSION_V1
                        || payload.request_id.0 != request.request_id
                        || payload.requester.as_str() != request.source_plugin_id
                        || payload
                            .target_owner_instance
                            .as_ref()
                            .is_some_and(|owner| owner != &self.owner_instance) =>
                {
                    rejected_service_reply(
                        request,
                        "identity_mismatch",
                        "contract version, request, requester, or owner instance mismatch",
                    )
                }
                Ok(payload)
                    if payload.issued_at_unix_ms != 0
                        && (now_unix_ms().saturating_sub(payload.issued_at_unix_ms) > 120_000
                            || payload.issued_at_unix_ms.saturating_sub(now_unix_ms())
                                > 30_000) =>
                {
                    rejected_service_reply(request, "stale_request", "request timestamp is stale")
                }
                Ok(payload) => match self.handle_photodiode_command(&payload) {
                    Ok(response) => accepted_service_reply(request, &response),
                    Err(error) => rejected_service_reply(
                        request,
                        format!("{:?}", error.code).to_ascii_lowercase(),
                        error.message,
                    ),
                },
            }
        };
        self.request_cache
            .push_back((request.clone(), reply.clone()));
        while self.request_cache.len() > REQUEST_CACHE_LIMIT {
            self.request_cache.pop_front();
        }
        reply
    }

    fn control_snapshots(&self) -> Vec<PluginControlSnapshot> {
        vec![PluginControlSnapshot {
            plugin_id: PLUGIN_ID_STAGE_A_PHOTODIODE.into(),
            topic: CTX_STAGE_A_PHOTODIODE_SUMMARY_V1.into(),
            revision: self.generation.load(Ordering::Relaxed).max(1),
            payload: serde_json::to_value(self.control_summary()).unwrap_or(Value::Null),
        }]
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
        let placement_variants: Vec<String> = PHOTODIODE_PLACEMENTS
            .iter()
            .map(|placement| placement_name(*placement).to_owned())
            .collect();
        let placement_default = PHOTODIODE_PLACEMENTS
            .iter()
            .position(|placement| *placement == self.placement)
            .unwrap_or(0);
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Photodiode readout".into(),
                    description: Some(
                        "Reads the photodiode on the Teensy's SECOND serial port. This is what \
                         measures the modulation depth the A1 plugin records against.\n\n\
                         Set the physical detector placement before recording. The PBS rejected \
                         port uses the complementary-light I_tot model. Camera and emission paths \
                         measure the local beam directly, use the lamp-off dark reference, and \
                         never use I_tot."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "port".into(),
                            label: "Port".into(),
                            tooltip: Some(
                                "auto (recommended) finds the Teensy port that is sending \
                                 photodiode samples. mock produces fake data for testing without \
                                 hardware."
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
                            key: "placement".into(),
                            label: "Detector placement".into(),
                            tooltip: Some(
                                "PBS rejected port: complementary excitation, needs the learned \
                                 I_tot anchor. Camera path: direct beam towards the camera. \
                                 Emission path: direct fluorescence after the emission filter."
                                    .into(),
                            ),
                            kind: SettingKind::Enum {
                                variants: placement_variants,
                                default: placement_default,
                            },
                        },
                        SettingItem {
                            key: "splitter_fraction".into(),
                            label: "Fraction sent to PD".into(),
                            tooltip: Some(
                                "0.5 for a 50:50 splitter. Stored as optical provenance; it is \
                                 not used to rescale log contrast. Ignored for the PBS rejected port."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.001,
                                max: 1.0,
                                speed: 0.01,
                                default: self.splitter_fraction,
                            },
                        },
                        SettingItem {
                            key: "direct_dark_volts".into(),
                            label: "Lamp-off dark (V)".into(),
                            tooltip: Some(
                                "Session-local blocked-light detector reading for camera/emission \
                                 path contrast. This is only a draft until Use manual dark is \
                                 pressed. Ignored for the PBS rejected port."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: ADC_FULL_SCALE_VOLTS,
                                speed: 0.0001,
                                default: self.direct_dark_manual_volts,
                            },
                        },
                        SettingItem {
                            key: "use_manual_direct_dark".into(),
                            label: "Use manual dark".into(),
                            tooltip: Some(
                                "Explicitly activates the typed value and records its source as \
                                 manual. Prefer Capture lamp-off dark when the detector is available."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: self.placement != PhotodiodePlacementV1::RejectedPort,
                            },
                        },
                        SettingItem {
                            key: "capture_direct_dark".into(),
                            label: "Capture lamp-off dark".into(),
                            tooltip: Some(
                                "With the beam physically blocked, freezes the current settled \
                                 raw detector level as the session dark reference."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: self.placement != PhotodiodePlacementV1::RejectedPort,
                            },
                        },
                        SettingItem {
                            key: "mode".into(),
                            label: "Mode".into(),
                            tooltip: Some(
                                "RAW shows what the detector reads. EXCITATION shows the \
                                 excitation beam instead (total power minus the detector \
                                 reading) — use this one to measure the modulation depth."
                                    .into(),
                            ),
                            kind: SettingKind::Enum {
                                variants: mode_variants,
                                default: mode_default,
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
                        SettingItem {
                            key: "show_markers".into(),
                            label: "Show phase-0 trigger markers".into(),
                            tooltip: Some(
                                "Overlay the firmware phase-0 markers (device-clock MARKER frames) \
                                 as a toggleable vertical curve. Also defines the modulation \
                                 frequency from the marker spacing."
                                    .into(),
                            ),
                            kind: SettingKind::Bool {
                                default: self.show_markers,
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Data".into(),
                    description: Some(
                        "Recording the photodiode on its own. The A1 plugin drives its own \
                         recordings and does not need anything here.\n\n\
                         The last few seconds are always kept in memory for the chart; recording \
                         writes everything to disk instead, so it can run as long as you have \
                         space. Files store the raw readings, with the mode and total power in a \
                         companion file."
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
                                "How many seconds of samples to keep in memory. This is also the \
                                 stretch the modulation depth is measured over, so it must cover \
                                 at least one full cycle of your slowest frequency — raise it if \
                                 A1 says the window is too short."
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
                            key: "record_start".into(),
                            label: "Start recording".into(),
                            tooltip: Some(
                                "Start appending every incoming sample frame to \
                             pd_rec_<timestamp>.pdq. Disabled until a data directory \
                             is selected."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: !self.data_dir.trim().is_empty(),
                            },
                        },
                        SettingItem {
                            key: "record_stop".into(),
                            label: "Stop recording".into(),
                            tooltip: Some(
                                "Stop the disk recording and write the JSON sidecar.".into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "save_snapshot".into(),
                            label: "Save cache snapshot".into(),
                            tooltip: Some(
                                "Write the current cache once as pd_cache_<timestamp>.csv \
                             (+ JSON sidecar). Disabled until a data directory is selected."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: !self.data_dir.trim().is_empty(),
                            },
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
            "placement" => Some(json!(PHOTODIODE_PLACEMENTS
                .iter()
                .position(|placement| *placement == self.placement)
                .unwrap_or(0))),
            "splitter_fraction" => Some(json!(self.splitter_fraction)),
            "direct_dark_volts" => Some(json!(self.direct_dark_manual_volts)),
            "use_manual_direct_dark" => Some(self.press_use_manual_direct_dark.value()),
            "capture_direct_dark" => Some(self.press_capture_direct_dark.value()),
            "mode" => {
                let index = Mode::VARIANTS
                    .iter()
                    .position(|m| *m == self.mode)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "window_s" => Some(json!(self.window_s)),
            "avg_samples" => Some(json!(self.avg_samples)),
            "show_markers" => Some(json!(self.show_markers)),
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
            // Kept for compatibility (tests, external tooling); not in the
            // schema anymore, so it is never synced across instances.
            "record" => Some(json!(self.recording_active())),
            // Button presses are exported as monotonic counters so the host's
            // settings snapshot transports them from the UI mirror to the
            // live worker (see PressLatch).
            "record_start" => Some(self.press_record_start.value()),
            "record_stop" => Some(self.press_record_stop.value()),
            "save_snapshot" => Some(self.press_save_snapshot.value()),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        if self.lease.is_some() {
            return Err(format!(
                "manual setting '{key}' is locked while the photodiode owner is leased"
            ));
        }
        match key {
            "port" => {
                self.port_hint = variant_path(&enum_choice(&value, &port_variants())?).to_owned();
                Ok(())
            }
            "placement" => {
                let names: Vec<String> = PHOTODIODE_PLACEMENTS
                    .iter()
                    .map(|placement| placement_name(*placement).to_owned())
                    .collect();
                let name = enum_choice(&value, &names)?;
                let placement = placement_from_name(&name)
                    .ok_or_else(|| format!("unknown photodiode placement: {name}"))?;
                if placement != self.placement {
                    self.placement = placement;
                    self.direct_dark_reference = None;
                }
                Ok(())
            }
            "splitter_fraction" => {
                let fraction = value.as_f64().ok_or("splitter_fraction must be a number")?;
                if !(fraction.is_finite() && 0.0 < fraction && fraction <= 1.0) {
                    return Err("splitter_fraction must be in (0, 1]".into());
                }
                self.splitter_fraction = fraction;
                Ok(())
            }
            "direct_dark_volts" => {
                let volts = value.as_f64().ok_or("direct_dark_volts must be a number")?;
                if !volts.is_finite() || !(0.0..ADC_FULL_SCALE_VOLTS).contains(&volts) {
                    return Err(format!(
                        "direct_dark_volts must be in [0, {ADC_FULL_SCALE_VOLTS})"
                    ));
                }
                self.direct_dark_manual_volts = volts;
                Ok(())
            }
            "use_manual_direct_dark" => {
                if self.press_use_manual_direct_dark.accept(&value) {
                    if self.placement == PhotodiodePlacementV1::RejectedPort {
                        return Err("lamp-off dark is not used for the PBS rejected port".into());
                    }
                    let captured_at_unix_ms = now_unix_ms();
                    self.direct_dark_reference = Some(StoredDirectDarkReference {
                        dark_id: format!("manual@{captured_at_unix_ms}"),
                        source: PhotodiodeDarkSourceV1::Manual,
                        dark_volts: self.direct_dark_manual_volts,
                        captured_at_unix_ms,
                    });
                    self.last_save_note = Some(format!(
                        "using manually entered dark: {:.6} V",
                        self.direct_dark_manual_volts
                    ));
                }
                Ok(())
            }
            "capture_direct_dark" => {
                if self.press_capture_direct_dark.accept(&value) {
                    if self.placement == PhotodiodePlacementV1::RejectedPort {
                        return Err("lamp-off dark is not used for the PBS rejected port".into());
                    }
                    let level = self
                        .shared
                        .lock()
                        .ok()
                        .and_then(|state| self.current_level(&state))
                        .ok_or("no settled photodiode level is available")?;
                    let volts = level.mean_volts;
                    let captured_at_unix_ms = now_unix_ms();
                    self.direct_dark_reference = Some(StoredDirectDarkReference {
                        dark_id: format!(
                            "lamp-off@sample-{}@{captured_at_unix_ms}",
                            level.end_sample_index
                        ),
                        source: PhotodiodeDarkSourceV1::MeasuredLampOff,
                        dark_volts: volts,
                        captured_at_unix_ms,
                    });
                    self.last_save_note = Some(format!("captured lamp-off dark: {volts:.6} V"));
                }
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
            "show_markers" => {
                self.show_markers = value.as_bool().ok_or("show_markers must be a boolean")?;
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
                // Compatibility alias (not in the schema): direct boolean
                // start/stop with the same edge-free semantics as before.
                let requested = value.as_bool().ok_or("record must be a boolean")?;
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
            "record_start" => {
                // Failures surface through status entries (like `connect`),
                // so a missing data directory doesn't read as a broken UI.
                if self.press_record_start.accept(&value) {
                    match self.start_recording() {
                        Ok(()) => self.last_error = None,
                        Err(err) => self.last_error = Some(err),
                    }
                    self.generation.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            "record_stop" => {
                if self.press_record_stop.accept(&value) {
                    match self.stop_recording() {
                        Ok(()) => self.last_error = None,
                        Err(err) => self.last_error = Some(err),
                    }
                    self.generation.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            // Accepted and ignored so a config saved before ADR 024 still
            // loads. The anchor is learned from the stream and dark cancels out
            // of the complement, so there is nothing left for these to set.
            "reference_volts"
            | "reference_anchor_id"
            | "reference_confirmed"
            | "dark_volts"
            | "capture_dark" => Ok(()),
            "save_snapshot" => {
                // Edge-guarded: the host re-applies the full settings snapshot
                // on every sync, and an unguarded arm wrote one cache file per
                // sync of *any* plugin's settings.
                if self.press_save_snapshot.accept(&value) {
                    match self.save_cache_snapshot() {
                        Ok(()) => self.last_error = None,
                        Err(err) => self.last_error = Some(err),
                    }
                    self.generation.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        let (latest, rate_hz, average, stream_error, anchor) = match self.shared.lock() {
            Ok(state) => (
                state.latest,
                state.rate_hz,
                self.current_average_code(&state),
                state.error.clone(),
                (self.placement == PhotodiodePlacementV1::RejectedPort)
                    .then(|| self.total_power_volts(&state))
                    .flatten(),
            ),
            Err(_) => (None, 0, None, None, None),
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
        entries.push(StatusEntry::Text(match self.placement {
            PhotodiodePlacementV1::RejectedPort => {
                "Optics: PBS rejected port (I_tot complement)".into()
            }
            placement => match self.published_direct_dark() {
                Some(dark) => format!(
                    "Optics: {} (PD fraction {:.1} %, dark {:.4} V, {:?}, age {:.1} s)",
                    placement_name(placement),
                    100.0 * self.splitter_fraction,
                    dark.dark_volts,
                    dark.source,
                    dark.age_s
                ),
                None => format!(
                    "Optics: {} (PD fraction {:.1} %, lamp-off dark REQUIRED)",
                    placement_name(placement),
                    100.0 * self.splitter_fraction
                ),
            },
        }));
        if let Some(sample) = latest {
            match self.mode {
                Mode::Raw => entries.push(StatusEntry::Text(format!(
                    "PD: code={sample} ({:.4} V)",
                    code_to_volts(f64::from(sample))
                ))),
                Mode::Excitation => entries.push(StatusEntry::Text(format!(
                    "Excitation: {:.4} V (I_tot={}, PD={:.4} V)",
                    self.display_volts(f64::from(sample), anchor),
                    match anchor {
                        Some(volts) => format!("{volts:.4} V"),
                        None => "learning…".into(),
                    },
                    code_to_volts(f64::from(sample))
                ))),
            }
        }
        if let Some(average) = average {
            let window = self.avg_window_samples(rate_hz);
            if window > 1 {
                entries.push(StatusEntry::Text(format!(
                    "Avg ({window} spl): {:.4} V",
                    self.display_volts(average, anchor)
                )));
            }
        }
        match self.latest_optical_result() {
            // Always the excitation contrast: the geometry follows the bench,
            // not the display mode.
            Some(Ok(optical)) => {
                entries.push(StatusEntry::Text(format!(
                    "a (local optical signal) = {:.3}  (I {:.4}..{:.4} V)",
                    optical.measured_log_contrast,
                    optical.excitation_min_volts,
                    optical.excitation_max_volts
                )));
            }
            // A withheld `a` is a fail-closed refusal, not an absence of data:
            // say which gate rejected the window so the operator can fix it.
            Some(Err(error)) => {
                entries.push(StatusEntry::Text(format!("a unavailable: {error}")));
            }
            None => {}
        }
        if let Ok(state) = self.shared.lock() {
            if let Some(period_samples) = state.marker_period_samples() {
                let hz = f64::from(state.rate_hz.max(1)) / period_samples;
                entries.push(StatusEntry::Text(format!(
                    "Trigger: {} markers, f = {hz:.3} Hz",
                    state.markers.len()
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
    use augur_plugin_api::{ExecutionContext, ExecutionMode};
    use stage_a_io::{Frame, FrameHeader, FrameType};

    fn live_execution() -> ExecutionContext {
        ExecutionContext {
            mode: ExecutionMode::LiveCapture,
            effects_allowed: true,
            session_id: Some("test".into()),
        }
    }

    fn live_plugin() -> StageAPhotodiodePlugin {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_runtime_role(PluginRuntimeRole::LiveWorker);
        plugin.effects_allowed = true;
        plugin
    }

    /// Pins the learned total-power anchor to a known `I_tot`, standing in for
    /// the excitation null the Pockels sweep drives the detector through.
    fn anchor_at(state: &mut SharedState, volts: f64) {
        state.observed_peak_code = Some(volts * ADC_MAX_CODE / ADC_FULL_SCALE_VOLTS);
    }

    /// A clean rejected-port sine: the detector swings around `center` while
    /// the excitation is its complement against `I_tot`.
    fn rejected_port_samples(center: f64, amplitude: f64, count: usize) -> Vec<u16> {
        (0..count)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * (i as f64) * 8.0 / count as f64;
                (center + amplitude * phase.sin())
                    .round()
                    .clamp(0.0, 4_095.0) as u16
            })
            .collect()
    }

    /// [`rejected_port_samples`] ingested into a ring, with one phase-0 marker
    /// per cycle when `mark_cycles` — the estimator sizes its window from them.
    fn rejected_port_state(
        center: f64,
        amplitude: f64,
        count: usize,
        mark_cycles: bool,
    ) -> SharedState {
        let mut state = SharedState::default();
        state.ingest(
            0,
            20_000,
            0,
            &rejected_port_samples(center, amplitude, count),
        );
        if mark_cycles {
            // `rejected_port_samples` puts 8 whole cycles in `count` samples.
            let period = (count / 8) as u64;
            for cycle in 0..=8 {
                state.push_marker(cycle * period);
            }
        }
        state
    }

    /// A slow sine streamed for `total` samples into a ring that only retains
    /// `retained` of them, with one phase-0 marker per cycle delivered as the
    /// stream goes past — so markers are evicted exactly as they are on the
    /// bench when the period outgrows the monitor cache.
    fn slow_sine_state(period_samples: u64, retained: usize, total: usize) -> SharedState {
        let mut state = SharedState {
            cache_seconds: retained as f64 / 20_000.0,
            ..Default::default()
        };
        let block = 4_000;
        let mut index = 0usize;
        while index < total {
            let end = (index + block).min(total);
            let codes: Vec<u16> = (index..end)
                .map(|i| {
                    let phase = 2.0 * std::f64::consts::PI * (i as f64) / period_samples as f64;
                    (1_600.0 + 700.0 * phase.sin()).round() as u16
                })
                .collect();
            state.ingest(index as u64, 20_000, 0, &codes);
            let mut marker = index.next_multiple_of(period_samples as usize) as u64;
            while (marker as usize) <= end {
                state.push_marker(marker);
                marker += period_samples;
            }
            index = end;
        }
        state
    }

    /// The bug an A1 survey paid for a recording at a time: a cache length left
    /// at its default is shorter than one cycle of a sub-hertz rung, so the
    /// estimator saw no whole cycle, `a` was withheld, and the sidecar was
    /// refused *after* the recording had already run its full duration. The ring
    /// grows to the drive now, so the same stream publishes an `a`.
    #[test]
    fn a_cache_shorter_than_the_drive_no_longer_starves_the_estimator() {
        let plugin = live_plugin();
        // 0.5 Hz at 20 kSa/s = 40 000 samples per cycle, against a cache set to
        // hold 0.6 of one.
        let mut state = slow_sine_state(40_000, 24_000, 200_000);
        anchor_at(&mut state, 3.0);
        let summary = plugin
            .optical_summary_result(&state)
            .expect("the ring sizes itself to the marker period");
        assert!(
            summary.covered_cycles.expect("cycles") >= 2.0,
            "window covers {:?} cycles",
            summary.covered_cycles
        );
    }

    #[test]
    fn a_window_shorter_than_one_cycle_withholds_a_instead_of_under_reporting_it() {
        // `a` is peak-to-peak. Below one full cycle the robust extrema see an
        // arc of the sine, so `a` comes out low — and A1's a₀ lock divides by
        // it, inflating its drive against a bias it cannot see. Fail closed.
        //
        // A cache too short for the drive is no longer the way to get here —
        // the ring follows the period. What is left is a drive whose cycles have
        // not gone by yet: the first phase-0 stamp after a retarget or a segment
        // restart bounds no whole cycle at all.
        let plugin = live_plugin();

        // 0.5 Hz at 20 kSa/s = 40 000 samples per cycle, one marker seen.
        let mut partial = slow_sine_state(40_000, 24_000, 200_000);
        anchor_at(&mut partial, 3.0);
        partial.markers.drain(1..);
        let error = plugin
            .optical_summary_result(&partial)
            .expect_err("a partial cycle must not publish an a");
        assert!(
            matches!(error, EstimateError::IncompleteModulationCycles { .. }),
            "unexpected rejection: {error:?}"
        );

        // Two whole cycles of the same drive: published, and the window is
        // reported so a consumer can wait it out before trusting a re-read.
        let mut whole = slow_sine_state(40_000, 80_000, 200_000);
        anchor_at(&mut whole, 3.0);
        let summary = plugin
            .optical_summary(&whole)
            .expect("two whole cycles estimate");
        let expected = ((3.0_f64 - (1_600.0 - 700.0) * (3.3 / 4_095.0))
            / (3.0 - (1_600.0 + 700.0) * (3.3 / 4_095.0)))
            .ln();
        assert!(
            (summary.measured_log_contrast - expected).abs() < 0.02,
            "a={} expected~{expected}",
            summary.measured_log_contrast
        );
        assert!((summary.measured_frequency_hz.expect("markers") - 0.5).abs() < 0.01);
        // The window is whole cycles, and says how many, so a consumer can wait
        // it out before trusting a re-read.
        let cycles = summary.covered_cycles.expect("cycles");
        assert!(cycles >= 2.0, "covered only {cycles} cycles");
        assert!(
            (summary.window_seconds.expect("window") - cycles * 2.0).abs() < 0.01,
            "window {:?} is not {cycles} cycles at 0.5 Hz",
            summary.window_seconds
        );
    }

    #[test]
    fn the_contrast_window_grows_to_cover_whole_cycles_at_low_frequency() {
        // A fixed 16 384-sample window is 0.82 s: below one cycle for every
        // f < 1.2 Hz, which is where the A1 plateau reference lives.
        let fast = rejected_port_state(1_600.0, 700.0, 4_096, true);
        let (window, cycles) = fast.contrast_window();
        assert_eq!(window, 4_096, "high f keeps the whole retained ring");
        assert!(cycles.expect("markers") >= 8.0);

        let slow = slow_sine_state(40_000, 400_000, 400_000);
        let (window, cycles) = slow.contrast_window();
        assert_eq!(
            window,
            (CONTRAST_WINDOW_CYCLES as usize) * 40_000,
            "the window is sized from the marker period, not fixed"
        );
        assert!((cycles.expect("markers") - CONTRAST_WINDOW_CYCLES).abs() < 0.01);

        // Without markers there is no period to size against: fall back to the
        // fixed window and report no cycle count rather than guess one.
        let mut unmarked = slow_sine_state(40_000, 400_000, 400_000);
        unmarked.markers.clear();
        unmarked.marker_period_estimate = None;
        assert_eq!(unmarked.contrast_window(), (CONTRAST_WINDOW_SAMPLES, None));
    }

    #[test]
    fn published_contrast_is_the_excitation_contrast_in_both_display_modes() {
        // The detector sits behind the PBS reject port whatever the operator
        // is plotting, so a display toggle must not move a published
        // scientific quantity. A1's amplitude sweep settles on this value.
        let mut plugin = live_plugin();
        let mut state = rejected_port_state(1_600.0, 700.0, 4_096, true);
        anchor_at(&mut state, 3.0);

        plugin.mode = Mode::Raw;
        let raw = plugin.optical_summary(&state).expect("raw display");
        plugin.mode = Mode::Excitation;
        let excitation = plugin.optical_summary(&state).expect("excitation display");

        assert_eq!(raw.measured_log_contrast, excitation.measured_log_contrast);
        assert!(raw
            .calibration
            .anchor_id
            .as_deref()
            .is_some_and(|anchor| anchor.starts_with("observed-peak@")));
        assert_eq!(raw.calibration.anchor_id, excitation.calibration.anchor_id);
        assert!((raw.calibration.total_power_volts.expect("I_tot") - 3.0).abs() < 1e-12);
        assert!((raw.measured_frequency_hz.expect("marker frequency") - 39.0625).abs() < 1e-12);

        // And it really is the complement contrast, not ln(v_max/v_min) of the
        // detector trace.
        let detector_direct = ((1_600.0_f64 + 700.0) / (1_600.0 - 700.0)).ln();
        assert!(
            (raw.measured_log_contrast - detector_direct).abs() > 0.1,
            "published a={} collapsed to the detector-direct contrast",
            raw.measured_log_contrast
        );
    }

    #[test]
    fn emission_path_measures_direct_contrast_without_i_tot() {
        let mut plugin = StageAPhotodiodePlugin {
            placement: PhotodiodePlacementV1::EmissionPath,
            splitter_fraction: 0.5,
            direct_dark_reference: Some(StoredDirectDarkReference {
                dark_id: "lamp-off@test".into(),
                source: PhotodiodeDarkSourceV1::MeasuredLampOff,
                dark_volts: 0.0,
                captured_at_unix_ms: now_unix_ms(),
            }),
            ..StageAPhotodiodePlugin::default()
        };
        let state = rejected_port_state(1_600.0, 700.0, 4_096, true);
        let optical = plugin
            .optical_summary_result(&state)
            .expect("direct fluorescence contrast");

        assert_eq!(optical.placement, PhotodiodePlacementV1::EmissionPath);
        assert_eq!(optical.splitter_fraction, Some(0.5));
        assert!(optical.measured_log_contrast.is_finite());
        assert!(optical.measured_log_contrast > 0.0);
        assert!(optical.calibration.anchor_id.is_none());
        assert!(optical.calibration.total_power_volts.is_none());
        assert_eq!(
            optical
                .calibration
                .dark_reference
                .as_ref()
                .map(|reference| reference.source),
            Some(PhotodiodeDarkSourceV1::MeasuredLampOff)
        );

        // Even a completely empty rejected-port anchor must not gate a direct
        // path measurement.
        plugin.shared = Arc::new(Mutex::new(state));
        let summary = plugin.control_summary();
        assert_eq!(summary.placement, PhotodiodePlacementV1::EmissionPath);
        assert_eq!(summary.splitter_fraction, Some(0.5));
        assert!(summary.optical_summary.is_some());
    }

    #[test]
    fn direct_path_refuses_contrast_until_dark_is_explicit() {
        let plugin = StageAPhotodiodePlugin {
            placement: PhotodiodePlacementV1::EmissionPath,
            ..StageAPhotodiodePlugin::default()
        };
        let state = rejected_port_state(1_600.0, 700.0, 4_096, true);

        assert_eq!(
            plugin.optical_summary_result(&state),
            Err(EstimateError::MissingDirectDarkReference)
        );
    }

    #[test]
    fn manual_direct_dark_is_marked_manual() {
        let mut plugin = StageAPhotodiodePlugin {
            placement: PhotodiodePlacementV1::EmissionPath,
            ..StageAPhotodiodePlugin::default()
        };
        plugin
            .set_setting("direct_dark_volts", json!(0.012))
            .expect("manual dark draft");
        assert!(plugin.published_direct_dark().is_none());
        plugin
            .set_setting("use_manual_direct_dark", json!(true))
            .expect("activate manual dark");

        let reference = plugin
            .published_direct_dark()
            .expect("manual reference published");
        assert_eq!(reference.source, PhotodiodeDarkSourceV1::Manual);
        assert!(reference.dark_id.starts_with("manual@"));
        assert_eq!(reference.dark_volts, 0.012);
    }

    #[test]
    fn captured_direct_dark_names_the_measured_sample_window() {
        let mut plugin = StageAPhotodiodePlugin {
            placement: PhotodiodePlacementV1::EmissionPath,
            shared: Arc::new(Mutex::new(rejected_port_state(16.0, 2.0, 4_096, true))),
            ..StageAPhotodiodePlugin::default()
        };
        plugin
            .set_setting("capture_direct_dark", json!(true))
            .expect("capture dark");

        let reference = plugin
            .published_direct_dark()
            .expect("measured reference published");
        assert_eq!(reference.source, PhotodiodeDarkSourceV1::MeasuredLampOff);
        assert!(reference.dark_id.starts_with("lamp-off@sample-4096@"));
        assert!(reference.captured_at_unix_ms > 0);
        assert!(reference.age_s >= 0.0);
    }

    #[test]
    fn optical_contrast_requires_a_learned_anchor_and_complete_cycles() {
        let plugin = live_plugin();
        let mut state = rejected_port_state(1_600.0, 700.0, 4_096, true);

        state.observed_peak_code = None;
        assert_eq!(
            plugin.optical_summary_result(&state),
            Err(EstimateError::MissingTotalPowerAnchor)
        );

        anchor_at(&mut state, 3.0);
        state.markers = VecDeque::from([0, 512]);
        assert!(matches!(
            plugin.optical_summary_result(&state),
            Err(EstimateError::IncompleteModulationCycles {
                marker_count: 2,
                max_samples: 4_096,
            })
        ));
    }

    #[test]
    fn a_withheld_contrast_publishes_its_reason_on_the_contract() {
        // A1 gates the a₀ lock and the frequency ladder on `a`. When `a` is
        // refused, the reason has to travel with the absent summary or the only
        // thing the operator can read is that it is missing.
        let plugin = live_plugin();
        {
            let mut state = plugin.shared.lock().expect("state");
            *state = rejected_port_state(1_600.0, 700.0, 4_096, true);
            state.observed_peak_code = None;
        }

        let summary = plugin.control_summary();
        assert!(summary.optical_summary.is_none());
        assert_eq!(
            summary.optical_unavailable.as_deref(),
            Some(EstimateError::MissingTotalPowerAnchor.to_string().as_str())
        );

        {
            let mut state = plugin.shared.lock().expect("state");
            anchor_at(&mut state, 3.0);
        }
        let summary = plugin.control_summary();
        assert!(summary.optical_summary.is_some());
        assert!(
            summary.optical_unavailable.is_none(),
            "an accepted a must not also carry a refusal: {:?}",
            summary.optical_unavailable
        );
    }

    #[test]
    fn the_millivolt_scale_reject_port_detector_still_yields_a() {
        // The bench detector operates around 0.5–15 mV, inside the bottom ~20
        // codes of the 12-bit range. That is not the bottom rail, so `a` must be
        // published rather than refused as clipped.
        let plugin = live_plugin();
        {
            let mut state = plugin.shared.lock().expect("state");
            // Detector swinging between ~0.6 and ~18.6 codes = 0.5..15 mV.
            *state = rejected_port_state(9.6, 9.0, 4_096, true);
            anchor_at(&mut state, 0.015_5);
        }

        let summary = plugin.control_summary();
        assert!(
            summary.optical_unavailable.is_none(),
            "a millivolt-scale window must not be refused: {:?}",
            summary.optical_unavailable
        );
        let optical = summary.optical_summary.expect("a is published");
        assert!(
            optical.measured_log_contrast > 0.0 && optical.measured_log_contrast.is_finite(),
            "a = {}",
            optical.measured_log_contrast
        );
    }

    #[test]
    fn the_anchor_is_learned_from_the_brightest_reading_the_detector_takes() {
        // The excitation null the Pockels sweep drives through is where the
        // reject-port detector reads I_tot. Nothing is entered by hand.
        let plugin = live_plugin();
        {
            let mut state = plugin.shared.lock().expect("state");
            // A sweep peak, then ordinary modulation well below it.
            state.ingest(0, 20_000, 0, &[3_000; SUMMARY_CELL]);
            state.ingest(SUMMARY_CELL as u64, 20_000, 0, &[1_200; SUMMARY_CELL * 4]);
        }

        let anchor = plugin.learned_anchor_volts().expect("anchor learned");
        assert!(
            (anchor - code_to_volts(3_000.0)).abs() < 1e-9,
            "anchor {anchor} did not latch on the sweep peak"
        );
    }

    #[test]
    fn a_single_spike_cannot_become_the_anchor() {
        // The latch runs on completed 64-sample cell means, so one outlier
        // sample cannot pin I_tot high for every later `a`.
        let plugin = live_plugin();
        {
            let mut state = plugin.shared.lock().expect("state");
            let mut codes = vec![1_000u16; SUMMARY_CELL];
            codes[7] = 4_095;
            state.ingest(0, 20_000, 0, &codes);
        }

        let anchor = plugin.learned_anchor_volts().expect("anchor learned");
        assert!(
            anchor < code_to_volts(1_100.0),
            "a single spike pulled the anchor to {anchor}"
        );
    }

    /// The anchor is learned from the detector's own stream, so before any
    /// Pockels sweep has driven the excitation to its null the only thing it
    /// has seen is the modulation itself — and then the "total power" is barely
    /// above the signal. That must refuse, not publish an enormous `a`: the
    /// excitation minimum would be dominated by the anchor's own error rather
    /// than by the light.
    #[test]
    fn a_modulation_only_anchor_refuses_instead_of_reporting_a_huge_contrast() {
        let plugin = live_plugin();
        // 8 cycles in 4096 samples at 20 kSa/s = ~39 Hz, and a slow 1 Hz case.
        for (count, label) in [(4_096usize, "39 Hz"), (160_000, "1 Hz")] {
            let state = rejected_port_state(1_600.0, 700.0, count, true);
            // No `anchor_at`: whatever `ingest` learned from this trace alone.
            match plugin.optical_summary_result(&state) {
                // Named, not just "some error": a refusal for want of cycles
                // would make this test pass without exercising the anchor at
                // all.
                Err(EstimateError::TotalPowerBelowSignal { .. }) => {}
                Err(other) => panic!("{label}: refused for the wrong reason: {other:?}"),
                Ok(summary) => panic!(
                    "{label}: published a = {} from an anchor that never saw the excitation \
                     null (headroom {} V)",
                    summary.measured_log_contrast, summary.excitation_headroom_volts
                ),
            }
        }
    }

    #[test]
    fn a_dc_dark_offset_cancels_out_of_the_complement() {
        // Both sides of `I_exc = I_tot - I_pd` are readings from the same
        // DC-coupled detector, so a dark offset appears in both and cancels
        // exactly. That is why there is no dark setting: there is nothing for
        // it to correct, and correcting only one side is the actual bug.
        let plugin = live_plugin();

        let contrast_with_offset = |offset: f64| {
            let mut state = rejected_port_state(1_600.0 + offset, 700.0, 4_096, true);
            // I_tot is read by the same detector, so it carries the offset too.
            anchor_at(&mut state, 3.0 + code_to_volts(offset));
            plugin
                .optical_summary(&state)
                .expect("estimate")
                .measured_log_contrast
        };

        let baseline = contrast_with_offset(0.0);
        let offset = contrast_with_offset(200.0);
        assert!(
            (baseline - offset).abs() < 1e-9,
            "dark did not cancel: {baseline} vs {offset}"
        );
    }

    #[test]
    fn a_dark_offset_on_only_one_side_would_bias_the_contrast() {
        // Guards the invariance above against a regression that dark-corrects
        // the detector but leaves the anchor raw (or vice versa).
        let calibration = AdcCalibration {
            volts_per_code: ADC_FULL_SCALE_VOLTS / ADC_MAX_CODE,
            offset_volts: 0.0,
            dark_volts: 0.05,
            full_scale_code: ADC_MAX_CODE as u16,
        };
        let samples: Vec<u16> = rejected_port_samples(1_600.0, 700.0, 4_096);
        let consistent = estimate_contrast(
            &samples,
            &calibration,
            ContrastGeometry::RejectedComplement {
                total_power_volts: 3.0 - 0.05,
            },
        )
        .expect("consistent");
        let asymmetric = estimate_contrast(
            &samples,
            &calibration,
            ContrastGeometry::RejectedComplement {
                total_power_volts: 3.0,
            },
        )
        .expect("anchor left raw");
        assert!(
            (consistent.a - asymmetric.a).abs() > 1e-3,
            "the asymmetry must be observable, else this test proves nothing"
        );
    }

    #[test]
    fn the_removed_anchor_settings_still_load_from_an_old_config() {
        // A config written before ADR 024 must not fail to load; the keys are
        // accepted and ignored.
        let mut plugin = live_plugin();
        for (key, value) in [
            ("reference_volts", json!(2.9)),
            ("reference_anchor_id", json!("itot-old")),
            ("reference_confirmed", json!(true)),
            ("dark_volts", json!(0.05)),
        ] {
            plugin
                .set_setting(key, value)
                .unwrap_or_else(|error| panic!("{key} rejected: {error}"));
        }
        assert!(plugin.get_setting("reference_volts").is_none());
    }

    #[test]
    fn the_ui_mirror_keeps_the_operators_connect_intent() {
        // The mirror runs `apply_execution_context` every control tick. If it
        // clears the intent, the host samples `connect` as false and the live
        // worker never opens the port.
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_runtime_role(PluginRuntimeRole::UiMirror);
        plugin.set_setting("connect", json!(true)).expect("connect");
        assert!(plugin.connect_requested);

        plugin.apply_execution_context(&live_execution());
        assert!(
            plugin.connect_requested,
            "the mirror cleared the connect intent"
        );
        assert_eq!(plugin.get_setting("connect"), Some(json!(true)));
        // ...but it must not have actually opened anything.
        assert!(!plugin.connected());
    }

    fn service_request(
        plugin: &StageAPhotodiodePlugin,
        id: u64,
        requester: &str,
        command: PhotodiodeCommandV1,
        revision: Option<u64>,
    ) -> PluginServiceRequest {
        let mut payload = PhotodiodeRequestV1::new(
            stage_a_plugin_contract::RequestId(id),
            ClientId::from(requester),
            command,
        );
        payload.target_owner_instance = Some(plugin.owner_instance.clone());
        payload.run_id = Some(RunId::from("run-a"));
        payload.lease_id = Some(LeaseId::from("lease-a"));
        payload.requested_revision = revision.map(SemanticRevision);
        payload.issued_at_unix_ms = now_unix_ms();
        PluginServiceRequest {
            request_id: id,
            source_plugin_id: requester.into(),
            target_plugin_id: PLUGIN_ID_STAGE_A_PHOTODIODE.into(),
            service: SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1.into(),
            payload: serde_json::to_value(payload).unwrap(),
        }
    }

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
    fn phase0_markers_define_frequency_and_evict_with_the_ring() {
        // Ring holds 1 s = 20_000 samples at 20 kSa/s.
        let mut state = SharedState {
            cache_seconds: 1.0,
            ..SharedState::default()
        };
        // 500 Hz modulation: markers every 40 samples.
        ingest_bytes(&mut state, &sample_frame(0, 0, 20_000, &[100; 40]));
        state.push_marker(0);
        state.push_marker(40);
        state.push_marker(80);
        assert_eq!(state.markers.len(), 3);
        let period = state.marker_period_samples().expect("period");
        assert!((period - 40.0).abs() < 1e-9);
        let hz = f64::from(state.rate_hz) / period;
        assert!((hz - 500.0).abs() < 1e-6, "hz={hz}");

        // Duplicate stamps are ignored, and markers before the ring start too.
        state.push_marker(80);
        state.ring_first_index = 60;
        state.push_marker(40); // now below the ring start
        assert_eq!(state.markers.len(), 3);
    }

    /// The A1 protocols' 0.075 Hz floor at the bench's 500 kSa/s: two whole
    /// cycles are 26.7 s, and the 20 s default cache cannot hold them. The ring
    /// has to follow the drive down on its own — a survey that only learns at
    /// the end of a 267 s recording that no `a` was retained has already spent
    /// the bench time.
    #[test]
    fn ring_follows_the_drive_down_to_sub_hertz() {
        let rate = 500_000_u32;
        let mut state = SharedState {
            rate_hz: rate,
            ..SharedState::default()
        };
        assert_eq!(
            state.ring_capacity(rate),
            10_000_000,
            "with no markers the operator's 20 s default stands"
        );

        // Two markers 0.075 Hz apart are all it takes to know the period.
        let period = (f64::from(rate) / 0.075) as u64;
        state.push_marker(0);
        state.push_marker(period);

        let capacity = state.ring_capacity(rate);
        assert_eq!(capacity, RING_MAX_SAMPLES, "sized up to the absolute cap");
        assert!(
            capacity as u64 >= 2 * period,
            "two cycles at 0.075 Hz need {} samples, ring holds {capacity}",
            2 * period
        );

        // Back up at 8.7 Hz the ring returns to the operator's cache length.
        let fast = (f64::from(rate) / 8.7) as u64;
        state.push_marker(period + fast);
        assert_eq!(state.ring_capacity(rate), 10_000_000);
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

    /// Codes for a full measurement window at `rate_hz`, all at `code`.
    fn level_window_codes(rate_hz: u32, code: u16) -> Vec<u16> {
        vec![code; (f64::from(rate_hz) * LEVEL_WINDOW_SECONDS).round() as usize]
    }

    #[test]
    fn published_level_is_raw_volts_and_survives_clipping() {
        let mut plugin = StageAPhotodiodePlugin::default();
        let mut codes = level_window_codes(20_000, 200);
        codes.extend([100, 200, 300, 400]);
        let mut state = SharedState::default();
        state.ingest(0, 20_000, 0, &codes);

        let level = plugin.current_level(&state).expect("has samples");
        // 400 samples at 20 kSa/s = the full 20 ms window, ending on the newest
        // sample — not the four the chart happens to be smoothing over.
        assert_eq!(level.sample_count, 400);
        assert_eq!(level.end_sample_index, codes.len() as u64);
        assert!(!level.clipped);

        // EXCITATION display must not leak into the published level: it stays
        // the raw detector reading whatever the operator is looking at.
        plugin.set_setting("mode", json!(1)).expect("excitation");
        let raw_again = plugin.current_level(&state).expect("has samples");
        assert_eq!(raw_again.mean_volts, level.mean_volts);

        // At the rail the optical summary refuses; the level must not, because
        // that is exactly where a transfer sweep needs a reading.
        let mut railed = SharedState::default();
        railed.ingest(0, 20_000, 0, &level_window_codes(20_000, 4_095));
        let clipped = plugin.current_level(&railed).expect("still reports");
        assert!(clipped.clipped);
        assert!(plugin.optical_summary(&railed).is_none());
    }

    #[test]
    fn the_published_level_window_ignores_the_chart_averaging_setting() {
        // The sweep's precision is a measurement property. Deriving it from the
        // chart's averaging made a display knob decide it: at the bench's
        // 500 kSa/s the default of four samples published 8 µs per settled CONST
        // code, and a clean Pockels curve came back as a 22 % residual (ADR 019).
        let mut plugin = StageAPhotodiodePlugin::default();
        let mut state = SharedState::default();
        state.ingest(0, 500_000, 0, &level_window_codes(500_000, 300));

        let level = plugin.current_level(&state).expect("has samples");
        assert_eq!(level.sample_count, 10_000, "20 ms at 500 kSa/s");

        for avg in [1, 4, 4_096] {
            plugin
                .set_setting("avg_samples", json!(avg))
                .expect("valid");
            assert_eq!(
                plugin
                    .current_level(&state)
                    .expect("has samples")
                    .sample_count,
                level.sample_count,
                "avg_samples = {avg} moved the published window"
            );
        }
        plugin
            .set_setting("avg_sync_freq_hz", json!(10.0))
            .expect("valid");
        assert_eq!(
            plugin
                .current_level(&state)
                .expect("has samples")
                .sample_count,
            level.sample_count,
            "the sync-averaging setting moved the published window"
        );
    }

    #[test]
    fn a_millivolt_scale_level_is_not_reported_as_railed() {
        // The reject-port detector's dark end sits a few codes above zero. A
        // fixed rail margin called every one of those windows clipped, which the
        // transfer fit then reported as "add attenuation and re-measure".
        let plugin = StageAPhotodiodePlugin::default();
        let rate = 500_000;
        let window = (f64::from(rate) * LEVEL_WINDOW_SECONDS).round() as usize;
        // Swinging between codes 6 and 26 — clear of the rail at both ends.
        let codes: Vec<u16> = (0..window)
            .map(|index| if index % 2 == 0 { 6 } else { 26 })
            .collect();
        let mut state = SharedState::default();
        state.ingest(0, rate, 0, &codes);
        assert!(!plugin.current_level(&state).expect("has samples").clipped);

        // Driven into the bottom rail, the refusal must survive.
        let railed: Vec<u16> = (0..window)
            .map(|index| if index % 2 == 0 { 0 } else { 26 })
            .collect();
        let mut state = SharedState::default();
        state.ingest(0, rate, 0, &railed);
        assert!(plugin.current_level(&state).expect("has samples").clipped);
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
    fn excitation_mode_inverts_against_the_learned_anchor() {
        let mut plugin = StageAPhotodiodePlugin::default();
        plugin.set_setting("mode", json!("EXCITATION")).unwrap();
        // I_pd = 0.5 V → I_exc = I_tot − I_pd = 1.5 V.
        let code = 0.5 * ADC_MAX_CODE / ADC_FULL_SCALE_VOLTS;
        assert!((plugin.display_volts(code, Some(2.0)) - 1.5).abs() < 1e-9);
        // Before the anchor is learned there is nothing to take a complement
        // against, so the raw reading is shown rather than a wrong one.
        assert!((plugin.display_volts(code, None) - 0.5).abs() < 1e-9);
        // RAW mode shows the measured voltage itself.
        plugin.set_setting("mode", json!("RAW")).unwrap();
        assert!((plugin.display_volts(code, Some(2.0)) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mock_reader_fills_the_ring_and_series() {
        let mut plugin = StageAPhotodiodePlugin {
            port_hint: "mock".into(),
            ..live_plugin()
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

    fn marker_frame(sequence: u32, sample_index: u64) -> Frame {
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&sample_index.to_le_bytes());
        payload.extend_from_slice(&0_u32.to_le_bytes()); // tick_us
        payload.push(1); // level
        payload.push(stage_a_io::wire::MARKER_SOURCE_PHASE0); // source
        payload.extend_from_slice(&[0, 0]); // reserved
        Frame::build(
            FrameHeader {
                version: stage_a_io::wire::PROTOCOL_VERSION,
                frame_type: FrameType::Marker,
                flags: 0,
                sequence,
                payload_bytes: 0,
                first_sample_index: sample_index,
                sample_rate_hz: MOCK_RATE_HZ,
                dropped_samples: 0,
                crc32: 0,
            },
            payload,
        )
    }

    #[test]
    fn comparator_markers_never_enter_the_a1_phase_ring() {
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let recording: SharedRecording = Arc::new(Mutex::new(None));
        {
            let mut state = shared.lock().unwrap();
            state.ingest(0, MOCK_RATE_HZ, 0, &[100, 200, 300, 400]);
        }
        let mut frame = marker_frame(1, 2);
        frame.payload[13] = stage_a_io::wire::MARKER_SOURCE_COMPARATOR;
        assert!(ingest_parse_event(
            ParseEvent::Frame(frame),
            &shared,
            &recording,
        ));
        let state = shared.lock().unwrap();
        assert!(state.markers.is_empty());
        assert_eq!(state.comparator_markers.back().copied(), Some((2, 1)));
    }

    #[test]
    fn phase_zero_markers_are_written_into_the_recording() {
        // Without the marker frames a recorded run cannot be phase-attributed
        // offline, which is the whole point of the .pdq evidence file.
        let dir = temp_dir("marker-record");
        let pdq_path = dir.join("run.pdq");
        let shared = Arc::new(Mutex::new(SharedState::default()));
        let recording: SharedRecording = Arc::new(Mutex::new(Some(RecordingSink {
            writer: PdqWriter::create(&pdq_path).expect("create pdq"),
            pdq_path: pdq_path.clone(),
            sidecar_path: dir.join("run.json"),
            pdq_path_label: "run.pdq".into(),
            sidecar_path_label: "run.json".into(),
            run_id: RunId::from("test"),
            opened_at_unix_ms: 0,
            stream_epoch: 0,
            first_sample_index: None,
            metadata: BTreeMap::new(),
            started_slug: "slug".into(),
            samples_written: 0,
            write_error: None,
            start_crc_failures: 0,
            start_resync_bytes: 0,
            start_device_dropped: 0,
            start_segments: 0,
        })));

        let codes = [100_u16, 200, 300, 400];
        assert!(ingest_parse_event(
            ParseEvent::Frame(mock_sample_frame(0, 0, &codes)),
            &shared,
            &recording,
        ));
        assert!(ingest_parse_event(
            ParseEvent::Frame(marker_frame(1, 2)),
            &shared,
            &recording,
        ));

        // The marker still reaches the live ring...
        assert_eq!(
            shared.lock().unwrap().markers.iter().copied().last(),
            Some(2)
        );
        // ...and the sample count is unaffected by the marker frame.
        let sink = recording.lock().unwrap().take().expect("sink");
        assert_eq!(sink.samples_written, codes.len() as u64);
        sink.writer
            .finish(StreamIntegrity::default())
            .expect("finish pdq");

        let mut reader = stage_a_io::PdqReader::open(&pdq_path).expect("open pdq");
        let mut frame_types = Vec::new();
        while let Some(event) = reader.next_event().expect("read event") {
            if let stage_a_io::PdqReadEvent::Frame(frame) = event {
                frame_types.push(frame.header.frame_type);
            }
        }
        assert!(
            frame_types.contains(&FrameType::Marker),
            "the .pdq holds no marker frame: {frame_types:?}"
        );
        assert!(frame_types.contains(&FrameType::SamplesU16));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_snapshot_writes_csv_and_sidecar() {
        let dir = temp_dir("snapshot");
        let mut plugin = live_plugin();
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
    fn forwarded_snapshot_counter_saves_exactly_once() {
        let dir = temp_dir("snapshot-forwarded");
        let mut plugin = live_plugin();
        plugin
            .set_setting("data_dir", json!(dir.display().to_string()))
            .unwrap();
        {
            let mut state = plugin.shared.lock().unwrap();
            state.ingest(10, 20_000, 0, &[100, 200, 300]);
        }
        let csv_count = |dir: &std::path::Path| {
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|ext| ext == "csv"))
                .count()
        };
        // First forwarded counter is the baseline a fresh worker adopts.
        plugin.set_setting("save_snapshot", json!(2)).unwrap();
        assert_eq!(csv_count(&dir), 0, "baseline must not save");
        // One press on the mirror advances the counter by one → one file.
        plugin.set_setting("save_snapshot", json!(3)).unwrap();
        assert_eq!(csv_count(&dir), 1);
        // The host re-applies the same snapshot on every settings sync of any
        // plugin — this used to write one file per sync.
        plugin.set_setting("save_snapshot", json!(3)).unwrap();
        plugin.set_setting("save_snapshot", json!(3)).unwrap();
        assert_eq!(csv_count(&dir), 1, "re-applied snapshots must not save");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn record_buttons_start_and_stop_the_disk_recording() {
        let dir = temp_dir("record-buttons");
        let mut plugin = live_plugin();
        plugin
            .set_setting("data_dir", json!(dir.display().to_string()))
            .unwrap();
        plugin.set_setting("record_start", json!(true)).unwrap();
        assert!(plugin.recording_active());
        // Idle stop is a no-op, an active stop finalizes.
        plugin.set_setting("record_stop", json!(true)).unwrap();
        assert!(!plugin.recording_active());
        assert!(plugin.last_error.is_none(), "{:?}", plugin.last_error);
        plugin.set_setting("record_stop", json!(true)).unwrap();
        assert!(plugin.last_error.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn snapshot_without_data_dir_reports_an_error() {
        let mut plugin = live_plugin();
        plugin.set_setting("save_snapshot", json!(true)).unwrap();
        assert!(plugin
            .last_error
            .as_deref()
            .is_some_and(|err| err.contains("data directory")));
    }

    #[test]
    fn recording_tees_frames_to_pdq_and_writes_a_sidecar() {
        let dir = temp_dir("recording");
        let mut plugin = live_plugin();
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

    #[test]
    fn ui_mirror_never_opens_the_stream_or_writes_recordings() {
        let dir = temp_dir("ui-mirror");
        let mut plugin = StageAPhotodiodePlugin {
            port_hint: "mock".into(),
            data_dir: dir.display().to_string(),
            ..Default::default()
        };
        plugin.set_setting("connect", json!(true)).unwrap();
        plugin.set_setting("record", json!(true)).unwrap();
        assert!(!plugin.connected());
        assert!(!plugin.recording_active());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn service_is_idempotent_and_enforces_exclusive_leases_without_frames() {
        let mut plugin = live_plugin();
        let acquire = service_request(
            &plugin,
            1,
            "workflow-a",
            PhotodiodeCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        let first = plugin.handle_service_request(&acquire, &live_execution());
        let expiry = plugin.lease.as_ref().unwrap().expires_at_unix_ms;
        let duplicate = plugin.handle_service_request(&acquire, &live_execution());
        assert_eq!(first, duplicate);
        assert_eq!(plugin.lease.as_ref().unwrap().expires_at_unix_ms, expiry);

        let conflict = service_request(
            &plugin,
            2,
            "workflow-b",
            PhotodiodeCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        assert!(matches!(
            plugin
                .handle_service_request(&conflict, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));
        assert!(plugin.set_setting("mode", json!("RAW")).is_err());
    }

    /// A workflow client that names its own recording root gets the PDQ written
    /// there, and the owner's Data directory stops being involved at all — that
    /// is what lets one coordinated run keep every file in one folder.
    #[test]
    fn a_client_named_root_overrides_the_data_directory() {
        let dir = temp_dir("client-root");
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        // Deliberately unset: it must not be consulted.
        plugin.data_dir = String::new();
        plugin.connect();
        let acquire = service_request(
            &plugin,
            20,
            "workflow-a",
            PhotodiodeCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        assert!(matches!(
            plugin
                .handle_service_request(&acquire, &live_execution())
                .outcome,
            PluginServiceOutcome::Accepted { .. }
        ));

        let begin = service_request(
            &plugin,
            21,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: "A1-row/run_pd.pdq".into(),
                    sidecar_path: "A1-row/run_pd.json".into(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::new(),
                    root_dir: Some(dir.display().to_string()),
                },
            },
            Some(1),
        );
        assert!(
            matches!(
                plugin
                    .handle_service_request(&begin, &live_execution())
                    .outcome,
                PluginServiceOutcome::Accepted { .. }
            ),
            "a client-named root must not need the owner's data directory"
        );
        assert!(dir.join("A1-row/run_pd.pdq").is_file());

        // Traversal is still refused below a client-named root.
        let escape = service_request(
            &plugin,
            22,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: "../escape.pdq".into(),
                    sidecar_path: "A1-row/escape.json".into(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::new(),
                    root_dir: Some(dir.display().to_string()),
                },
            },
            Some(2),
        );
        assert!(matches!(
            plugin
                .handle_service_request(&escape, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));

        // A relative root is refused outright.
        let relative_root = service_request(
            &plugin,
            23,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: "A1-row/other_pd.pdq".into(),
                    sidecar_path: "A1-row/other_pd.json".into(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::new(),
                    root_dir: Some("relative/root".into()),
                },
            },
            Some(3),
        );
        assert!(matches!(
            plugin
                .handle_service_request(&relative_root, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_recording_rejects_unsafe_paths_and_returns_final_receipt() {
        let dir = temp_dir("named");
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.data_dir = dir.display().to_string();
        plugin.connect();
        let acquire = service_request(
            &plugin,
            10,
            "workflow-a",
            PhotodiodeCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        assert!(matches!(
            plugin
                .handle_service_request(&acquire, &live_execution())
                .outcome,
            PluginServiceOutcome::Accepted { .. }
        ));

        let unsafe_begin = service_request(
            &plugin,
            11,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: "../escape.pdq".into(),
                    sidecar_path: "run/escape.json".into(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::new(),
                    root_dir: None,
                },
            },
            Some(1),
        );
        assert!(matches!(
            plugin
                .handle_service_request(&unsafe_begin, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));

        let begin = service_request(
            &plugin,
            12,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: "A1/run-a_pd.pdq".into(),
                    sidecar_path: "A1/run-a_pd.json".into(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::from([("workflow".into(), "A1".into())]),
                    root_dir: None,
                },
            },
            Some(1),
        );
        let begin_reply = plugin.handle_service_request(&begin, &live_execution());
        assert!(matches!(
            begin_reply.outcome,
            PluginServiceOutcome::Accepted { .. }
        ));
        assert_eq!(
            plugin.handle_service_request(&begin, &live_execution()),
            begin_reply,
            "duplicate begin must not open a second file"
        );
        record_frame(&plugin.recording, &mock_sample_frame(9, 0, &[1, 2, 3]), 3);

        let finalize = service_request(
            &plugin,
            13,
            "workflow-a",
            PhotodiodeCommandV1::FinalizeRecording {
                termination: PdqTerminationV1::Completed,
            },
            Some(2),
        );
        let reply = plugin.handle_service_request(&finalize, &live_execution());
        let PluginServiceOutcome::Accepted { payload } = reply.outcome else {
            panic!("finalize rejected");
        };
        let response: PhotodiodeResponseV1 = serde_json::from_value(payload).unwrap();
        let Some(PdqReceiptV1::Finalized(receipt)) = response.receipt else {
            panic!("missing finalized receipt");
        };
        assert_eq!(receipt.sha256.as_str().len(), 64);
        assert!(receipt.file_size_bytes > 0);
        assert!(dir.join(&receipt.pdq_path).is_file());
        assert!(dir.join(&receipt.sidecar_path).is_file());

        let collision = service_request(
            &plugin,
            14,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: receipt.pdq_path.clone(),
                    sidecar_path: receipt.sidecar_path.clone(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::new(),
                    root_dir: None,
                },
            },
            Some(3),
        );
        assert!(matches!(
            plugin
                .handle_service_request(&collision, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));
        plugin.disconnect();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn effects_revocation_finalizes_and_disconnects_without_a_frame() {
        let dir = temp_dir("revoked");
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.data_dir = dir.display().to_string();
        plugin.connect();
        let acquire = service_request(
            &plugin,
            20,
            "workflow-a",
            PhotodiodeCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());
        let begin = service_request(
            &plugin,
            21,
            "workflow-a",
            PhotodiodeCommandV1::BeginRecording {
                specification: PdqStartSpecV1 {
                    pdq_path: "revoked/run.pdq".into(),
                    sidecar_path: "revoked/run.json".into(),
                    expected_sample_rate_hz: None,
                    expected_stream_epoch: None,
                    metadata: BTreeMap::new(),
                    root_dir: None,
                },
            },
            Some(1),
        );
        plugin.handle_service_request(&begin, &live_execution());
        assert!(plugin.recording_active());

        plugin.apply_execution_context(&ExecutionContext::fail_closed());
        assert!(!plugin.connected());
        assert!(!plugin.recording_active());
        assert!(plugin.lease.is_none());
        assert_eq!(
            plugin
                .last_finalized_recording
                .as_ref()
                .unwrap()
                .termination,
            PdqTerminationV1::Aborted
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
