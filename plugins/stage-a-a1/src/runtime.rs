//! Live A1 recording coordinator.
//!
//! A1 has two jobs on the Stage-A bench, both deliberately thin:
//!
//! 1. **Recording coordinator.** One *Start recording* button records, for a fixed
//!    duration, the camera **RAW** stream (host recording) and the photodiode **PDQ**
//!    stream (leased `stage-a.photodiode` service) together, grouped under a
//!    per-`(I_k, f)` measurement **id** and a shared `<id>_<timestamp>` file stem, and
//!    writes an A1 **config sidecar** (`.toml`) linking the two files with the
//!    modulation settings, the photodiode-measured modulation depth `a`, the ROI, and
//!    the trigger info needed to reproduce and analyse the run offline. A1 owns no
//!    hardware and, outside the leased sweep below, never drives the Teensy — the
//!    optical drive is armed in the modulation plugin; A1 only *reads* its published
//!    settings into the sidecar. The **amplitude sweep** (ADR 010) is the one scoped
//!    exception: per sweep point it retargets the armed drive's *depth* through the
//!    leased modulation service (`SetOpticalDepth`), waits for the photodiode-measured
//!    `a` to settle, and records the point through the same coordinator.
//!
//! 2. **Live sanity quicklooks.** Folding the camera event stream on the modulation
//!    period `T` (defined by the firmware phase-0 `EXT_TRIGGER`), it renders the
//!    **rolling half-period response** `S_p(t)` (a live "are events appearing, is the
//!    ON/OFF timing sane?" indicator) and the **response probability** `q_p` curve
//!    (frozen-window Bernoulli statistic vs the measured `a`). The authoritative
//!    `q_p(a, f)` fit is computed offline from the recordings; the live plot is a
//!    quicklook.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use augur_plugin_api::{
    export_plugin, EventStoreHandle, FfiCdEvent, GlobalSettings, HostCommand, HostCommandOutcome,
    HostCommandReply, HostCommandRequest, HostContext, HostDatasetDescriptor, HostDatasetKind,
    HostOutput, HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry,
    PathDialogKind, Plugin, PluginCapabilities, PluginControlContext, PluginControlInbox,
    PluginDiscontinuity, PluginFrame, PluginInput, PluginRuntimeRole, PluginServiceOutcome,
    PluginServiceReply, PluginServiceRequest, RoiV1, Series1dLine, Series1dPoint, Series1dV1,
    SettingItem, SettingKind, SettingsSchema, SettingsSection, StatusEntry, TableColumn,
    TableColumnData, TableColumnValues, TableDatasetV1, TableSchema, TableValueType,
    CTX_GLOBAL_SETTINGS,
};
use serde::Serialize;
use serde_json::{json, Value};
use stage_a_plugin_contract::{
    ClientId, ConnectionStateV1, LeaseId, ModulationCommandV1, ModulationRequestV1,
    ModulationStateV1, PdqReceiptV1, PdqStartSpecV1, PhotodiodeCommandV1, PhotodiodeRequestV1,
    PhotodiodeResponseV1, PhotodiodeSummaryV1, RequestId, RunId, SemanticRevision, WaveformV1,
    CTX_STAGE_A_MODULATION_STATE_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
    SERVICE_STAGE_A_MODULATION_CONTROL_V1, SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
};

use crate::phase::{fold_events, fold_events_free_running, MarkerValidationConfig, PhaseFold};
use crate::rates::{rolling_half_period_response, RollingResponsePoint};
use crate::response_curve::{auto_windows, response_probability, PhaseWindow, ResponsePoint, Roi};
use crate::types::{CameraEvent, Polarity};

const MODULATION_PLUGIN_ID: &str = "stage-a.modulation";
const PHOTODIODE_PLUGIN_ID: &str = "stage-a.photodiode";
const A1_PLUGIN_ID: &str = "stage-a.a1";

const STATUS_DATASET_ID: &str = "stage-a-a1.status";
const STATUS_VIEW_ID: &str = "stage-a-a1.status.view";
const ROLLING_DATASET_ID: &str = "stage-a-a1.rolling-response";
const ROLLING_VIEW_ID: &str = "stage-a-a1.rolling-response.view";
const RESPONSE_CURVE_DATASET_ID: &str = "stage-a-a1.response-curve";
const RESPONSE_CURVE_VIEW_ID: &str = "stage-a-a1.response-curve.view";

/// Camera events retained for the live fold. At the bench event rates this is a
/// few seconds of history and keeps the fold cost bounded.
const MAX_EVENTS: usize = 4_000_000;
/// Sample points on the rolling half-period trace.
const ROLLING_SAMPLES: u64 = 256;
/// Default `q_p` window floor: grow each ON/OFF window until it falls to this
/// fraction of its histogram peak (or the opposite polarity takes over).
const DEFAULT_WINDOW_FLOOR: f64 = 0.10;
/// Default analysis window (ms) pulled from the retained EventStore each frame.
const DEFAULT_ANALYSIS_WINDOW_MS: i64 = 2_000;
/// Give up waiting for a control-plane reply after this many milliseconds.
const REPLY_TIMEOUT_MS: u64 = 15_000;
/// Upper bound on retained phase-0 markers in the no-EventStore fallback path.
const MAX_MARKERS: usize = 65_536;
/// Give up waiting for the photodiode-measured `a` to reach a sweep target
/// after this long and record anyway (the sidecar stores the measured value).
const SWEEP_SETTLE_TIMEOUT_MS: u64 = 30_000;

/// Absolute/relative tolerance for "the measured `a` reached the sweep target".
fn sweep_tolerance(target_a: f64) -> f64 {
    (target_a * 0.10).max(0.05)
}

trait RecordingControl {
    fn request_service(&mut self, request: &PluginServiceRequest);
    fn request_host(&mut self, request: &HostCommandRequest);
}

impl RecordingControl for PluginControlContext<'_> {
    fn request_service(&mut self, request: &PluginServiceRequest) {
        let _ = PluginControlContext::request_service(self, request);
    }

    fn request_host(&mut self, request: &HostCommandRequest) {
        let _ = PluginControlContext::request_host(self, request);
    }
}

/// Forwards momentary button presses across the host's UI-mirror → live-worker
/// settings snapshot. A click arrives as `true` on the clicked instance; the
/// other instance only ever sees the snapshot value from `get_setting`, so the
/// press is transported as a monotonic counter and a counter advance counts as
/// one press edge. The first counter a fresh instance sees is adopted silently
/// so a reloaded worker does not replay old presses.
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

/// Where the coordinated recording is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecPhase {
    Idle,
    /// Camera start sent; waiting until the host has switched into recording.
    StartingCamera,
    /// Camera is running; reconnecting the photodiode after the pipeline switch.
    ConnectingPhotodiode,
    /// AcquireLease sent to the photodiode; waiting for the grant.
    AcquiringLease,
    /// Camera is running; waiting for the photodiode PDQ start receipt.
    StartingPhotodiode,
    /// Camera RAW + photodiode PDQ recording are both in flight.
    Running,
    /// Photodiode finalize sent; camera keeps recording until PDQ is closed.
    StoppingPhotodiode,
    /// PDQ is closed; waiting for the host camera finalize receipt.
    StoppingCamera,
}

/// What a recording is for within one `(I_k, f)` measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecRole {
    /// One amplitude point of the sweep.
    Normal,
    /// Bright reference that freezes the ON/OFF windows for the whole row.
    Pilot,
    /// Unmodulated (`a≈0`) reference that gives the false-response floor.
    Background,
}

impl RecRole {
    /// Filename-stem suffix, empty for a normal sweep point.
    fn suffix(self) -> &'static str {
        match self {
            RecRole::Normal => "",
            RecRole::Pilot => "_pilot",
            RecRole::Background => "_background",
        }
    }

    fn label(self) -> &'static str {
        match self {
            RecRole::Normal => "point",
            RecRole::Pilot => "pilot",
            RecRole::Background => "background",
        }
    }
}

/// One coordinated `(camera RAW + photodiode PDQ + sidecar)` recording.
struct Recording {
    phase: RecPhase,
    role: RecRole,
    id: String,
    stem: String,
    folder: String,
    duration_s: u64,
    start_unix_ms: u64,
    last_activity_ms: u64,
    lease_id: LeaseId,
    stop_requested: bool,
    // outstanding request-id correlation
    connect_req: u64,
    lease_req: u64,
    cam_start_req: u64,
    cam_stop_req: u64,
    pd_begin_req: u64,
    pd_finalize_req: u64,
    // captured receipts
    connect_accepted: bool,
    lease_granted: bool,
    cam_raw_path: Option<String>,
    cam_finalized_path: Option<String>,
    /// True only for a complete host finalization receipt, not a partial file.
    cam_complete: bool,
    /// The host rejected StartRecording — skip the stop and don't wait for a
    /// finalize receipt.
    cam_rejected: bool,
    pd_pdq_path: Option<String>,
    pd_sidecar_path: Option<String>,
    pd_finalized: bool,
    pd_valid: bool,
    /// The photodiode rejected BeginRecording — skip the finalize and don't
    /// wait for its receipt.
    pd_rejected: bool,
    /// First thing that went wrong, kept verbatim so the closing message names
    /// the cause instead of only reporting that the run was incomplete.
    failure: Option<String>,
}

/// Where the amplitude sweep is within its per-point cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepPhase {
    /// AcquireLease sent to the modulation owner; waiting for the grant.
    AcquiringLease,
    /// SetOpticalDepth for the current point sent; waiting for Applied.
    SettingDepth,
    /// Waiting for the photodiode-measured `a` to settle at the target.
    Settling,
    /// The per-point recording coordinator owns this phase.
    Recording,
}

/// One "record every point of the amplitude range" run: per point the sweep
/// retargets the leased modulation drive, waits for the photodiode-measured
/// `a` to settle, and hands off to the normal recording coordinator.
struct Sweep {
    phase: SweepPhase,
    /// Requested `a` per point, ascending over `[min_a, max_a]`.
    points: Vec<f64>,
    index: usize,
    lease_id: LeaseId,
    lease_granted: bool,
    lease_req: u64,
    depth_req: u64,
    depth_applied: bool,
    /// Instant the measured `a` first satisfied the tolerance, for the dwell.
    settled_since_ms: Option<u64>,
    /// Give-up deadline for the settle phase.
    settle_deadline_ms: u64,
    /// Whether the current point's recording actually started (vs. was
    /// refused by validation before it began).
    point_started: bool,
    last_activity_ms: u64,
    stop_requested: bool,
}

impl Sweep {
    fn target_a(&self) -> f64 {
        self.points.get(self.index).copied().unwrap_or(0.0)
    }

    fn total(&self) -> usize {
        self.points.len()
    }
}

pub struct StageAA1Plugin {
    enabled: bool,
    runtime_role: PluginRuntimeRole,
    /// While true, camera events are folded into the live quicklooks. This does
    /// not record anything — recording is the separate coordinator below.
    live: bool,
    modulation: Option<ModulationStateV1>,
    photodiode: Option<PhotodiodeSummaryV1>,
    camera_events: Vec<CameraEvent>,
    /// Reusable buffer for exact events pulled from the retained EventStore.
    event_scratch: Vec<FfiCdEvent>,
    /// Sliding analysis window (ms) for the live fold.
    analysis_window_ms: i64,
    /// Rising `EXT_TRIGGER` timestamps (firmware phase-0 sync). When present these
    /// anchor the fold to the drive on the camera clock; empty falls back to the
    /// free-running fold on `T`.
    camera_markers_us: Vec<u64>,
    valid_pixels: usize,
    frame_width: u16,
    frame_height: u16,
    // -- host camera ROI/mask, mirrored from CTX_GLOBAL_SETTINGS --
    host_roi: Option<RoiV1>,
    masked_pixels: HashSet<(u16, u16)>,
    // -- response curve (auto-windowed Bernoulli q_p) --
    /// Window floor as a fraction of the ON/OFF histogram peak (see `auto_windows`).
    window_floor: f64,
    response_points: Vec<ResponsePoint>,
    /// ON/OFF windows frozen from the pilot for the current measurement row. When
    /// set they override the per-fold auto-windows so the row's `q_p` is
    /// consistent; loaded from the pilot's sidecar in the measurement folder.
    pilot_windows: Option<(PhaseWindow, PhaseWindow)>,
    /// Background floor `(q0_on, q0_off)` from the `a≈0` reference.
    background_floor: Option<(f64, f64)>,
    // -- recording coordinator --
    output_folder: String,
    measurement_id: String,
    /// Sweep range `[min_a, max_a]` for this `(I_k, f)` row (automation template).
    min_a: f64,
    max_a: f64,
    duration_s: i64,
    recording: Recording,
    /// Whether the most recent recording reached its finalize path (vs. being
    /// aborted); the sweep uses this to decide between advancing and stopping.
    recording_completed_ok: bool,
    request_seq: u64,
    pd_revision_seq: u64,
    /// Role latched by the Start/Pilot/Background buttons, consumed next tick.
    pending_role: Option<RecRole>,
    /// `(folder, id)` last scanned for pilot/background sidecars, so the folder is
    /// re-read only when the measurement changes.
    loaded_key: Option<(String, String)>,
    /// One-line operator feedback about the most recent recording action.
    message: String,
    dataset_generation: u64,
    // -- amplitude sweep --
    /// Number of sweep points across `[min_a, max_a]`.
    sweep_count: i64,
    /// Dwell the measured `a` must hold the target tolerance before recording.
    settle_s: f64,
    /// Latched by the Start sweep button, consumed next control tick.
    sweep_pending: bool,
    sweep: Option<Sweep>,
    // -- momentary-button press forwarding (see PressLatch) --
    press_start: PressLatch,
    press_pilot: PressLatch,
    press_background: PressLatch,
    press_stop: PressLatch,
    press_sweep: PressLatch,
    press_clear: PressLatch,
    press_record_point: PressLatch,
    press_clear_curve: PressLatch,
}

impl Default for StageAA1Plugin {
    fn default() -> Self {
        Self {
            enabled: false,
            runtime_role: PluginRuntimeRole::UiMirror,
            live: false,
            modulation: None,
            photodiode: None,
            camera_events: Vec::new(),
            event_scratch: Vec::new(),
            analysis_window_ms: DEFAULT_ANALYSIS_WINDOW_MS,
            camera_markers_us: Vec::new(),
            valid_pixels: 0,
            frame_width: 0,
            frame_height: 0,
            host_roi: None,
            masked_pixels: HashSet::new(),
            window_floor: DEFAULT_WINDOW_FLOOR,
            response_points: Vec::new(),
            pilot_windows: None,
            background_floor: None,
            output_folder: String::new(),
            measurement_id: generate_measurement_id(),
            min_a: 0.0,
            max_a: 2.0,
            duration_s: 10,
            recording: Recording::idle(),
            recording_completed_ok: false,
            request_seq: 0,
            pd_revision_seq: 0,
            pending_role: None,
            loaded_key: None,
            message: String::new(),
            dataset_generation: 1,
            sweep_count: 5,
            settle_s: 2.0,
            sweep_pending: false,
            sweep: None,
            press_start: PressLatch::default(),
            press_pilot: PressLatch::default(),
            press_background: PressLatch::default(),
            press_stop: PressLatch::default(),
            press_sweep: PressLatch::default(),
            press_clear: PressLatch::default(),
            press_record_point: PressLatch::default(),
            press_clear_curve: PressLatch::default(),
        }
    }
}

impl Recording {
    fn idle() -> Self {
        Self {
            phase: RecPhase::Idle,
            role: RecRole::Normal,
            id: String::new(),
            stem: String::new(),
            folder: String::new(),
            duration_s: 0,
            start_unix_ms: 0,
            last_activity_ms: 0,
            lease_id: LeaseId::new(String::new()),
            stop_requested: false,
            connect_req: 0,
            lease_req: 0,
            cam_start_req: 0,
            cam_stop_req: 0,
            pd_begin_req: 0,
            pd_finalize_req: 0,
            connect_accepted: false,
            lease_granted: false,
            cam_raw_path: None,
            cam_finalized_path: None,
            cam_complete: false,
            cam_rejected: false,
            pd_pdq_path: None,
            pd_sidecar_path: None,
            pd_finalized: false,
            pd_valid: false,
            pd_rejected: false,
            failure: None,
        }
    }

    /// Records the first failure only: later fallout (a stop that finds nothing
    /// to finalize) must not mask the reason the run went wrong.
    fn fail(&mut self, reason: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(reason.into());
        }
    }

    fn is_active(&self) -> bool {
        self.phase != RecPhase::Idle
    }

    fn state_label(&self) -> &'static str {
        match self.phase {
            RecPhase::Idle => "idle",
            RecPhase::StartingCamera => "starting camera",
            RecPhase::ConnectingPhotodiode => "connecting photodiode",
            RecPhase::AcquiringLease => "acquiring lease",
            RecPhase::StartingPhotodiode => "starting photodiode",
            RecPhase::Running => "recording",
            RecPhase::StoppingPhotodiode => "finalizing photodiode",
            RecPhase::StoppingCamera => "finalizing camera",
        }
    }

    /// Seconds remaining in the fixed-duration window, when running.
    fn remaining_s(&self, now_ms: u64) -> Option<u64> {
        if self.phase != RecPhase::Running {
            return None;
        }
        let elapsed_ms = now_ms.saturating_sub(self.start_unix_ms);
        let total_ms = self.duration_s.saturating_mul(1_000);
        Some(total_ms.saturating_sub(elapsed_ms) / 1_000)
    }
}

impl StageAA1Plugin {
    fn bump(&mut self) {
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
    }

    /// Sets the concise operator-facing recording result.
    fn note(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.bump();
    }

    /// Reports a failure and remembers it as the run's cause, so the closing
    /// message can name it after the coordinator has unwound. The first cause
    /// wins: it is the specific one, and later arms only see the fallout.
    fn note_failure(&mut self, message: impl Into<String>) {
        if self.recording.failure.is_some() {
            return;
        }
        let message = message.into();
        self.recording.fail(message.clone());
        self.note(message);
    }

    /// The modulation period `T` in microseconds: measured from the phase-0
    /// markers when present (the trigger *defines* the frequency, latency-
    /// invariant), otherwise the modulation plugin's acknowledged waveform.
    fn period_us(&self) -> Option<f64> {
        if let Some(period) = self.measured_period_us() {
            return Some(period);
        }
        let hz = self.acknowledged_frequency_hz()?;
        (hz > 0.0).then(|| 1_000_000.0 / hz)
    }

    /// Modulation period measured from the phase-0 markers (mean spacing).
    fn measured_period_us(&self) -> Option<f64> {
        if self.camera_markers_us.len() < 2 {
            return None;
        }
        let first = *self.camera_markers_us.first()?;
        let last = *self.camera_markers_us.last()?;
        let spans = (self.camera_markers_us.len() - 1) as f64;
        let period = last.saturating_sub(first) as f64 / spans;
        (period > 0.0).then_some(period)
    }

    /// Frequency (Hz) from the modulation plugin's acknowledged periodic waveform.
    fn acknowledged_frequency_hz(&self) -> Option<f64> {
        let target = self.modulation.as_ref()?.acknowledged.as_ref()?;
        match target.waveform.as_ref()? {
            WaveformV1::Periodic {
                frequency_millihz, ..
            } => (*frequency_millihz > 0).then(|| *frequency_millihz as f64 / 1_000.0),
            _ => None,
        }
    }

    fn is_marker_anchored(&self) -> bool {
        self.camera_markers_us.len() >= 2
    }

    fn frequency_source(&self) -> &'static str {
        if self.measured_period_us().is_some() {
            "trigger"
        } else {
            "modulation"
        }
    }

    fn current_fold(&self) -> Option<PhaseFold> {
        let period_us = self.period_us()?;
        let marker_fold = self.is_marker_anchored().then(|| {
            let expected_hz = 1_000_000.0 / period_us;
            fold_events(
                &self.camera_events,
                &self.camera_markers_us,
                MarkerValidationConfig {
                    expected_frequency_hz: expected_hz,
                    // Live quicklook: accept real-world drift/jitter rather than
                    // rejecting the whole fold.
                    frequency_tolerance_fraction: 0.5,
                    max_period_jitter_fraction: 0.75,
                    expected_cycles: None,
                },
            )
            .ok()
        });
        // A marker glitch (dropped trigger, out-of-tolerance jitter) must not
        // blank the live plots — fall back to the free-running fold on T.
        marker_fold
            .flatten()
            .or_else(|| fold_events_free_running(&self.camera_events, period_us))
    }

    /// Optical modulation depth `a` published by the photodiode plugin.
    fn measured_a(&self) -> Option<f64> {
        self.photodiode
            .as_ref()?
            .optical_summary
            .as_ref()
            .map(|summary| summary.measured_log_contrast)
    }

    /// Current ROI from the host camera config, clamped to the frame.
    fn roi(&self) -> Option<Roi> {
        if self.frame_width == 0 || self.frame_height == 0 {
            return None;
        }
        let host = self.host_roi.unwrap_or_default();
        let x0 = host.x.min(self.frame_width);
        let y0 = host.y.min(self.frame_height);
        let x1 = if host.width == 0 {
            self.frame_width
        } else {
            host.x.saturating_add(host.width).min(self.frame_width)
        };
        let y1 = if host.height == 0 {
            self.frame_height
        } else {
            host.y.saturating_add(host.height).min(self.frame_height)
        };
        (x1 > x0 && y1 > y0).then_some(Roi { x0, y0, x1, y1 })
    }

    /// Number of valid pixels: ROI area minus masked pixels inside it.
    fn valid_pixel_count(&self) -> Option<usize> {
        let roi = self.roi()?;
        let masked = self
            .masked_pixels
            .iter()
            .filter(|(x, y)| roi.contains(*x, *y))
            .count();
        Some(roi.area().saturating_sub(masked))
    }

    /// ON/OFF phase windows for `q_p`: the pilot-frozen windows when a pilot has
    /// been recorded for this row, otherwise the per-fold auto-windows.
    fn current_windows(&self) -> Option<(PhaseWindow, PhaseWindow)> {
        if let Some(windows) = self.pilot_windows {
            return Some(windows);
        }
        auto_windows(&self.current_fold()?, self.window_floor)
    }

    /// Whether the `q_p` windows are frozen from a pilot (vs live auto-windows).
    fn windows_are_frozen(&self) -> bool {
        self.pilot_windows.is_some()
    }

    /// ON/OFF response probability for the current fold against `current_windows`.
    fn current_response(&self) -> Option<(f64, f64, usize, usize)> {
        let fold = self.current_fold()?;
        let roi = self.roi()?;
        let (window_on, window_off) = self.current_windows()?;
        response_probability(&fold, window_on, window_off, roi, &self.masked_pixels)
    }

    /// Freezes the ON/OFF windows for this row from the current fold (a pilot).
    fn freeze_pilot_windows(&mut self) {
        match self
            .current_fold()
            .and_then(|fold| auto_windows(&fold, self.window_floor))
        {
            Some(windows) => {
                self.pilot_windows = Some(windows);
                self.note("Pilot windows frozen from the live signal");
            }
            None => {
                self.note("No live signal to freeze windows — enable Live analysis first");
            }
        }
    }

    /// Captures the background floor `(q0_on, q0_off)` from the current fold.
    fn capture_background_floor(&mut self) {
        match self.current_response() {
            Some((q_on, q_off, _, _)) => {
                self.background_floor = Some((q_on, q_off));
                self.note(format!("Background floor captured (q0_on={q_on:.3})"));
            }
            None => {
                self.note("No valid background window yet (need events and a valid ROI)");
            }
        }
    }

    /// Records one response-curve point at the current photodiode-measured `a`.
    fn record_response_point(&mut self) -> Result<(), String> {
        let measured_a = self
            .measured_a()
            .ok_or("no photodiode-measured a available (connect the photodiode)")?;
        let (q_on, q_off, cycles, valid_pixels) = self
            .current_response()
            .ok_or("no valid response window yet (need trigger-anchored events and a valid ROI)")?;
        self.response_points.push(ResponsePoint {
            measured_a,
            q_on,
            q_off,
            cycles,
            valid_pixels,
        });
        Ok(())
    }

    fn response_curve_dataset(&self) -> Series1dV1 {
        let line = |select: fn(&ResponsePoint) -> f64| {
            let mut points: Vec<Series1dPoint> = self
                .response_points
                .iter()
                .map(|point| Series1dPoint {
                    x: point.measured_a,
                    y: select(point),
                })
                .collect();
            points.sort_by(|a, b| a.x.total_cmp(&b.x));
            points
        };
        Series1dV1 {
            x_label: "Measured modulation depth a = ln(I_max / I_min)".into(),
            y_label: "Response probability q_p = fraction of pixel-cycles that fired".into(),
            lines: vec![
                Series1dLine {
                    name: "ON".into(),
                    points: line(|point| point.q_on),
                },
                Series1dLine {
                    name: "OFF".into(),
                    points: line(|point| point.q_off),
                },
            ],
        }
    }

    fn rolling_dataset(&self) -> Series1dV1 {
        const X: &str = "Camera time since first event (s)";
        const Y: &str = "Events per valid pixel in the trailing half-cycle T/2";
        let empty = || Series1dV1 {
            x_label: X.into(),
            y_label: Y.into(),
            lines: vec![
                Series1dLine {
                    name: "ON".into(),
                    points: Vec::new(),
                },
                Series1dLine {
                    name: "OFF".into(),
                    points: Vec::new(),
                },
            ],
        };
        let Some(fold) = self.current_fold() else {
            return empty();
        };
        let first = fold.validation.first_marker_us;
        let last = fold.validation.last_marker_us;
        let samples = ROLLING_SAMPLES.min(last.saturating_sub(first).saturating_add(1));
        if samples < 2 {
            return empty();
        }
        let sample_times: Vec<u64> = (0..samples)
            .map(|index| first + (last - first) * index / (samples - 1))
            .collect();
        let line = |polarity: Polarity| {
            rolling_half_period_response(&fold, polarity, self.valid_pixels, &sample_times, None)
                .map(|points| points_for(&points, first))
                .unwrap_or_default()
        };
        Series1dV1 {
            x_label: X.into(),
            y_label: Y.into(),
            lines: vec![
                Series1dLine {
                    name: "ON".into(),
                    points: line(Polarity::On),
                },
                Series1dLine {
                    name: "OFF".into(),
                    points: line(Polarity::Off),
                },
            ],
        }
    }

    /// Latest rolling half-period value per polarity, for the status readout.
    fn latest_rolling(&self) -> Option<(f64, f64)> {
        let fold = self.current_fold()?;
        let at = [fold.validation.last_marker_us];
        let value = |polarity| {
            rolling_half_period_response(&fold, polarity, self.valid_pixels, &at, None)
                .ok()
                .and_then(|points| points.first().map(|point| point.run_per_pixel))
        };
        Some((value(Polarity::On)?, value(Polarity::Off)?))
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let now_ms = now_unix_ms();
        let period_us = self.period_us();
        let frequency = period_us.map(|t| 1_000_000.0 / t);
        let source = self.frequency_source();
        let (on_now, off_now) = self
            .latest_rolling()
            .map_or((None, None), |(on, off)| (Some(on), Some(off)));
        let cell = |id: &str, value: String| TableColumnData {
            column_id: id.into(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                cell("state", self.recording.state_label().into()),
                cell(
                    "measurement_id",
                    if self.recording.is_active() {
                        self.recording.id.clone()
                    } else {
                        self.measurement_id.clone()
                    },
                ),
                cell(
                    "remaining",
                    self.recording
                        .remaining_s(now_ms)
                        .map_or_else(|| "—".into(), |s| format!("{s} s")),
                ),
                cell(
                    "frequency",
                    frequency.map_or_else(|| "—".into(), |hz| format!("{hz:.3} Hz ({source})")),
                ),
                cell(
                    "a",
                    self.measured_a()
                        .map_or_else(|| "—".into(), |a| format!("{a:.3}")),
                ),
                cell(
                    "s_on",
                    on_now.map_or_else(|| "—".into(), |v| format!("{v:.4}")),
                ),
                cell(
                    "s_off",
                    off_now.map_or_else(|| "—".into(), |v| format!("{v:.4}")),
                ),
                cell("events", self.camera_events.len().to_string()),
                cell(
                    "message",
                    // While idle, anything that would refuse the next recording
                    // is worth more than the previous run's result: the operator
                    // sees it before pressing Record, not after.
                    match (self.recording.is_active(), self.photodiode_blocker()) {
                        (false, Some(blocker)) => blocker,
                        _ if self.message.is_empty() => "—".into(),
                        _ => self.message.clone(),
                    },
                ),
            ],
        }
    }

    fn update_snapshots(&mut self, inbox: &PluginControlInbox) {
        for snapshot in &inbox.snapshots {
            match (snapshot.plugin_id.as_str(), snapshot.topic.as_str()) {
                (MODULATION_PLUGIN_ID, CTX_STAGE_A_MODULATION_STATE_V1) => {
                    if let Ok(state) = serde_json::from_value(snapshot.payload.clone()) {
                        self.modulation = Some(state);
                    }
                }
                (PHOTODIODE_PLUGIN_ID, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1) => {
                    if let Ok(summary) = serde_json::from_value(snapshot.payload.clone()) {
                        self.photodiode = Some(summary);
                    }
                }
                _ => {}
            }
        }
    }

    fn next_request_id(&mut self) -> u64 {
        self.request_seq = self.request_seq.wrapping_add(1);
        self.request_seq
    }

    /// Wraps a photodiode command in the routed service request A1 emits.
    fn photodiode_request(&mut self, command: PhotodiodeCommandV1) -> PluginServiceRequest {
        let request_id = self.next_request_id();
        let needs_revision = matches!(
            command,
            PhotodiodeCommandV1::BeginRecording { .. }
                | PhotodiodeCommandV1::FinalizeRecording { .. }
                | PhotodiodeCommandV1::AbortRecording { .. }
        );
        let mut envelope =
            PhotodiodeRequestV1::new(RequestId(request_id), ClientId::new(A1_PLUGIN_ID), command);
        envelope.lease_id = Some(self.recording.lease_id.clone());
        if !self.recording.stem.is_empty() {
            envelope.run_id = Some(RunId::new(self.recording.stem.clone()));
        }
        if needs_revision {
            let observed = self
                .photodiode
                .as_ref()
                .map(|summary| {
                    summary
                        .requested_revision
                        .into_iter()
                        .chain(summary.acknowledged_revision)
                        .map(|revision| revision.0)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            self.pd_revision_seq = self
                .pd_revision_seq
                .saturating_add(1)
                .max(observed.saturating_add(1));
            envelope.requested_revision = Some(SemanticRevision(self.pd_revision_seq));
        }
        envelope.target_owner_instance = self
            .photodiode
            .as_ref()
            .map(|summary| summary.owner_instance.clone());
        envelope.issued_at_unix_ms = now_unix_ms();
        PluginServiceRequest {
            request_id,
            source_plugin_id: A1_PLUGIN_ID.into(),
            target_plugin_id: PHOTODIODE_PLUGIN_ID.into(),
            service: SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1.into(),
            payload: serde_json::to_value(&envelope).unwrap_or(Value::Null),
        }
    }

    /// String metadata embedded in both recorders' own sidecars.
    fn recording_metadata(&self) -> BTreeMap<String, String> {
        let mut meta = BTreeMap::new();
        meta.insert("a1_measurement_id".into(), self.recording.id.clone());
        meta.insert("a1_stem".into(), self.recording.stem.clone());
        meta.insert("a1_role".into(), self.recording.role.label().into());
        meta.insert(
            "a1_duration_s".into(),
            self.recording.duration_s.to_string(),
        );
        meta.insert("sweep_min_a".into(), format!("{:.6}", self.min_a));
        meta.insert("sweep_max_a".into(), format!("{:.6}", self.max_a));
        if let Some(sweep) = self
            .sweep
            .as_ref()
            .filter(|sweep| sweep.phase == SweepPhase::Recording)
        {
            meta.insert(
                "sweep_requested_a".into(),
                format!("{:.6}", sweep.target_a()),
            );
            meta.insert("sweep_point_index".into(), (sweep.index + 1).to_string());
            meta.insert("sweep_point_total".into(), sweep.total().to_string());
        }
        if let Some(a) = self.measured_a() {
            meta.insert("measured_a".into(), format!("{a:.6}"));
        }
        if let Some(hz) = self.period_us().map(|t| 1_000_000.0 / t) {
            meta.insert("modulation_frequency_hz".into(), format!("{hz:.6}"));
        }
        if let Some(config) = self
            .modulation
            .as_ref()
            .and_then(|s| s.acknowledged.as_ref())
            .and_then(|t| t.a1_configuration.as_ref())
        {
            meta.insert("center_dac".into(), config.center_dac.to_string());
            meta.insert("amplitude_dac".into(), config.amplitude_dac.to_string());
        }
        if let Some(n) = self.valid_pixel_count() {
            meta.insert("n_valid".into(), n.to_string());
        }
        meta
    }

    /// Why the photodiode cannot record right now, phrased as the operator
    /// action that fixes it. `None` means the PDQ leg is expected to succeed.
    fn photodiode_blocker(&self) -> Option<String> {
        let Some(photodiode) = self.photodiode.as_ref() else {
            return Some(
                "The photodiode plugin is not reporting status — enable it before recording".into(),
            );
        };
        if !matches!(photodiode.connection, ConnectionStateV1::Connected { .. }) {
            return Some(format!(
                "The photodiode is {} — connect it before recording",
                connection_label(&photodiode.connection)
            ));
        }
        // The photodiode's own Data directory is deliberately *not* checked: A1
        // names the destination root in the start spec, so a recording started
        // here does not depend on the owner's folder setting at all.
        //
        // A lease held by anyone else means the PDQ is already committed.
        if let Some(lease) = photodiode.lease.as_ref() {
            if lease.holder.as_str() != A1_PLUGIN_ID {
                return Some(format!(
                    "The photodiode is leased by {} — release it before recording",
                    lease.holder.as_str()
                ));
            }
        }
        None
    }

    /// Kick off a coordinated recording by starting the camera first. Called
    /// on the control tick after a record button is pressed.
    fn begin_recording(&mut self, context: &mut impl RecordingControl, role: RecRole) {
        if self.recording.is_active() {
            return;
        }
        if self.output_folder.trim().is_empty() {
            self.note("Set an output folder before recording");
            return;
        }
        if self.measurement_id.trim().is_empty() {
            self.note("Set a measurement id before recording");
            return;
        }
        // Checked before the camera starts: every one of these used to surface
        // as a PDQ rejection *after* the host was already recording, which left
        // a stub RAW behind and no photodiode data.
        if let Some(blocker) = self.photodiode_blocker() {
            self.note(blocker);
            return;
        }
        let now_ms = now_unix_ms();
        let id = sanitize_stem(self.measurement_id.trim());
        // Sweep points get a stable per-point tag so the row's files sort by
        // sweep order as well as by timestamp.
        let sweep_tag = self
            .sweep
            .as_ref()
            .filter(|sweep| sweep.phase == SweepPhase::Recording)
            .map(|sweep| format!("_p{:02}", sweep.index + 1))
            .unwrap_or_default();
        let stem = format!(
            "{id}_{}{}{sweep_tag}",
            format_compact_utc(now_ms / 1_000),
            role.suffix()
        );
        let lease_id = LeaseId::new(format!("a1-{stem}"));
        self.recording_completed_ok = false;
        let mut recording = Recording::idle();
        recording.role = role;
        recording.id = id;
        recording.stem = stem;
        recording.folder = self.output_folder.trim().to_string();
        recording.duration_s = self.duration_s.max(1) as u64;
        // The measurement clock starts only after both recorders acknowledge
        // that they are running.
        recording.start_unix_ms = 0;
        recording.last_activity_ms = now_ms;
        recording.lease_id = lease_id;
        self.recording = recording;

        // Capture the science reference now (after any folder scan this tick),
        // from the live signal at the current drive amplitude.
        match role {
            RecRole::Pilot => self.freeze_pilot_windows(),
            RecRole::Background => self.capture_background_floor(),
            RecRole::Normal => {}
        }

        self.start_camera(context);
    }

    /// Re-reads pilot/background sidecars from the measurement folder when the
    /// measurement (folder + id) changes, so the `q_p` plot reuses them.
    fn scan_measurement_folder(&mut self) {
        let folder = self.output_folder.trim().to_string();
        let id = sanitize_stem(self.measurement_id.trim());
        let key = (folder.clone(), id.clone());
        if self.loaded_key.as_ref() == Some(&key) {
            return;
        }
        self.loaded_key = Some(key);
        self.pilot_windows = None;
        self.background_floor = None;
        if folder.is_empty() || id.is_empty() {
            return;
        }
        let measurement_dir = Path::new(&folder).join(&id);
        let measurement_dir = measurement_dir.to_string_lossy();
        if let Some((on, off)) = load_row_windows(&measurement_dir, &id, "_pilot") {
            self.pilot_windows = Some((on, off));
        }
        self.background_floor = load_row_background(&measurement_dir, &id, "_background");
    }

    /// Start the host camera recorder first. Starting it switches the host from
    /// preview into recording and briefly revokes plugin effects, so the PDQ
    /// stream must not be opened until the host acknowledges this transition.
    fn start_camera(&mut self, context: &mut impl RecordingControl) {
        let subdir = self.recording.id.clone();
        let stem = self.recording.stem.clone();
        let metadata = self.recording_metadata();

        let cam_req = self.next_request_id();
        context.request_host(&HostCommandRequest {
            request_id: cam_req,
            command: HostCommand::StartRecording {
                run_id: stem.clone(),
                base_path: format!("{subdir}/{stem}.raw"),
                metadata,
            },
        });
        self.recording.cam_start_req = cam_req;
        self.recording.phase = RecPhase::StartingCamera;
        self.recording.last_activity_ms = now_unix_ms();
        self.note(format!("Recording {}: starting camera…", self.recording.id));
    }

    fn connect_photodiode(&mut self, context: &mut impl RecordingControl) {
        let request = self.photodiode_request(PhotodiodeCommandV1::Connect);
        self.recording.connect_req = request.request_id;
        context.request_service(&request);
        self.recording.phase = RecPhase::ConnectingPhotodiode;
        self.recording.last_activity_ms = now_unix_ms();
        self.note(format!(
            "Recording {}: connecting photodiode…",
            self.recording.id
        ));
    }

    fn acquire_photodiode(&mut self, context: &mut impl RecordingControl) {
        let ttl_ms = self
            .recording
            .duration_s
            .saturating_mul(1_000)
            .saturating_add(60_000);
        let request = self.photodiode_request(PhotodiodeCommandV1::AcquireLease { ttl_ms });
        self.recording.lease_req = request.request_id;
        context.request_service(&request);
        self.recording.phase = RecPhase::AcquiringLease;
        self.recording.last_activity_ms = now_unix_ms();
        self.note(format!(
            "Recording {}: preparing photodiode…",
            self.recording.id
        ));
    }

    /// Start the PDQ only after the camera recorder is running.
    fn start_photodiode(&mut self, context: &mut impl RecordingControl) {
        let subdir = self.recording.id.clone();
        let stem = self.recording.stem.clone();
        let spec = PdqStartSpecV1 {
            pdq_path: format!("{subdir}/{stem}_pd.pdq"),
            sidecar_path: format!("{subdir}/{stem}_pd.json"),
            expected_sample_rate_hz: None,
            expected_stream_epoch: None,
            metadata: self.recording_metadata(),
            // Write the PDQ straight into this measurement's folder rather than
            // the photodiode's own data directory: for a recording started here,
            // this plugin's output folder is the one that decides where files go.
            root_dir: Some(self.recording.folder.clone()),
        };
        let pd_request = self.photodiode_request(PhotodiodeCommandV1::BeginRecording {
            specification: spec,
        });
        self.recording.pd_begin_req = pd_request.request_id;
        context.request_service(&pd_request);
        self.recording.phase = RecPhase::StartingPhotodiode;
        self.recording.last_activity_ms = now_unix_ms();
        self.note(format!(
            "Recording {}: starting photodiode…",
            self.recording.id
        ));
    }

    /// Atomically close the PDQ and release its lease while the camera
    /// pipeline is still live.
    fn stop_photodiode(&mut self, context: &mut impl RecordingControl) {
        if self.recording.lease_granted {
            let pd_request = self.photodiode_request(PhotodiodeCommandV1::ReleaseLease {
                finalize_recording: true,
                reason: "a1 recording complete".into(),
            });
            self.recording.pd_finalize_req = pd_request.request_id;
            context.request_service(&pd_request);
            self.recording.phase = RecPhase::StoppingPhotodiode;
        } else {
            self.stop_camera(context);
            return;
        }
        self.recording.last_activity_ms = now_unix_ms();
        self.note(format!(
            "Recording {}: saving photodiode data…",
            self.recording.id
        ));
    }

    /// The photodiode leg failed while the camera was already recording. The
    /// camera RAW is the primary measurement, so it keeps running for its full
    /// duration instead of being cut short — a truncated file that reports
    /// itself as finalized is worse than a complete camera-only one. Any lease
    /// still held is released by the normal stop path at the end.
    fn continue_without_photodiode(&mut self, context: &mut impl RecordingControl) {
        let camera_running = self.recording.cam_raw_path.is_some() && !self.recording.cam_rejected;
        if !camera_running || self.recording.stop_requested {
            self.stop_camera(context);
            return;
        }
        self.recording.phase = RecPhase::Running;
        self.recording.start_unix_ms = now_unix_ms();
        self.recording.last_activity_ms = self.recording.start_unix_ms;
        let reason = self
            .recording
            .failure
            .clone()
            .unwrap_or_else(|| "the photodiode did not start".into());
        self.note(format!(
            "{reason} — recording camera only for {} s",
            self.recording.duration_s
        ));
    }

    /// Stop the host recorder after the PDQ has been safely finalized.
    fn stop_camera(&mut self, context: &mut impl RecordingControl) {
        if self.recording.cam_raw_path.is_some() && !self.recording.cam_rejected {
            let cam_req = self.next_request_id();
            context.request_host(&HostCommandRequest {
                request_id: cam_req,
                command: HostCommand::StopRecording,
            });
            self.recording.cam_stop_req = cam_req;
            self.recording.phase = RecPhase::StoppingCamera;
            self.recording.last_activity_ms = now_unix_ms();
            self.note(format!(
                "Recording {}: saving camera data…",
                self.recording.id
            ));
        } else {
            self.finish_recording(context);
        }
    }

    fn finish_recording(&mut self, context: &mut impl RecordingControl) {
        let clean = self.recording.cam_complete
            && self.recording.pd_finalized
            && self.recording.pd_valid
            && self.recording.pd_pdq_path.is_some()
            && self.recording.pd_sidecar_path.is_some();
        // Gather the RAW/PDQ next to the sidecar before writing it, so the
        // recorded paths are the final ones.
        self.gather_into_measurement_folder();
        let sidecar = self.write_sidecar();
        let reason = self
            .recording
            .failure
            .clone()
            .unwrap_or_else(|| "not every file was finalized".into());
        let message = match (sidecar, clean) {
            (Ok(path), true) => format!("Saved recording {} → {path}", self.recording.id),
            (Ok(path), false) => format!(
                "Recording {} incomplete: {reason} — metadata saved to {path}",
                self.recording.id
            ),
            (Err(err), _) => format!(
                "Recording {} finished, metadata save failed: {err}",
                self.recording.id
            ),
        };
        self.recording_completed_ok = clean;
        self.release_and_idle(context, message);
    }

    /// Collects the finalized artifacts into `<output folder>/<measurement id>/`.
    ///
    /// The camera RAW and the PDQ are written by two other owners against their
    /// own roots — the host resolves plugin recording paths below *its* output
    /// directory and rejects absolute ones, and the photodiode resolves PDQ
    /// paths below *its* data directory. Left alone, one measurement scatters
    /// across up to three unrelated folders. Both files are closed and hashed
    /// by the time their receipts arrive, so moving them here is safe and makes
    /// this plugin's output folder authoritative for the whole measurement.
    fn gather_into_measurement_folder(&mut self) {
        let dir = PathBuf::from(&self.recording.folder).join(&self.recording.id);
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let raw = self
            .recording
            .cam_finalized_path
            .clone()
            .or_else(|| self.recording.cam_raw_path.clone());
        // The host writes the bias/config sidecar as a sibling of the RAW; it
        // travels with it so the recording stays self-describing.
        if let Some(raw) = raw {
            if let Some(moved) = move_into(&dir, &raw) {
                if self.recording.cam_finalized_path.is_some() {
                    self.recording.cam_finalized_path = Some(moved.clone());
                }
                self.recording.cam_raw_path = Some(moved);
            }
            if let Some(bias) = sibling_toml(&raw) {
                move_into(&dir, &bias);
            }
        }
        // PDQ receipts report the *label* A1 asked for, which is relative to the
        // photodiode's data directory — resolve it before touching the file, and
        // record the absolute path either way.
        if let Some(pdq) = self.resolved_photodiode_path(self.recording.pd_pdq_path.as_deref()) {
            self.recording.pd_pdq_path = Some(move_into(&dir, &pdq).unwrap_or(pdq));
        }
        if let Some(sidecar) =
            self.resolved_photodiode_path(self.recording.pd_sidecar_path.as_deref())
        {
            self.recording.pd_sidecar_path = Some(move_into(&dir, &sidecar).unwrap_or(sidecar));
        }
    }

    /// Absolute location of a photodiode-reported recording path. Receipts name
    /// the *label* A1 asked for, which is relative to whichever root the owner
    /// used: the folder A1 named in the start spec, or — for an owner too old to
    /// honour it — the owner's own data directory. Resolve against both and
    /// prefer the one that exists.
    fn resolved_photodiode_path(&self, reported: Option<&str>) -> Option<String> {
        let reported = reported?;
        let path = Path::new(reported);
        if path.is_absolute() {
            return Some(reported.to_owned());
        }
        let requested = Path::new(&self.recording.folder).join(path);
        if requested.exists() {
            return Some(requested.display().to_string());
        }
        let owner_root = self
            .photodiode
            .as_ref()
            .and_then(|photodiode| photodiode.data_dir.as_deref())
            .map(|root| Path::new(root).join(path));
        match owner_root {
            Some(owner_root) if owner_root.exists() => Some(owner_root.display().to_string()),
            _ => Some(requested.display().to_string()),
        }
    }

    /// Release the photodiode lease (only if we actually hold it) and return to idle.
    fn release_and_idle(&mut self, context: &mut impl RecordingControl, message: String) {
        if self.recording.lease_granted {
            let request = self.photodiode_request(PhotodiodeCommandV1::ReleaseLease {
                finalize_recording: true,
                reason: "a1 recording complete".into(),
            });
            context.request_service(&request);
        }
        self.recording = Recording::idle();
        self.note(message);
    }

    /// Wraps a modulation command in the routed service request A1 emits.
    fn modulation_request(
        &mut self,
        command: ModulationCommandV1,
        lease_id: &LeaseId,
    ) -> PluginServiceRequest {
        let request_id = self.next_request_id();
        let mut envelope =
            ModulationRequestV1::new(RequestId(request_id), ClientId::new(A1_PLUGIN_ID), command);
        envelope.lease_id = Some(lease_id.clone());
        envelope.target_owner_instance = self
            .modulation
            .as_ref()
            .map(|state| state.owner_instance.clone());
        envelope.issued_at_unix_ms = now_unix_ms();
        PluginServiceRequest {
            request_id,
            source_plugin_id: A1_PLUGIN_ID.into(),
            target_plugin_id: MODULATION_PLUGIN_ID.into(),
            service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
            payload: serde_json::to_value(&envelope).unwrap_or(Value::Null),
        }
    }

    fn modulation_connected(&self) -> bool {
        matches!(
            self.modulation.as_ref().map(|state| &state.connection),
            Some(ConnectionStateV1::Connected { .. })
        )
    }

    /// The requested `a` per sweep point, ascending and inclusive of both ends.
    fn sweep_points(&self) -> Vec<f64> {
        let count = self.sweep_count.clamp(2, 64) as usize;
        let span = self.max_a - self.min_a;
        (0..count)
            .map(|index| self.min_a + span * index as f64 / (count - 1) as f64)
            .collect()
    }

    /// Worst-case sweep duration, used as the modulation lease TTL.
    fn sweep_lease_ttl_ms(&self, remaining_points: usize) -> u64 {
        let per_point_ms = (self.duration_s.max(1) as u64)
            .saturating_mul(1_000)
            .saturating_add(SWEEP_SETTLE_TIMEOUT_MS)
            .saturating_add(30_000);
        (remaining_points as u64)
            .saturating_mul(per_point_ms)
            .saturating_add(60_000)
    }

    /// Kick off the amplitude sweep: validate, then lease the modulation owner.
    fn begin_sweep(&mut self, context: &mut PluginControlContext<'_>) {
        if self.recording.is_active() || self.sweep.is_some() {
            self.message = "A recording or sweep is already running".into();
            return;
        }
        if self.output_folder.trim().is_empty() {
            self.message = "Set an output folder before sweeping".into();
            return;
        }
        if self.measurement_id.trim().is_empty() {
            self.message = "Set a measurement id before sweeping".into();
            return;
        }
        if !self.modulation_connected() {
            self.message = "Modulation owner is not connected — cannot sweep".into();
            return;
        }
        if self.min_a.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            self.message =
                "Set Sweep min a > 0 (a = 0 is the background reference, not a sweep point)".into();
            return;
        }
        if self.max_a.partial_cmp(&self.min_a) != Some(std::cmp::Ordering::Greater) {
            self.message = "Sweep needs max a > min a".into();
            return;
        }
        let points = self.sweep_points();
        let now_ms = now_unix_ms();
        let lease_id = LeaseId::new(format!("a1-sweep-{}", format_compact_utc(now_ms / 1_000)));
        let ttl_ms = self.sweep_lease_ttl_ms(points.len());
        let request =
            self.modulation_request(ModulationCommandV1::AcquireLease { ttl_ms }, &lease_id);
        let lease_req = request.request_id;
        let _ = context.request_service(&request);
        let total = points.len();
        self.sweep = Some(Sweep {
            phase: SweepPhase::AcquiringLease,
            points,
            index: 0,
            lease_id,
            lease_granted: false,
            lease_req,
            depth_req: 0,
            depth_applied: false,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: false,
            last_activity_ms: now_ms,
            stop_requested: false,
        });
        self.message = format!("Sweep: acquiring modulation lease for {total} points…");
    }

    /// Release the modulation lease (if held) and clear the sweep.
    fn finish_sweep(&mut self, context: &mut PluginControlContext<'_>, message: String) {
        if let Some(sweep) = self.sweep.take() {
            if sweep.lease_granted {
                let request = self.modulation_request(
                    ModulationCommandV1::ReleaseLease {
                        safe_off: false,
                        reason: "a1 sweep finished".into(),
                    },
                    &sweep.lease_id,
                );
                let _ = context.request_service(&request);
            }
        }
        self.message = message;
    }

    /// Renew the modulation lease and retarget the drive at the current point.
    fn send_sweep_depth(&mut self, context: &mut PluginControlContext<'_>) {
        let Some(sweep) = self.sweep.as_ref() else {
            return;
        };
        let lease_id = sweep.lease_id.clone();
        let remaining = sweep.total().saturating_sub(sweep.index);
        let target_a = sweep.target_a();
        let index = sweep.index;
        let total = sweep.total();

        let ttl_ms = self.sweep_lease_ttl_ms(remaining);
        let renew = self.modulation_request(ModulationCommandV1::RenewLease { ttl_ms }, &lease_id);
        let _ = context.request_service(&renew);

        let depth = self.modulation_request(
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: (target_a * 1_000.0).round().clamp(0.0, u32::MAX as f64) as u32,
            },
            &lease_id,
        );
        let depth_req = depth.request_id;
        let _ = context.request_service(&depth);

        let now_ms = now_unix_ms();
        if let Some(sweep) = self.sweep.as_mut() {
            sweep.phase = SweepPhase::SettingDepth;
            sweep.depth_req = depth_req;
            sweep.depth_applied = false;
            sweep.settled_since_ms = None;
            sweep.point_started = false;
            sweep.last_activity_ms = now_ms;
        }
        self.message = format!(
            "Sweep point {}/{}: retargeting drive to a = {:.3}…",
            index + 1,
            total,
            target_a
        );
    }

    /// Advance the amplitude sweep one control tick. Runs before
    /// `drive_recording`, so a point's recording starts on the same tick.
    fn drive_sweep(&mut self, context: &mut PluginControlContext<'_>) {
        if self.sweep.is_none() {
            if std::mem::take(&mut self.sweep_pending) {
                self.begin_sweep(context);
            }
            return;
        }
        self.sweep_pending = false;
        let now_ms = now_unix_ms();
        let (phase, stop_requested, lease_granted, depth_applied, last_activity_ms, index, total) = {
            let sweep = self.sweep.as_ref().expect("sweep checked above");
            (
                sweep.phase,
                sweep.stop_requested,
                sweep.lease_granted,
                sweep.depth_applied,
                sweep.last_activity_ms,
                sweep.index,
                sweep.total(),
            )
        };
        if stop_requested && phase != SweepPhase::Recording {
            let message = if self.message.is_empty() {
                "Sweep stopped".into()
            } else {
                self.message.clone()
            };
            self.finish_sweep(context, message);
            return;
        }
        match phase {
            SweepPhase::AcquiringLease => {
                if lease_granted {
                    self.send_sweep_depth(context);
                } else if now_ms.saturating_sub(last_activity_ms) > REPLY_TIMEOUT_MS {
                    self.finish_sweep(
                        context,
                        "Sweep aborted: timed out acquiring the modulation lease".into(),
                    );
                }
            }
            SweepPhase::SettingDepth => {
                if depth_applied {
                    let target = self
                        .sweep
                        .as_mut()
                        .map(|sweep| {
                            sweep.phase = SweepPhase::Settling;
                            sweep.settled_since_ms = None;
                            sweep.settle_deadline_ms = now_ms + SWEEP_SETTLE_TIMEOUT_MS;
                            sweep.target_a()
                        })
                        .unwrap_or_default();
                    self.message = format!(
                        "Sweep point {}/{}: waiting for a to settle at {target:.3}…",
                        index + 1,
                        total,
                    );
                } else if now_ms.saturating_sub(last_activity_ms) > REPLY_TIMEOUT_MS {
                    self.finish_sweep(
                        context,
                        "Sweep aborted: timed out retargeting the modulation drive".into(),
                    );
                }
            }
            SweepPhase::Settling => {
                let target = self.sweep.as_ref().map(Sweep::target_a).unwrap_or_default();
                let settled = self
                    .measured_a()
                    .is_some_and(|measured| (measured - target).abs() <= sweep_tolerance(target));
                let dwell_ms = (self.settle_s.max(0.0) * 1_000.0) as u64;
                let mut start_recording = false;
                let mut settle_timed_out = false;
                if let Some(sweep) = self.sweep.as_mut() {
                    if settled {
                        let since = *sweep.settled_since_ms.get_or_insert(now_ms);
                        if now_ms.saturating_sub(since) >= dwell_ms {
                            start_recording = true;
                        }
                    } else {
                        sweep.settled_since_ms = None;
                    }
                    if !start_recording && now_ms >= sweep.settle_deadline_ms {
                        // Record anyway: the sidecar stores the *measured* a,
                        // so an unsettled point is still a usable sample.
                        start_recording = true;
                        settle_timed_out = true;
                    }
                    if start_recording {
                        sweep.phase = SweepPhase::Recording;
                    }
                }
                if start_recording {
                    self.pending_role = Some(RecRole::Normal);
                    if settle_timed_out {
                        self.message = format!(
                            "Sweep point {}/{}: a did not settle at {target:.3} — recording anyway",
                            index + 1,
                            total,
                        );
                    }
                }
            }
            SweepPhase::Recording => {
                if self.pending_role.is_some() || self.recording.is_active() {
                    if self.recording.is_active() {
                        if let Some(sweep) = self.sweep.as_mut() {
                            sweep.point_started = true;
                        }
                        if stop_requested {
                            self.recording.stop_requested = true;
                        }
                    }
                    return;
                }
                // The recording coordinator is idle again: the point either
                // finished, failed, or was refused before starting.
                let point_started = self.sweep.as_ref().is_some_and(|sweep| sweep.point_started);
                if stop_requested {
                    let message = self.message.clone();
                    self.finish_sweep(context, message);
                } else if !point_started || !self.recording_completed_ok {
                    let message = format!("Sweep aborted: {}", self.message);
                    self.finish_sweep(context, message);
                } else if index + 1 >= total {
                    self.finish_sweep(context, format!("Sweep complete: {total} points recorded"));
                } else {
                    if let Some(sweep) = self.sweep.as_mut() {
                        sweep.index += 1;
                    }
                    self.send_sweep_depth(context);
                }
            }
        }
    }

    /// Routes modulation-service replies belonging to the sweep. Returns true
    /// when the reply was consumed.
    fn on_sweep_reply(&mut self, reply: &PluginServiceReply) -> bool {
        let Some((lease_req, depth_req)) = self
            .sweep
            .as_ref()
            .map(|sweep| (sweep.lease_req, sweep.depth_req))
        else {
            return false;
        };
        let abort = |this: &mut Self, message: String| {
            this.message = message;
            if let Some(sweep) = this.sweep.as_mut() {
                sweep.stop_requested = true;
            }
        };
        if reply.request_id == lease_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(sweep) = self.sweep.as_mut() {
                        sweep.lease_granted = true;
                        sweep.last_activity_ms = now_unix_ms();
                    }
                }
                PluginServiceOutcome::Rejected { message, .. } => {
                    abort(
                        self,
                        format!("Sweep aborted: modulation lease rejected: {message}"),
                    );
                }
            }
            true
        } else if reply.request_id == depth_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(sweep) = self.sweep.as_mut() {
                        sweep.depth_applied = true;
                        sweep.last_activity_ms = now_unix_ms();
                    }
                }
                PluginServiceOutcome::Rejected { message, .. } => {
                    abort(
                        self,
                        format!("Sweep aborted: drive retarget rejected: {message}"),
                    );
                }
            }
            true
        } else {
            false
        }
    }

    fn on_host_reply(&mut self, reply: &HostCommandReply) {
        if reply.request_id == self.recording.cam_start_req {
            match &reply.outcome {
                HostCommandOutcome::RecordingStarted {
                    actual_raw_path, ..
                } => {
                    self.recording.cam_raw_path = Some(actual_raw_path.clone());
                    self.recording.last_activity_ms = now_unix_ms();
                }
                HostCommandOutcome::Rejected { code, message } => {
                    // Stop the rest of the recording; drive_recording resolves the
                    // abort from the current phase on the next tick.
                    self.note_failure(format!("Camera recording rejected ({code}): {message}"));
                    self.recording.cam_rejected = true;
                    self.recording.stop_requested = true;
                }
                _ => {}
            }
        } else if reply.request_id == self.recording.cam_stop_req {
            match &reply.outcome {
                HostCommandOutcome::RecordingFinalized {
                    actual_raw_path, ..
                } => {
                    self.recording.cam_finalized_path = Some(actual_raw_path.clone());
                    self.recording.cam_complete = true;
                    self.recording.last_activity_ms = now_unix_ms();
                }
                HostCommandOutcome::RecordingPartial {
                    actual_raw_path, ..
                } => {
                    self.recording.cam_finalized_path = Some(actual_raw_path.clone());
                    self.recording.last_activity_ms = now_unix_ms();
                }
                HostCommandOutcome::Rejected { code, message } => {
                    self.note_failure(format!("Camera stop failed ({code}): {message}"));
                    self.recording.cam_rejected = true;
                    self.recording.last_activity_ms = now_unix_ms();
                }
                _ => {}
            }
        }
    }

    fn on_service_reply(&mut self, reply: &PluginServiceReply) {
        if self.on_sweep_reply(reply) {
            return;
        }
        let response = match &reply.outcome {
            PluginServiceOutcome::Accepted { payload } => {
                serde_json::from_value::<PhotodiodeResponseV1>(payload.clone()).ok()
            }
            PluginServiceOutcome::Rejected { code, message } => {
                if reply.request_id == self.recording.connect_req
                    || reply.request_id == self.recording.lease_req
                    || reply.request_id == self.recording.pd_begin_req
                {
                    // Not `stop_requested`: that flag means the operator asked
                    // to stop. A photodiode fault leaves the camera running to
                    // its full duration (see `continue_without_photodiode`).
                    self.note_failure(format!("Photodiode start failed ({code}): {message}"));
                    self.recording.pd_rejected = true;
                } else if reply.request_id == self.recording.pd_finalize_req {
                    self.note_failure(format!("Photodiode save failed ({code}): {message}"));
                    self.recording.pd_rejected = true;
                    self.recording.lease_granted = false;
                    self.recording.last_activity_ms = now_unix_ms();
                }
                None
            }
        };
        let Some(response) = response else {
            return;
        };
        if reply.request_id == self.recording.connect_req {
            self.recording.connect_accepted = true;
            self.recording.last_activity_ms = now_unix_ms();
        } else if reply.request_id == self.recording.lease_req {
            self.recording.lease_granted = true;
            self.recording.last_activity_ms = now_unix_ms();
        } else if reply.request_id == self.recording.pd_begin_req {
            if let Some(PdqReceiptV1::Started(started)) = &response.receipt {
                self.recording.pd_pdq_path = Some(started.pdq_path.clone());
                self.recording.pd_sidecar_path = Some(started.sidecar_path.clone());
                self.recording.last_activity_ms = now_unix_ms();
            }
        } else if reply.request_id == self.recording.pd_finalize_req {
            self.recording.pd_finalized = true;
            if let Some(PdqReceiptV1::Finalized(finalized)) = &response.receipt {
                self.recording.pd_pdq_path = Some(finalized.pdq_path.clone());
                self.recording.pd_sidecar_path = Some(finalized.sidecar_path.clone());
                self.recording.pd_valid = finalized.valid;
            }
            self.recording.lease_granted = false;
            self.recording.last_activity_ms = now_unix_ms();
        }
    }

    /// Advance the recording state machine one control tick.
    fn drive_recording(&mut self, context: &mut impl RecordingControl) {
        let now_ms = now_unix_ms();
        match self.recording.phase {
            RecPhase::Idle => {
                if let Some(role) = self.pending_role.take() {
                    self.begin_recording(context, role);
                }
            }
            RecPhase::StartingCamera => {
                if self.recording.cam_rejected {
                    let message = self.message.clone();
                    self.release_and_idle(context, message);
                } else if self.recording.cam_raw_path.is_some() {
                    if self.recording.stop_requested {
                        self.stop_camera(context);
                    } else {
                        self.connect_photodiode(context);
                    }
                } else if now_ms.saturating_sub(self.recording.last_activity_ms) > REPLY_TIMEOUT_MS
                {
                    self.recording.cam_rejected = true;
                    self.note_failure("Timed out starting camera recording");
                    let message = self.message.clone();
                    self.release_and_idle(context, message);
                }
            }
            RecPhase::ConnectingPhotodiode => {
                if self.recording.pd_rejected {
                    self.continue_without_photodiode(context);
                } else if self.recording.stop_requested && !self.recording.connect_accepted {
                    self.stop_camera(context);
                } else if self.recording.connect_accepted {
                    if self.recording.stop_requested {
                        self.stop_camera(context);
                    } else {
                        self.acquire_photodiode(context);
                    }
                } else if now_ms.saturating_sub(self.recording.last_activity_ms) > REPLY_TIMEOUT_MS
                {
                    self.recording.pd_rejected = true;
                    self.note_failure("Timed out connecting the photodiode");
                    self.continue_without_photodiode(context);
                }
            }
            RecPhase::AcquiringLease => {
                if self.recording.pd_rejected {
                    self.continue_without_photodiode(context);
                } else if self.recording.lease_granted {
                    if self.recording.stop_requested {
                        self.stop_photodiode(context);
                    } else {
                        self.start_photodiode(context);
                    }
                } else if now_ms.saturating_sub(self.recording.last_activity_ms) > REPLY_TIMEOUT_MS
                {
                    self.recording.pd_rejected = true;
                    self.note_failure("Timed out preparing the photodiode");
                    self.continue_without_photodiode(context);
                }
            }
            RecPhase::StartingPhotodiode => {
                if self.recording.pd_pdq_path.is_some() && self.recording.pd_sidecar_path.is_some()
                {
                    self.recording.phase = RecPhase::Running;
                    self.recording.start_unix_ms = now_ms;
                    if self.recording.stop_requested {
                        self.stop_photodiode(context);
                    } else {
                        self.note(format!(
                            "Recording {} for {} s…",
                            self.recording.id, self.recording.duration_s
                        ));
                    }
                } else if self.recording.pd_rejected
                    || now_ms.saturating_sub(self.recording.last_activity_ms) > REPLY_TIMEOUT_MS
                {
                    self.recording.pd_rejected = true;
                    self.note_failure("The photodiode did not open its PDQ file");
                    self.continue_without_photodiode(context);
                }
            }
            RecPhase::Running => {
                let elapsed_ms = now_ms.saturating_sub(self.recording.start_unix_ms);
                let over = elapsed_ms >= self.recording.duration_s.saturating_mul(1_000);
                if over || self.recording.stop_requested {
                    self.stop_photodiode(context);
                }
            }
            RecPhase::StoppingPhotodiode => {
                if self.recording.pd_finalized || self.recording.pd_rejected {
                    self.stop_camera(context);
                } else if now_ms.saturating_sub(self.recording.last_activity_ms) > REPLY_TIMEOUT_MS
                {
                    self.recording.pd_rejected = true;
                    self.recording.lease_granted = false;
                    self.note_failure("Timed out saving photodiode data");
                    self.stop_camera(context);
                }
            }
            RecPhase::StoppingCamera => {
                if self.recording.cam_finalized_path.is_some()
                    || self.recording.cam_rejected
                    || now_ms.saturating_sub(self.recording.last_activity_ms) > REPLY_TIMEOUT_MS
                {
                    if self.recording.cam_finalized_path.is_none() && !self.recording.cam_rejected {
                        self.recording.cam_rejected = true;
                        self.note_failure("Timed out saving camera data");
                    }
                    self.finish_recording(context);
                }
            }
        }
    }

    /// Build and write the A1 config sidecar linking the RAW + PDQ files.
    fn write_sidecar(&self) -> Result<String, String> {
        let now_ms = now_unix_ms();
        let modulation = self
            .modulation
            .as_ref()
            .and_then(|s| s.acknowledged.as_ref());
        let a1_config = modulation.and_then(|t| t.a1_configuration.as_ref());
        let optical = self
            .photodiode
            .as_ref()
            .and_then(|s| s.optical_summary.as_ref());
        let roi = self.host_roi.unwrap_or_default();

        let raw_path = self
            .recording
            .cam_finalized_path
            .clone()
            .or_else(|| self.recording.cam_raw_path.clone());
        let camera_bias_sidecar = raw_path.as_deref().and_then(sibling_toml);

        let doc = SidecarDoc {
            measurement_id: self.recording.id.clone(),
            file_stem: self.recording.stem.clone(),
            role: self.recording.role.label().into(),
            recorded_at_utc: format_iso_utc(
                if self.recording.start_unix_ms == 0 {
                    now_ms
                } else {
                    self.recording.start_unix_ms
                } / 1_000,
            ),
            finalized_at_utc: format_iso_utc(now_ms / 1_000),
            duration_s: self.recording.duration_s,
            sweep: {
                let point = self
                    .sweep
                    .as_ref()
                    .filter(|sweep| sweep.phase == SweepPhase::Recording);
                SweepSidecar {
                    min_a: self.min_a,
                    max_a: self.max_a,
                    requested_a: point.map(Sweep::target_a),
                    point_index: point.map(|sweep| sweep.index + 1),
                    point_total: point.map(Sweep::total),
                }
            },
            pilot: (self.recording.role == RecRole::Pilot)
                .then_some(self.pilot_windows)
                .flatten()
                .map(|(on, off)| PilotSidecar {
                    window_on_start: on.start,
                    window_on_end: on.end,
                    window_off_start: off.start,
                    window_off_end: off.end,
                }),
            background: (self.recording.role == RecRole::Background)
                .then_some(self.background_floor)
                .flatten()
                .map(|(q_on, q_off)| BackgroundSidecar { q_on, q_off }),
            modulation: ModulationSidecar {
                frequency_hz: self.period_us().map(|t| 1_000_000.0 / t),
                frequency_source: self.frequency_source().into(),
                center_dac: a1_config.map(|c| c.center_dac),
                amplitude_dac: a1_config.map(|c| c.amplitude_dac),
                waveform: modulation
                    .and_then(|t| t.waveform.as_ref())
                    .map(waveform_label),
            },
            optical: OpticalSidecar {
                measured_a: optical.map(|o| o.measured_log_contrast),
                low_clip_fraction: optical.map(|o| o.low_clip_fraction),
                high_clip_fraction: optical.map(|o| o.high_clip_fraction),
                measured_frequency_hz: optical.and_then(|o| o.measured_frequency_hz),
            },
            camera: CameraSidecar {
                roi_x: roi.x,
                roi_y: roi.y,
                roi_width: roi.width,
                roi_height: roi.height,
                masked_pixels: self.masked_pixels.len(),
                n_valid: self.valid_pixel_count(),
            },
            trigger: TriggerSidecar {
                marker_anchored: self.is_marker_anchored(),
                marker_count: self.camera_markers_us.len(),
                measured_period_us: self.measured_period_us(),
            },
            files: FilesSidecar {
                camera_raw: raw_path,
                camera_config_sidecar: camera_bias_sidecar,
                photodiode_pdq: self.recording.pd_pdq_path.clone(),
                photodiode_sidecar: self.recording.pd_sidecar_path.clone(),
            },
        };

        let toml = toml::to_string_pretty(&doc).map_err(|err| err.to_string())?;
        let dir = PathBuf::from(&self.recording.folder).join(&self.recording.id);
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let path = dir.join(format!("{}_config.toml", self.recording.stem));
        std::fs::write(&path, toml).map_err(|err| err.to_string())?;
        Ok(path.display().to_string())
    }
}

// ---- sidecar document ------------------------------------------------------

#[derive(Serialize)]
struct SidecarDoc {
    measurement_id: String,
    file_stem: String,
    role: String,
    recorded_at_utc: String,
    finalized_at_utc: String,
    duration_s: u64,
    sweep: SweepSidecar,
    #[serde(skip_serializing_if = "Option::is_none")]
    pilot: Option<PilotSidecar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<BackgroundSidecar>,
    modulation: ModulationSidecar,
    optical: OpticalSidecar,
    camera: CameraSidecar,
    trigger: TriggerSidecar,
    files: FilesSidecar,
}

#[derive(Serialize)]
struct SweepSidecar {
    min_a: f64,
    max_a: f64,
    /// The `a` this sweep point asked the drive for (measured `a` is in
    /// `[optical]`); absent on manual recordings.
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_a: Option<f64>,
    /// 1-based point position within the sweep; absent on manual recordings.
    #[serde(skip_serializing_if = "Option::is_none")]
    point_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_total: Option<usize>,
}

/// Frozen ON/OFF windows written into a **pilot** recording's sidecar and read
/// back to reuse them across the row.
#[derive(Serialize, serde::Deserialize)]
struct PilotSidecar {
    window_on_start: f64,
    window_on_end: f64,
    window_off_start: f64,
    window_off_end: f64,
}

/// False-response floor written into a **background** recording's sidecar.
#[derive(Serialize, serde::Deserialize)]
struct BackgroundSidecar {
    q_on: f64,
    q_off: f64,
}

/// Partial view of a config sidecar for reading the pilot/background sections
/// back; every other section is ignored.
#[derive(serde::Deserialize)]
struct RowSidecar {
    #[serde(default)]
    pilot: Option<PilotSidecar>,
    #[serde(default)]
    background: Option<BackgroundSidecar>,
}

#[derive(Serialize)]
struct ModulationSidecar {
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_hz: Option<f64>,
    frequency_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    center_dac: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amplitude_dac: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    waveform: Option<String>,
}

#[derive(Serialize)]
struct OpticalSidecar {
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_a: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    low_clip_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    high_clip_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_frequency_hz: Option<f64>,
}

#[derive(Serialize)]
struct CameraSidecar {
    roi_x: u16,
    roi_y: u16,
    roi_width: u16,
    roi_height: u16,
    masked_pixels: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    n_valid: Option<usize>,
}

#[derive(Serialize)]
struct TriggerSidecar {
    marker_anchored: bool,
    marker_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_period_us: Option<f64>,
}

#[derive(Serialize)]
struct FilesSidecar {
    #[serde(skip_serializing_if = "Option::is_none")]
    camera_raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    camera_config_sidecar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    photodiode_pdq: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    photodiode_sidecar: Option<String>,
}

// ---- free functions --------------------------------------------------------

fn ffi_to_camera_event(event: &FfiCdEvent) -> CameraEvent {
    CameraEvent {
        x: event.x,
        y: event.y,
        timestamp_us: event.timestamp_us(),
        polarity: if event.is_on() {
            Polarity::On
        } else {
            Polarity::Off
        },
    }
}

fn points_for(points: &[RollingResponsePoint], first: u64) -> Vec<Series1dPoint> {
    points
        .iter()
        .map(|point| Series1dPoint {
            x: point.timestamp_us.saturating_sub(first) as f64 / 1_000_000.0,
            y: point.run_per_pixel,
        })
        .collect()
}

fn waveform_label(waveform: &WaveformV1) -> String {
    match waveform {
        WaveformV1::Off => "off".into(),
        WaveformV1::Constant { level_dac } => format!("constant({level_dac})"),
        WaveformV1::Periodic {
            waveform,
            min_dac,
            max_dac,
            frequency_millihz,
        } => format!(
            "periodic({waveform:?}, {min_dac}..{max_dac}, {:.3} Hz)",
            *frequency_millihz as f64 / 1_000.0
        ),
    }
}

/// Moves `source` into `dir`, returning the new path when it now lives there.
///
/// A rename covers the common case (one volume) at zero cost; a cross-volume
/// move falls back to copy-then-delete, and the copy is size-checked before the
/// original goes away so a failed move never loses measurement data. `None`
/// means the file stayed where it was — callers keep the original path.
fn move_into(dir: &Path, source: &str) -> Option<String> {
    let source = Path::new(source);
    let name = source.file_name()?;
    if source.parent() == Some(dir) {
        return None;
    }
    if !source.is_file() {
        return None;
    }
    let destination = dir.join(name);
    if destination.exists() {
        return None;
    }
    if std::fs::rename(source, &destination).is_ok() {
        return Some(destination.display().to_string());
    }
    let copied = std::fs::copy(source, &destination).ok()?;
    let expected = source.metadata().ok()?.len();
    if copied != expected {
        let _ = std::fs::remove_file(&destination);
        return None;
    }
    // Keeping the original after a verified copy is harmless; losing it is not.
    let _ = std::fs::remove_file(source);
    Some(destination.display().to_string())
}

fn sibling_toml(raw_path: &str) -> Option<String> {
    let path = Path::new(raw_path);
    let stem = path.file_stem()?.to_string_lossy();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    Some(parent.join(format!("{stem}.toml")).display().to_string())
}

/// Parses the newest config sidecar in `folder` for measurement `id` whose stem
/// carries `role_tag` (e.g. `_pilot`). Filenames embed a sortable timestamp, so
/// the lexicographically largest matching name is the most recent.
fn load_row_sidecar(folder: &str, id: &str, role_tag: &str) -> Option<RowSidecar> {
    let prefix = format!("{id}_");
    let mut best: Option<String> = None;
    for entry in std::fs::read_dir(folder).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix)
            && name.contains(role_tag)
            && name.ends_with("_config.toml")
            && best.as_ref().is_none_or(|current| name > *current)
        {
            best = Some(name);
        }
    }
    let text = std::fs::read_to_string(Path::new(folder).join(best?)).ok()?;
    toml::from_str::<RowSidecar>(&text).ok()
}

fn load_row_windows(folder: &str, id: &str, role_tag: &str) -> Option<(PhaseWindow, PhaseWindow)> {
    let pilot = load_row_sidecar(folder, id, role_tag)?.pilot?;
    Some((
        PhaseWindow {
            start: pilot.window_on_start,
            end: pilot.window_on_end,
        },
        PhaseWindow {
            start: pilot.window_off_start,
            end: pilot.window_off_end,
        },
    ))
}

fn load_row_background(folder: &str, id: &str, role_tag: &str) -> Option<(f64, f64)> {
    let background = load_row_sidecar(folder, id, role_tag)?.background?;
    Some((background.q_on, background.q_off))
}

/// Replace anything that is not `[A-Za-z0-9._-]` with `_` so ids are file-safe.
fn sanitize_stem(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "A1".into()
    } else {
        trimmed
    }
}

fn generate_measurement_id() -> String {
    let ms = now_unix_ms();
    format!(
        "A1-{}-{:04x}",
        format_compact_date(ms / 1_000),
        (ms & 0xffff)
    )
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Gregorian date for a count of days since the Unix epoch (Howard Hinnant's
/// civil-from-days algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (year + i64::from(month <= 2), month, day)
}

fn ymd_hms(unix_secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = (unix_secs / 86_400) as i64;
    let sod = unix_secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    (y, m, d, sod / 3_600, (sod % 3_600) / 60, sod % 60)
}

fn format_compact_date(unix_secs: u64) -> String {
    let (y, m, d, ..) = ymd_hms(unix_secs);
    format!("{y:04}{m:02}{d:02}")
}

fn format_compact_utc(unix_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = ymd_hms(unix_secs);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

fn format_iso_utc(unix_secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = ymd_hms(unix_secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

impl Plugin for StageAA1Plugin {
    fn name(&self) -> &'static str {
        "Stage-A A1 Analysis"
    }

    fn description(&self) -> &'static str {
        "Stage-A A1 recording coordinator: one-button synchronized camera RAW + photodiode PDQ recording with a config sidecar, plus live rolling-response and response-probability quicklooks."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn set_runtime_role(&mut self, role: PluginRuntimeRole) {
        self.runtime_role = role;
    }

    fn reset(&mut self) {
        self.camera_events.clear();
        self.event_scratch.clear();
        self.camera_markers_us.clear();
        self.valid_pixels = 0;
        self.response_points.clear();
        self.pilot_windows = None;
        self.background_floor = None;
        self.loaded_key = None;
        self.bump();
    }

    fn on_discontinuity(&mut self, reason: PluginDiscontinuity) {
        match reason {
            // The host raises SettingsChanged on *every* settings sync of any
            // plugin (including our own button presses). The fold window
            // rebuilds itself each frame, and the response curve, pilot
            // windows, and background floor are operator-owned science state —
            // wiping them here made "Record point" appear dead.
            PluginDiscontinuity::SettingsChanged => {}
            PluginDiscontinuity::Seek
            | PluginDiscontinuity::SourceChanged
            | PluginDiscontinuity::HistoryEvicted => {
                // Starting and stopping the host recorder restarts the capture
                // pipeline, and the host reports that as SourceChanged. Those
                // boundaries are self-inflicted — twice per recording — so they
                // must not wipe the row's pilot windows, background floor, or
                // the response points collected across a sweep. The event fold
                // still resets: that timeline really did restart.
                if self.recording.is_active() || self.sweep.is_some() {
                    self.camera_events.clear();
                    self.event_scratch.clear();
                    self.camera_markers_us.clear();
                    self.bump();
                } else {
                    self.reset();
                }
            }
        }
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::RawEvents
    }

    fn capabilities(&self) -> PluginCapabilities {
        // Request retained event history so the analysis window comes exactly
        // from the EventStore rather than best-effort preview frames.
        PluginCapabilities {
            retained_event_history: true,
        }
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        event_store: &EventStoreHandle<'_>,
    ) {
        self.frame_width = frame.width();
        self.frame_height = frame.height();
        // ROI and masked pixels are owned by the host camera config, not the
        // plugin; mirror the latest snapshot each frame.
        if let Some(settings) = context
            .get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)
            .ok()
            .flatten()
        {
            self.host_roi = Some(settings.roi);
            self.masked_pixels = settings.masked_pixels.into_iter().collect();
        }
        if !self.live {
            return;
        }
        self.valid_pixels = usize::from(frame.width()) * usize::from(frame.height());

        // Markers (phase-0 sync) only exist on the preview frame, so accumulate
        // the rising EXT_TRIGGER edges here regardless of the event source.
        // Preview windows overlap, so the same trigger arrives on several
        // consecutive frames — dedup after every merge or the duplicate
        // timestamps fail marker validation and blank the fold.
        self.camera_markers_us.extend(
            frame
                .external_triggers()
                .iter()
                .filter(|trigger| trigger.is_rising())
                .map(|trigger| trigger.timestamp_us),
        );
        self.camera_markers_us.sort_unstable();
        self.camera_markers_us.dedup();
        if self.camera_markers_us.len() > MAX_MARKERS {
            let excess = self.camera_markers_us.len() - MAX_MARKERS;
            self.camera_markers_us.drain(..excess);
        }

        let window_end = frame.window_end_us();
        let window_us = (self.analysis_window_ms.max(1) as u64).saturating_mul(1_000);
        if event_store.frame_count() > 0 {
            // Exact path: rebuild the analysis window from the retained event
            // history, immune to dropped preview frames.
            let window_start = window_end
                .saturating_sub(window_us)
                .max(event_store.oldest_timestamp_us());
            self.event_scratch.clear();
            event_store.collect_events_in_range(window_start, window_end, &mut self.event_scratch);
            self.camera_events.clear();
            self.camera_events
                .extend(self.event_scratch.iter().map(ffi_to_camera_event));
            // Keep the marker set on the same window as the events.
            self.camera_markers_us
                .retain(|&marker| marker >= window_start);
        } else if self.camera_events.len() < MAX_EVENTS {
            // Fallback (no retained history available): accumulate the
            // best-effort preview-frame events.
            self.camera_events
                .extend(frame.events().iter().map(ffi_to_camera_event));
        }
        self.bump();
    }

    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        let inbox = context.inbox().clone();
        self.update_snapshots(&inbox);
        for reply in &inbox.host_replies {
            self.on_host_reply(reply);
        }
        for reply in &inbox.service_replies {
            self.on_service_reply(reply);
        }
        // Reuse the pilot/background captured for this measurement when idle: when
        // the folder or id changes, look them up in the folder.
        if !self.recording.is_active() {
            self.scan_measurement_folder();
        }
        // The sweep runs first so a point's recording starts on the same tick.
        self.drive_sweep(context);
        self.drive_recording(context);
        // The fold reflects live snapshots (T, a) even between frames.
        self.bump();
    }

    fn settings_schema(&self) -> SettingsSchema {
        // The record/sweep buttons stay disabled until the recording has a
        // destination, instead of failing with a status message after a click.
        let can_record = !self.output_folder.trim().is_empty();
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Recording".into(),
                    description: Some(
                        "Records the camera RAW stream and the photodiode PDQ stream together for \
                         a fixed duration and writes an A1 config sidecar (.toml) linking them. \
                         Everything lands under <output folder>/<measurement id>/ and shares an \
                         <id>_<timestamp> stem: the RAW and PDQ are gathered here once both are \
                         finalized, wherever their own recorders wrote them. Arm the optical \
                         drive in the modulation plugin first; A1 only reads its settings — it \
                         never drives the Teensy. The photodiode must be connected and have a \
                         data directory set, otherwise the recording is refused before it starts."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "output_folder".into(),
                            label: "Output folder".into(),
                            tooltip: Some(
                                "Experiment directory for this measurement. The config sidecar is \
                                 written here, and the camera RAW and photodiode PDQ are moved \
                                 here once finalized, so one measurement is one folder."
                                    .into(),
                            ),
                            kind: SettingKind::Path {
                                dialog: PathDialogKind::Directory,
                                default: self.output_folder.clone(),
                            },
                        },
                        SettingItem {
                            key: "measurement_id".into(),
                            label: "Measurement id (one per I_k, f pair)".into(),
                            tooltip: Some(
                                "Groups every repeat of one illumination/frequency pair. Included \
                                 in every file name. Edit it freely or press New id."
                                    .into(),
                            ),
                            kind: SettingKind::Text {
                                default: self.measurement_id.clone(),
                            },
                        },
                        SettingItem {
                            key: "new_id".into(),
                            label: "New id".into(),
                            tooltip: Some("Generate a fresh default measurement id.".into()),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "min_a".into(),
                            label: "Sweep min a".into(),
                            tooltip: Some(
                                "Low end of the modulation-depth sweep for this (I_k, f) row. \
                                 Stored in every sidecar as the automation template; A1 does not \
                                 drive it — you set the drive in the modulation plugin."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: 10.0,
                                speed: 0.01,
                                default: self.min_a,
                            },
                        },
                        SettingItem {
                            key: "max_a".into(),
                            label: "Sweep max a".into(),
                            tooltip: Some(
                                "High end of the modulation-depth sweep for this (I_k, f) row \
                                 (also the natural amplitude for the pilot). Stored in every \
                                 sidecar; A1 does not drive it."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: 10.0,
                                speed: 0.01,
                                default: self.max_a,
                            },
                        },
                        SettingItem {
                            key: "sweep_count".into(),
                            label: "Sweep points (count)".into(),
                            tooltip: Some(
                                "How many amplitudes the Start sweep button records, spaced \
                                 evenly from Sweep min a to Sweep max a (inclusive)."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 2,
                                max: 64,
                                default: self.sweep_count,
                            },
                        },
                        SettingItem {
                            key: "settle_s".into(),
                            label: "Sweep settle (s)".into(),
                            tooltip: Some(
                                "After retargeting the drive, the sweep waits until the \
                                 photodiode-measured a holds the target (±10 %, at least ±0.05) \
                                 for this long before recording. Gives up after 30 s and records \
                                 anyway — the sidecar stores the measured a."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: 60.0,
                                speed: 0.1,
                                default: self.settle_s,
                            },
                        },
                        SettingItem {
                            key: "duration_s".into(),
                            label: "Duration (s)".into(),
                            tooltip: Some(
                                "How long each recording runs before it auto-stops and finalizes."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 1,
                                max: 3_600,
                                default: self.duration_s,
                            },
                        },
                        SettingItem {
                            key: "start_recording".into(),
                            label: "Start recording (sweep point)".into(),
                            tooltip: Some(
                                "Acquire the photodiode lease, start the camera RAW + photodiode \
                                 PDQ recording, auto-stop after the duration, and write the \
                                 sidecar. Disabled until an output folder is selected."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "start_sweep".into(),
                            label: "Start sweep (record all points)".into(),
                            tooltip: Some(
                                "Sweeps the modulation depth over [Sweep min a, Sweep max a] in \
                                 the configured number of points: per point A1 leases the \
                                 modulation owner, retargets the armed calibrated drive, waits \
                                 for the photodiode-measured a to settle, and records one sweep \
                                 point (…_pNN) like the Start recording button. Requires the \
                                 modulation plugin to have a calibrated periodic/optical drive \
                                 armed and Sweep min a > 0. Disabled until an output folder is \
                                 selected."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "record_pilot".into(),
                            label: "Record pilot (freeze ON/OFF windows)".into(),
                            tooltip: Some(
                                "Records a bright reference for this row into the same folder \
                                 (…_pilot) and freezes the ON/OFF windows from the current live \
                                 signal. Set a high, non-saturating a in the modulation plugin \
                                 first. The frozen windows are reused for the whole row's q_p. \
                                 Disabled until an output folder is selected."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "record_background".into(),
                            label: "Record background (a≈0 floor)".into(),
                            tooltip: Some(
                                "Records an unmodulated reference (…_background) and captures the \
                                 false-response floor q0 in the current windows. Set a≈0 in the \
                                 modulation plugin first. Disabled until an output folder is \
                                 selected."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "stop_recording".into(),
                            label: "Stop (abort recording / sweep)".into(),
                            tooltip: Some(
                                "Stop and finalize the current recording before the duration \
                                 ends; during a sweep this also aborts the remaining points."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                    ],
                },
                SettingsSection {
                    label: "Live analysis".into(),
                    description: Some(
                        "Live sanity quicklook. Folds the camera event stream on the modulation \
                         period T (defined by the firmware phase-0 EXT_TRIGGER) and renders the \
                         rolling half-period response S_p(t): events per valid pixel in the \
                         trailing T/2, ON and OFF separately. Use it to confirm events are \
                         appearing and the ON/OFF timing looks sane before recording. Nothing is \
                         recorded here."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "live".into(),
                            label: "Live analysis".into(),
                            tooltip: Some(
                                "Fold incoming events into the live plots. Off freezes the plots \
                                 at their current values. This does not record anything."
                                    .into(),
                            ),
                            kind: SettingKind::Bool { default: self.live },
                        },
                        SettingItem {
                            key: "analysis_window_ms".into(),
                            label: "Analysis window (ms)".into(),
                            tooltip: Some(
                                "Trailing window pulled exactly from the retained EventStore. \
                                 Longer windows cover more cycles; bounded by the host event-store \
                                 memory budget."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 1,
                                max: 120_000,
                                default: self.analysis_window_ms,
                            },
                        },
                        SettingItem {
                            key: "clear".into(),
                            label: "Clear captured events".into(),
                            tooltip: Some(
                                "Empties the fold buffer and resets the live plots.".into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                    ],
                },
                SettingsSection {
                    label: "Response probability q_p (live quicklook)".into(),
                    description: Some(
                        "Live view of the response-curve metric q_p: the fraction of valid \
                         pixel-cycles that fire at least once in the ON/OFF phase window (unlike \
                         S_p, each pixel-cycle counts at most once). The ON/OFF windows come from \
                         the row's pilot when one has been recorded (frozen, in the Recording \
                         section) and otherwise from the trigger-anchored fold automatically — each \
                         window grows out from its histogram peak until events drop below the \
                         window floor or the opposite polarity takes over. Press Record point at \
                         each amplitude to append a q_p(a) dot at the photodiode-measured a. The \
                         ROI and masked pixels come from the camera config. The authoritative fit \
                         is computed offline from the recordings; this is a quicklook."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "window_floor".into(),
                            label: "Window floor (fraction of peak)".into(),
                            tooltip: Some(
                                "Each ON/OFF window grows out from its histogram peak until events \
                                 fall below this fraction of the peak (or the opposite polarity \
                                 takes over). 0.10 = stop at 10 % of the peak."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.02,
                                max: 0.5,
                                speed: 0.01,
                                default: self.window_floor,
                            },
                        },
                        SettingItem {
                            key: "record_point".into(),
                            label: "Record point (at current a)".into(),
                            tooltip: Some(
                                "Computes q_on/q_off for the current buffer against the \
                                 auto-detected windows and appends a point at the \
                                 photodiode-measured a."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "clear_curve".into(),
                            label: "Clear response curve".into(),
                            tooltip: Some("Drops the recorded response-curve points.".into()),
                            kind: SettingKind::Button { enabled: true },
                        },
                    ],
                },
            ],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "output_folder" => Some(json!(self.output_folder)),
            "measurement_id" => Some(json!(self.measurement_id)),
            "min_a" => Some(json!(self.min_a)),
            "max_a" => Some(json!(self.max_a)),
            "sweep_count" => Some(json!(self.sweep_count)),
            "settle_s" => Some(json!(self.settle_s)),
            "duration_s" => Some(json!(self.duration_s)),
            "live" => Some(json!(self.live)),
            "analysis_window_ms" => Some(json!(self.analysis_window_ms)),
            "window_floor" => Some(json!(self.window_floor)),
            // Button presses are exported as monotonic counters so the host's
            // settings snapshot transports them from the UI mirror to the
            // live worker (see PressLatch).
            "start_recording" => Some(self.press_start.value()),
            "record_pilot" => Some(self.press_pilot.value()),
            "record_background" => Some(self.press_background.value()),
            "stop_recording" => Some(self.press_stop.value()),
            "start_sweep" => Some(self.press_sweep.value()),
            "clear" => Some(self.press_clear.value()),
            "record_point" => Some(self.press_record_point.value()),
            "clear_curve" => Some(self.press_clear_curve.value()),
            // New id regenerates the measurement id locally; the id itself is
            // what synchronizes, so the press must not be forwarded (both
            // instances would generate different ids).
            "new_id" => Some(json!(false)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "output_folder" => {
                self.output_folder = value
                    .as_str()
                    .ok_or("output_folder must be a string")?
                    .to_string();
            }
            "measurement_id" => {
                self.measurement_id = value
                    .as_str()
                    .ok_or("measurement_id must be a string")?
                    .to_string();
            }
            "new_id" if value.as_bool() == Some(true) => {
                self.measurement_id = generate_measurement_id();
            }
            "min_a" => {
                self.min_a = value
                    .as_f64()
                    .ok_or("min_a must be a number")?
                    .clamp(0.0, 10.0);
            }
            "max_a" => {
                self.max_a = value
                    .as_f64()
                    .ok_or("max_a must be a number")?
                    .clamp(0.0, 10.0);
            }
            "sweep_count" => {
                self.sweep_count = value
                    .as_i64()
                    .ok_or("sweep_count must be an integer")?
                    .clamp(2, 64);
            }
            "settle_s" => {
                self.settle_s = value
                    .as_f64()
                    .ok_or("settle_s must be a number")?
                    .clamp(0.0, 60.0);
            }
            "duration_s" => {
                self.duration_s = value
                    .as_i64()
                    .ok_or("duration_s must be an integer")?
                    .clamp(1, 3_600);
            }
            "start_recording" => {
                if self.press_start.accept(&value) {
                    self.pending_role = Some(RecRole::Normal);
                }
            }
            "record_pilot" => {
                if self.press_pilot.accept(&value) {
                    self.pending_role = Some(RecRole::Pilot);
                }
            }
            "record_background" => {
                if self.press_background.accept(&value) {
                    self.pending_role = Some(RecRole::Background);
                }
            }
            "start_sweep" => {
                if self.press_sweep.accept(&value) {
                    self.sweep_pending = true;
                }
            }
            "stop_recording" => {
                if self.press_stop.accept(&value) {
                    if self.recording.is_active() {
                        self.recording.stop_requested = true;
                    }
                    if let Some(sweep) = self.sweep.as_mut() {
                        sweep.stop_requested = true;
                        self.message = "Sweep stop requested".into();
                    }
                    self.sweep_pending = false;
                }
            }
            "live" => {
                self.live = value.as_bool().ok_or("live must be a boolean")?;
            }
            "analysis_window_ms" => {
                self.analysis_window_ms = value
                    .as_i64()
                    .ok_or("analysis_window_ms must be an integer")?
                    .clamp(1, 120_000);
            }
            "clear" => {
                if self.press_clear.accept(&value) {
                    self.camera_events.clear();
                    self.event_scratch.clear();
                    self.camera_markers_us.clear();
                    self.valid_pixels = 0;
                }
            }
            "window_floor" => {
                self.window_floor = value
                    .as_f64()
                    .ok_or("window_floor must be a number")?
                    .clamp(0.02, 0.5);
            }
            "record_point" => {
                if self.press_record_point.accept(&value) {
                    // Report failure via the status message: on the worker the
                    // press arrives through the settings snapshot, where a
                    // returned error would be silently dropped.
                    if let Err(error) = self.record_response_point() {
                        self.message = format!("Record point failed: {error}");
                    }
                }
            }
            "clear_curve" => {
                if self.press_clear_curve.accept(&value) {
                    self.response_points.clear();
                }
            }
            "new_id" => return Ok(()),
            _ => return Err(format!("unknown setting '{key}'")),
        }
        self.bump();
        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = vec![StatusEntry::LabeledValue {
            label: "Recording".into(),
            value: self.recording.state_label().into(),
            color: None,
        }];
        if self.recording.is_active() {
            if let Some(remaining) = self.recording.remaining_s(now_unix_ms()) {
                entries.push(StatusEntry::Text(format!(
                    "{} — {remaining} s remaining",
                    self.recording.id
                )));
            }
        }
        if let Some(sweep) = &self.sweep {
            let phase = match sweep.phase {
                SweepPhase::AcquiringLease => "leasing modulation",
                SweepPhase::SettingDepth => "retargeting drive",
                SweepPhase::Settling => "settling",
                SweepPhase::Recording => "recording",
            };
            entries.push(StatusEntry::Text(format!(
                "Sweep: point {}/{} at a → {:.3} ({phase})",
                sweep.index + 1,
                sweep.total(),
                sweep.target_a()
            )));
        }
        if !self.message.is_empty() {
            entries.push(StatusEntry::Text(self.message.clone()));
        }
        match self.period_us() {
            Some(period_us) => {
                let source = self.frequency_source();
                entries.push(StatusEntry::Text(format!(
                    "T = {:.3} ms ({:.3} Hz, {source})",
                    period_us / 1_000.0,
                    1_000_000.0 / period_us,
                )));
            }
            None => entries.push(StatusEntry::Text(
                "No modulation period (connect modulation or the EXT_TRIGGER)".into(),
            )),
        }
        let anchor = if self.is_marker_anchored() {
            format!(
                "{} phase-0 markers (trigger-anchored)",
                self.camera_markers_us.len()
            )
        } else {
            "free-running (no EXT_TRIGGER)".into()
        };
        entries.push(StatusEntry::Text(format!(
            "{} events, {} valid pixels; {anchor}",
            self.camera_events.len(),
            self.valid_pixels
        )));
        entries.push(StatusEntry::Text(match self.measured_a() {
            Some(a) => format!("a = {a:.3} (photodiode)"),
            None => {
                let detail = self
                    .photodiode
                    .as_ref()
                    .map(|summary| connection_label(&summary.connection))
                    .unwrap_or("no snapshot");
                format!("a = — (photodiode: {detail})")
            }
        }));
        if let Some((on, off)) = self.latest_rolling() {
            entries.push(StatusEntry::Text(format!(
                "S_on = {on:.4}, S_off = {off:.4} (events/pixel per T/2)"
            )));
        }
        let source = if self.windows_are_frozen() {
            "pilot-frozen"
        } else {
            "auto"
        };
        let windows = self.current_windows().map_or_else(
            || "windows —".into(),
            |(on, off)| {
                format!(
                    "windows ({source}) ON [{:.2},{:.2}) OFF [{:.2},{:.2})",
                    on.start, on.end, off.start, off.end
                )
            },
        );
        let valid = self
            .valid_pixel_count()
            .map_or_else(|| "—".into(), |n| n.to_string());
        entries.push(StatusEntry::Text(format!(
            "Response curve: {windows}, N_valid = {valid}, {} point(s)",
            self.response_points.len()
        )));
        if let Some((q0_on, q0_off)) = self.background_floor {
            entries.push(StatusEntry::Text(format!(
                "Background floor: q0_on = {q0_on:.3}, q0_off = {q0_off:.3}"
            )));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        fn column(id: &str, title: &str) -> TableColumn {
            TableColumn {
                id: id.into(),
                title: title.into(),
                value_type: TableValueType::String,
            }
        }
        HostViewRegistry {
            datasets: vec![
                HostDatasetDescriptor {
                    id: STATUS_DATASET_ID.into(),
                    title: "A1 status".into(),
                    kind: HostDatasetKind::TableV1(TableSchema {
                        columns: vec![
                            column("state", "Recording"),
                            column("measurement_id", "Measurement id"),
                            column("remaining", "Remaining"),
                            column("frequency", "Frequency"),
                            column("a", "a (photodiode)"),
                            column("s_on", "S_on"),
                            column("s_off", "S_off"),
                            column("events", "Events"),
                            column("message", "Message"),
                        ],
                        ..TableSchema::default()
                    }),
                    empty_message: "A1 idle".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: ROLLING_DATASET_ID.into(),
                    title: "A1 rolling response S_p(t) — live sanity check".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "Enable Live analysis; waiting for events and a period".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: RESPONSE_CURVE_DATASET_ID.into(),
                    title: "A1 response probability q_p(a) — live quicklook".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "Capture a pilot, then record points per amplitude".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: STATUS_VIEW_ID.into(),
                    title: "A1 status".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
                HostViewDescriptor {
                    id: ROLLING_VIEW_ID.into(),
                    title: "A1 rolling response S_p (ON/OFF)".into(),
                    dataset_id: ROLLING_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: RESPONSE_CURVE_VIEW_ID.into(),
                    title: "A1 response probability q_p (ON/OFF)".into(),
                    dataset_id: RESPONSE_CURVE_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
            ],
            actions: Vec::new(),
        }
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            ROLLING_DATASET_ID => serde_json::to_vec(&self.rolling_dataset()).ok(),
            RESPONSE_CURVE_DATASET_ID => serde_json::to_vec(&self.response_curve_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        matches!(
            dataset_id,
            STATUS_DATASET_ID | ROLLING_DATASET_ID | RESPONSE_CURVE_DATASET_ID
        )
        .then_some(self.dataset_generation)
        .unwrap_or(0)
    }
}

fn connection_label(connection: &ConnectionStateV1) -> &'static str {
    match connection {
        ConnectionStateV1::Connected { .. } => "connected",
        ConnectionStateV1::Connecting => "connecting",
        ConnectionStateV1::Disconnected => "disconnected",
        ConnectionStateV1::Faulted { .. } => "faulted",
    }
}

export_plugin!(StageAA1Plugin);

#[cfg(test)]
mod tests {
    use stage_a_plugin_contract::{
        FreshnessV1, OwnerInstanceId, PdqFinalizedReceiptV1, PdqStartedReceiptV1,
        PhotodiodeStreamV1, RequestOutcomeV1, ResponseCommonV1, Sha256V1, StreamIntegrityV1,
        SynchronizationV1, CONTRACT_VERSION_V1,
    };

    use super::*;

    #[derive(Default)]
    struct ControlSink {
        services: Vec<PluginServiceRequest>,
        hosts: Vec<HostCommandRequest>,
    }

    impl RecordingControl for ControlSink {
        fn request_service(&mut self, request: &PluginServiceRequest) {
            self.services.push(request.clone());
        }

        fn request_host(&mut self, request: &HostCommandRequest) {
            self.hosts.push(request.clone());
        }
    }

    fn control_tick(
        plugin: &mut StageAA1Plugin,
        inbox: PluginControlInbox,
        sink: &mut ControlSink,
    ) {
        for reply in &inbox.host_replies {
            plugin.on_host_reply(reply);
        }
        for reply in &inbox.service_replies {
            plugin.on_service_reply(reply);
        }
        plugin.drive_recording(sink);
    }

    fn pd_reply(request_id: u64, receipt: Option<PdqReceiptV1>) -> PluginServiceReply {
        let response = PhotodiodeResponseV1 {
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: RequestId(request_id),
                owner_instance: OwnerInstanceId::new("pd-test"),
                run_id: None,
                requested_revision: None,
                acknowledged_revision: None,
                outcome: RequestOutcomeV1::Applied,
                completed_at_unix_ms: Some(now_unix_ms()),
                error: None,
            },
            receipt,
        };
        PluginServiceReply {
            request_id,
            source_plugin_id: A1_PLUGIN_ID.into(),
            target_plugin_id: PHOTODIODE_PLUGIN_ID.into(),
            service: SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1.into(),
            outcome: PluginServiceOutcome::Accepted {
                payload: serde_json::to_value(response).expect("response"),
            },
        }
    }

    /// A photodiode summary that passes the pre-flight: connected, unleased,
    /// and with somewhere to put the PDQ.
    fn ready_photodiode() -> PhotodiodeSummaryV1 {
        PhotodiodeSummaryV1 {
            contract_version: CONTRACT_VERSION_V1,
            owner_instance: OwnerInstanceId::new("pd-test"),
            service_revision: 1,
            connection: ConnectionStateV1::Connected {
                port_label: "mock".into(),
                firmware_version: None,
            },
            lease: None,
            active_run_id: None,
            requested_revision: None,
            acknowledged_revision: None,
            stream: PhotodiodeStreamV1 {
                stream_epoch: 1,
                sample_range: None,
                sample_rate_hz: Some(20_000),
                latest_adc_code: Some(1_000),
                integrity: StreamIntegrityV1::default(),
                level: None,
            },
            data_dir: Some("/pd".into()),
            active_recording: None,
            last_finalized_recording: None,
            optical_summary: None,
            synchronization: SynchronizationV1::Unsynced {
                reason: stage_a_plugin_contract::UnsyncedReasonV1::NoLease,
                detail: None,
            },
            last_response: None,
            freshness: FreshnessV1 {
                observed_at_unix_ms: now_unix_ms(),
                valid_for_ms: 60_000,
            },
        }
    }

    fn on(timestamp_us: u64) -> CameraEvent {
        CameraEvent {
            timestamp_us,
            x: 0,
            y: 0,
            polarity: Polarity::On,
        }
    }

    /// A plugin whose period comes from marker spacing (no fallback frequency).
    fn plugin_with_markers() -> StageAA1Plugin {
        StageAA1Plugin {
            valid_pixels: 10,
            camera_markers_us: vec![0, 1_000, 2_000, 3_000],
            ..StageAA1Plugin::default()
        }
    }

    #[test]
    fn period_comes_from_the_trigger_marker_spacing() {
        let plugin = plugin_with_markers();
        let period = plugin.period_us().expect("measured period");
        assert!((period - 1_000.0).abs() < 1e-6, "period={period}");
        assert_eq!(plugin.frequency_source(), "trigger");
    }

    #[test]
    fn no_markers_and_no_modulation_yields_no_period() {
        let plugin = StageAA1Plugin::default();
        assert!(plugin.period_us().is_none());
        assert!(plugin.rolling_dataset().lines[0].points.is_empty());
    }

    #[test]
    fn external_triggers_anchor_the_fold() {
        let mut plugin = plugin_with_markers();
        for cycle in 0..3 {
            plugin.camera_events.push(on(cycle * 1_000 + 200));
        }
        assert!(plugin.is_marker_anchored());
        let fold = plugin.current_fold().expect("marker fold");
        assert_eq!(fold.validation.cycle_count, 3);
        assert!((fold.events[0].phase - 0.2).abs() < 1e-9);
    }

    #[test]
    fn rolling_dataset_keeps_on_and_off_separate() {
        let mut plugin = plugin_with_markers();
        for cycle in 0..3 {
            let base = cycle * 1_000;
            plugin.camera_events.push(on(base + 100));
            plugin.camera_events.push(CameraEvent {
                polarity: Polarity::Off,
                ..on(base + 600)
            });
        }
        let rolling = plugin.rolling_dataset();
        assert_eq!(rolling.lines.len(), 2);
        assert_eq!(rolling.lines[0].name, "ON");
        assert!(rolling.lines[0].points.len() >= 2);
    }

    #[test]
    fn response_curve_auto_windows_without_a_pilot_and_refuses_without_a() {
        let mut plugin = plugin_with_markers();
        plugin.frame_width = 8;
        plugin.frame_height = 1;
        for cycle in 0..20 {
            let base = cycle * 1_000;
            for x in 0..4 {
                plugin.camera_events.push(CameraEvent {
                    timestamp_us: base + 200,
                    x,
                    y: 0,
                    polarity: Polarity::On,
                });
                plugin.camera_events.push(CameraEvent {
                    timestamp_us: base + 700,
                    x,
                    y: 0,
                    polarity: Polarity::Off,
                });
            }
        }
        plugin.camera_markers_us = (0..=20).map(|c| c * 1_000).collect();
        plugin.host_roi = Some(RoiV1 {
            x: 0,
            y: 0,
            width: 4,
            height: 1,
        });

        // Windows come straight from the fold — no pilot capture needed.
        let (q_on, q_off, _, valid) = plugin.current_response().expect("response");
        assert_eq!(valid, 4);
        assert!(q_on > 0.9 && q_off > 0.9, "q_on={q_on} q_off={q_off}");
        // Recording a point is still refused without a photodiode-measured a.
        assert!(plugin.measured_a().is_none());
        assert!(plugin.record_response_point().is_err());
    }

    #[test]
    fn press_latch_distinguishes_clicks_baselines_and_advances() {
        let mut latch = PressLatch::default();
        // Direct click on this instance: an edge, and the counter advances.
        assert!(latch.accept(&json!(true)));
        assert_eq!(latch.value(), json!(1));
        // `false` writes (legacy snapshots) are never edges.
        assert!(!latch.accept(&json!(false)));

        // A fresh instance adopts the first forwarded counter silently…
        let mut worker = PressLatch::default();
        assert!(!worker.accept(&json!(3)));
        // …repeats are not edges…
        assert!(!worker.accept(&json!(3)));
        // …and only an advance is one press.
        assert!(worker.accept(&json!(4)));
        assert!(!worker.accept(&json!(4)));
    }

    #[test]
    fn forwarded_button_counter_latches_the_recording_role() {
        let mut plugin = StageAA1Plugin::default();
        // First snapshot after (re)load: adopt the mirror's counter, no press.
        plugin
            .set_setting("start_recording", json!(2))
            .expect("baseline");
        assert!(plugin.pending_role.is_none());
        // The mirror's counter advances by one click → one press edge.
        plugin
            .set_setting("start_recording", json!(3))
            .expect("press");
        assert_eq!(plugin.pending_role, Some(RecRole::Normal));
        // Re-applying the same snapshot must not re-press.
        plugin.pending_role = None;
        plugin
            .set_setting("start_recording", json!(3))
            .expect("repeat");
        assert!(plugin.pending_role.is_none());
    }

    #[test]
    fn recording_orders_camera_then_pdq_and_saves_inside_the_measurement_folder() {
        let folder = std::env::temp_dir().join(format!("a1-lifecycle-{}", now_unix_ms()));
        let mut plugin = StageAA1Plugin {
            output_folder: folder.display().to_string(),
            measurement_id: "A1-row".into(),
            duration_s: 1,
            pending_role: Some(RecRole::Normal),
            photodiode: Some(ready_photodiode()),
            ..StageAA1Plugin::default()
        };
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert_eq!(plugin.recording.phase, RecPhase::StartingCamera);
        assert_eq!(sink.hosts.len(), 1);
        assert!(sink.services.is_empty(), "PDQ must not start before camera");
        let cam_start_req = sink.hosts[0].request_id;

        control_tick(
            &mut plugin,
            PluginControlInbox {
                host_replies: vec![HostCommandReply {
                    request_id: cam_start_req,
                    outcome: HostCommandOutcome::RecordingStarted {
                        actual_raw_path: "/camera/A1-row/run.raw".into(),
                        started_at: "2026-07-23T00:00:00Z".into(),
                    },
                }],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let connect = sink.services.last().expect("connect request");
        let connect_envelope: PhotodiodeRequestV1 =
            serde_json::from_value(connect.payload.clone()).expect("connect envelope");
        assert!(matches!(
            connect_envelope.command,
            PhotodiodeCommandV1::Connect
        ));

        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_reply(connect.request_id, None)],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let acquire = sink.services.last().expect("lease request");
        let acquire_envelope: PhotodiodeRequestV1 =
            serde_json::from_value(acquire.payload.clone()).expect("lease envelope");
        assert!(matches!(
            acquire_envelope.command,
            PhotodiodeCommandV1::AcquireLease { .. }
        ));
        assert_eq!(
            acquire_envelope.run_id.as_ref().map(RunId::as_str),
            Some(plugin.recording.stem.as_str())
        );

        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_reply(acquire.request_id, None)],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let begin = sink.services.last().expect("begin request");
        let begin_envelope: PhotodiodeRequestV1 =
            serde_json::from_value(begin.payload.clone()).expect("begin envelope");
        assert!(matches!(
            begin_envelope.command,
            PhotodiodeCommandV1::BeginRecording { .. }
        ));
        assert_eq!(begin_envelope.requested_revision, Some(SemanticRevision(1)));
        assert_eq!(plugin.recording.start_unix_ms, 0);

        let run_id = begin_envelope.run_id.expect("run id");
        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_reply(
                    begin.request_id,
                    Some(PdqReceiptV1::Started(PdqStartedReceiptV1 {
                        run_id: run_id.clone(),
                        pdq_path: "/pd/A1-row/run_pd.pdq".into(),
                        sidecar_path: "/pd/A1-row/run_pd.json".into(),
                        opened_at_unix_ms: now_unix_ms(),
                        stream_epoch: 1,
                        first_sample_index: Some(0),
                    })),
                )],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        assert_eq!(plugin.recording.phase, RecPhase::Running);
        assert!(plugin.recording.start_unix_ms > 0);

        plugin.recording.start_unix_ms = now_unix_ms().saturating_sub(1_000);
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let release = sink.services.last().expect("release request");
        let release_envelope: PhotodiodeRequestV1 =
            serde_json::from_value(release.payload.clone()).expect("release envelope");
        assert!(matches!(
            release_envelope.command,
            PhotodiodeCommandV1::ReleaseLease {
                finalize_recording: true,
                ..
            }
        ));
        assert_eq!(sink.hosts.len(), 1, "camera keeps running until PDQ closes");

        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_reply(
                    release.request_id,
                    Some(PdqReceiptV1::Finalized(PdqFinalizedReceiptV1 {
                        run_id,
                        pdq_path: "/pd/A1-row/run_pd.pdq".into(),
                        sidecar_path: "/pd/A1-row/run_pd.json".into(),
                        opened_at_unix_ms: now_unix_ms().saturating_sub(1_000),
                        finalized_at_unix_ms: now_unix_ms(),
                        file_size_bytes: 64,
                        sha256: Sha256V1::parse("ab".repeat(32)).expect("sha"),
                        frames_written: 1,
                        sample_frames_written: 1,
                        sample_range: None,
                        sample_rate_hz: Some(20_000),
                        segment_count: 1,
                        integrity: StreamIntegrityV1::default(),
                        termination: stage_a_plugin_contract::PdqTerminationV1::OperatorStopped,
                        valid: true,
                    })),
                )],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        assert_eq!(plugin.recording.phase, RecPhase::StoppingCamera);
        assert_eq!(sink.hosts.len(), 2);
        let cam_stop_req = sink.hosts[1].request_id;

        control_tick(
            &mut plugin,
            PluginControlInbox {
                host_replies: vec![HostCommandReply {
                    request_id: cam_stop_req,
                    outcome: HostCommandOutcome::RecordingFinalized {
                        actual_raw_path: "/camera/A1-row/run.raw".into(),
                        size: 128,
                        sha256: "cd".repeat(32),
                        duration_us: 1_000_000,
                    },
                }],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        assert_eq!(plugin.recording.phase, RecPhase::Idle);
        let measurement_dir = folder.join("A1-row");
        let sidecars: Vec<_> = std::fs::read_dir(&measurement_dir)
            .expect("measurement folder")
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(sidecars.len(), 1);
        assert!(sidecars[0]
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with("_config.toml")));
        assert!(plugin.message.starts_with("Saved recording A1-row"));

        std::fs::remove_dir_all(folder).expect("cleanup");
    }

    #[test]
    fn duplicate_or_jittery_markers_still_yield_a_fold() {
        // A long trigger dropout leaves a gap far beyond the jitter tolerance:
        // marker validation rejects the fold, but the quicklook must fall back
        // to the free-running fold instead of blanking.
        let mut plugin = StageAA1Plugin {
            valid_pixels: 10,
            camera_markers_us: vec![0, 1_000, 2_000, 10_000],
            ..StageAA1Plugin::default()
        };
        for cycle in 0..10 {
            plugin.camera_events.push(on(cycle * 1_000 + 200));
        }
        let fold = plugin.current_fold().expect("fallback fold");
        assert!(fold.markers_us.is_empty(), "free-running fold expected");
        assert!(!plugin.rolling_dataset().lines[0].points.is_empty());
    }

    #[test]
    fn sweep_points_span_the_range_inclusively() {
        let plugin = StageAA1Plugin {
            min_a: 0.5,
            max_a: 2.5,
            sweep_count: 5,
            ..StageAA1Plugin::default()
        };
        let points = plugin.sweep_points();
        assert_eq!(points.len(), 5);
        assert!((points[0] - 0.5).abs() < 1e-12);
        assert!((points[4] - 2.5).abs() < 1e-12);
        assert!((points[2] - 1.5).abs() < 1e-12);
    }

    #[test]
    fn sweep_point_recordings_carry_the_requested_a_in_the_sidecar() {
        let mut plugin = plugin_with_markers();
        plugin.min_a = 0.5;
        plugin.max_a = 1.5;
        plugin.sweep = Some(Sweep {
            phase: SweepPhase::Recording,
            points: vec![0.5, 1.0, 1.5],
            index: 1,
            lease_id: LeaseId::new("a1-sweep-test"),
            lease_granted: true,
            lease_req: 0,
            depth_req: 0,
            depth_applied: true,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: true,
            last_activity_ms: 0,
            stop_requested: false,
        });
        plugin.recording.id = "A1-sweeprow".into();
        plugin.recording.stem = "A1-sweeprow_20260723-000000_p02".into();
        plugin.recording.folder = std::env::temp_dir().display().to_string();
        plugin.recording.duration_s = 5;
        plugin.recording.start_unix_ms = 1_774_224_000_000;
        let path = plugin.write_sidecar().expect("sidecar path");
        let text = std::fs::read_to_string(&path).expect("read sidecar");
        assert!(text.contains("requested_a = 1.0"), "sidecar: {text}");
        assert!(text.contains("point_index = 2"));
        assert!(text.contains("point_total = 3"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settings_discontinuities_keep_the_response_curve() {
        let mut plugin = StageAA1Plugin::default();
        plugin.response_points.push(ResponsePoint {
            measured_a: 1.0,
            q_on: 0.5,
            q_off: 0.1,
            cycles: 10,
            valid_pixels: 4,
        });
        plugin.on_discontinuity(PluginDiscontinuity::SettingsChanged);
        assert_eq!(plugin.response_points.len(), 1);
        plugin.on_discontinuity(PluginDiscontinuity::SourceChanged);
        assert!(plugin.response_points.is_empty());
    }

    #[test]
    fn measurement_id_generation_is_file_safe_and_prefixed() {
        let id = generate_measurement_id();
        assert!(id.starts_with("A1-"));
        assert_eq!(sanitize_stem(&id), id);
        assert_eq!(sanitize_stem("I_k 3 / f=10Hz"), "I_k_3_f_10Hz");
    }

    #[test]
    fn compact_utc_formats_a_known_epoch() {
        // 2026-07-23T00:00:00Z = 1_784_764_800 s; +3661 s = 01:01:01.
        assert_eq!(format_compact_date(1_784_764_800), "20260723");
        assert_eq!(format_iso_utc(1_784_764_800), "2026-07-23T00:00:00Z");
        assert_eq!(format_compact_utc(1_784_764_800), "20260723-000000");
        assert_eq!(
            format_iso_utc(1_784_764_800 + 3_661),
            "2026-07-23T01:01:01Z"
        );
    }

    #[test]
    fn sidecar_serializes_the_expected_sections() {
        let mut plugin = plugin_with_markers();
        plugin.frame_width = 4;
        plugin.frame_height = 1;
        plugin.recording.id = "A1-test".into();
        plugin.recording.stem = "A1-test_20260723-000000".into();
        plugin.recording.folder = std::env::temp_dir().display().to_string();
        plugin.recording.duration_s = 5;
        plugin.recording.start_unix_ms = 1_774_224_000_000;
        plugin.recording.cam_finalized_path = Some("/data/A1-test/A1-test.raw".into());
        plugin.recording.pd_pdq_path = Some("/pd/A1-test/A1-test_pd.pdq".into());
        let doc = plugin.write_sidecar().expect("sidecar path");
        let text = std::fs::read_to_string(&doc).expect("read sidecar");
        assert!(text.contains("measurement_id = \"A1-test\""));
        assert!(text.contains("[modulation]"));
        assert!(text.contains("[camera]"));
        assert!(text.contains("[files]"));
        assert!(text.contains("camera_config_sidecar = \"/data/A1-test/A1-test.toml\""));
        let _ = std::fs::remove_file(&doc);
    }

    #[test]
    fn pilot_windows_round_trip_through_the_folder() {
        let dir = std::env::temp_dir().join(format!("a1-pilot-{}", now_unix_ms()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let folder = dir.display().to_string();

        let mut plugin = plugin_with_markers();
        plugin.output_folder = folder.clone();
        plugin.measurement_id = "A1-row".into();
        plugin.pilot_windows = Some((
            PhaseWindow {
                start: 0.10,
                end: 0.30,
            },
            PhaseWindow {
                start: 0.55,
                end: 0.80,
            },
        ));
        // Write a pilot sidecar for the row.
        plugin.recording.role = RecRole::Pilot;
        plugin.recording.id = "A1-row".into();
        plugin.recording.stem = "A1-row_20260723-000000_pilot".into();
        plugin.recording.folder = folder.clone();
        plugin.write_sidecar().expect("pilot sidecar");

        // A fresh plugin on the same folder+id auto-loads the frozen windows.
        let mut other = StageAA1Plugin {
            output_folder: folder.clone(),
            measurement_id: "A1-row".into(),
            ..StageAA1Plugin::default()
        };
        other.scan_measurement_folder();
        let (on, off) = other.pilot_windows.expect("loaded windows");
        assert!((on.start - 0.10).abs() < 1e-9 && (off.end - 0.80).abs() < 1e-9);
        assert!(other.windows_are_frozen());

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn pd_rejection(request_id: u64, code: &str, message: &str) -> PluginServiceReply {
        PluginServiceReply {
            request_id,
            source_plugin_id: A1_PLUGIN_ID.into(),
            target_plugin_id: PHOTODIODE_PLUGIN_ID.into(),
            service: SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1.into(),
            outcome: PluginServiceOutcome::Rejected {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    /// A photodiode that cannot record is caught before the host is recording,
    /// so a misconfigured bench no longer leaves a stub RAW behind.
    #[test]
    fn a_disconnected_photodiode_is_refused_before_the_camera_starts() {
        let mut photodiode = ready_photodiode();
        photodiode.connection = ConnectionStateV1::Disconnected;
        let mut plugin = StageAA1Plugin {
            output_folder: "/tmp/a1-preflight".into(),
            measurement_id: "A1-row".into(),
            duration_s: 10,
            pending_role: Some(RecRole::Normal),
            photodiode: Some(photodiode),
            ..StageAA1Plugin::default()
        };
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert_eq!(plugin.recording.phase, RecPhase::Idle);
        assert!(
            sink.hosts.is_empty(),
            "the camera must not start when the PDQ cannot follow"
        );
        assert!(
            plugin.message.contains("connect it"),
            "message={}",
            plugin.message
        );
    }

    /// The photodiode's own Data directory is irrelevant to a recording started
    /// from A1: A1 names the destination root, so the run proceeds and the PDQ
    /// is written into A1's measurement folder.
    #[test]
    fn the_pdq_start_spec_points_at_the_a1_output_folder() {
        let mut photodiode = ready_photodiode();
        photodiode.data_dir = None;
        let mut plugin = StageAA1Plugin {
            output_folder: "/tmp/a1-destination".into(),
            measurement_id: "A1-row".into(),
            duration_s: 10,
            pending_role: Some(RecRole::Normal),
            photodiode: Some(photodiode),
            ..StageAA1Plugin::default()
        };
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert_eq!(
            plugin.recording.phase,
            RecPhase::StartingCamera,
            "an unset owner data directory must not block an A1-driven run"
        );
        let cam_start_req = sink.hosts[0].request_id;

        control_tick(
            &mut plugin,
            PluginControlInbox {
                host_replies: vec![HostCommandReply {
                    request_id: cam_start_req,
                    outcome: HostCommandOutcome::RecordingStarted {
                        actual_raw_path: "/camera/A1-row/run.raw".into(),
                        started_at: "2026-07-25T00:00:00Z".into(),
                    },
                }],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let connect = sink.services.last().expect("connect").clone();
        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_reply(connect.request_id, None)],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let acquire = sink.services.last().expect("lease").clone();
        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_reply(acquire.request_id, None)],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );

        let begin: PhotodiodeRequestV1 =
            serde_json::from_value(sink.services.last().expect("begin").payload.clone())
                .expect("begin envelope");
        let PhotodiodeCommandV1::BeginRecording { specification } = begin.command else {
            panic!("expected BeginRecording");
        };
        assert_eq!(
            specification.root_dir.as_deref(),
            Some("/tmp/a1-destination"),
            "the PDQ must be written below the A1 output folder"
        );
        assert_eq!(
            specification.pdq_path,
            format!("A1-row/{}_pd.pdq", plugin.recording.stem)
        );
    }

    /// The regression this whole coordinator exists for: a photodiode failure
    /// used to stop the host recorder immediately, leaving a RAW that was a
    /// fraction of the requested duration but reported itself as finalized.
    #[test]
    fn a_photodiode_failure_keeps_the_camera_recording_for_the_full_duration() {
        let folder = std::env::temp_dir().join(format!("a1-camera-only-{}", now_unix_ms()));
        let host_dir = folder.join("host-output");
        std::fs::create_dir_all(&host_dir).expect("host dir");
        let mut plugin = StageAA1Plugin {
            output_folder: folder.display().to_string(),
            measurement_id: "A1-row".into(),
            duration_s: 10,
            pending_role: Some(RecRole::Normal),
            photodiode: Some(ready_photodiode()),
            ..StageAA1Plugin::default()
        };
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let cam_start_req = sink.hosts[0].request_id;
        let raw_path = host_dir.join(format!("{}.raw", plugin.recording.stem));
        std::fs::write(&raw_path, b"raw-events").expect("raw file");
        std::fs::write(raw_path.with_extension("toml"), b"biases = true").expect("bias sidecar");

        control_tick(
            &mut plugin,
            PluginControlInbox {
                host_replies: vec![HostCommandReply {
                    request_id: cam_start_req,
                    outcome: HostCommandOutcome::RecordingStarted {
                        actual_raw_path: raw_path.display().to_string(),
                        started_at: "2026-07-25T00:00:00Z".into(),
                    },
                }],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let connect = sink.services.last().expect("connect request").clone();

        // The photodiode refuses to open the stream.
        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![pd_rejection(
                    connect.request_id,
                    "transport",
                    "photodiode connection failed",
                )],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );

        assert_eq!(
            plugin.recording.phase,
            RecPhase::Running,
            "the camera must keep recording without the photodiode"
        );
        assert_eq!(
            sink.hosts.len(),
            1,
            "no StopRecording may be sent before the duration elapses"
        );

        // Nothing happens until the fixed duration is actually over.
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert_eq!(sink.hosts.len(), 1, "the run is still inside its window");

        plugin.recording.start_unix_ms = now_unix_ms().saturating_sub(10_000);
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert_eq!(plugin.recording.phase, RecPhase::StoppingCamera);
        let cam_stop_req = sink.hosts[1].request_id;

        control_tick(
            &mut plugin,
            PluginControlInbox {
                host_replies: vec![HostCommandReply {
                    request_id: cam_stop_req,
                    outcome: HostCommandOutcome::RecordingFinalized {
                        actual_raw_path: raw_path.display().to_string(),
                        size: 10,
                        sha256: "cd".repeat(32),
                        duration_us: 10_000_000,
                    },
                }],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );

        assert_eq!(plugin.recording.phase, RecPhase::Idle);
        assert!(!plugin.recording_completed_ok, "the PDQ is missing");
        // The closing message names the cause instead of only "incomplete".
        assert!(
            plugin.message.contains("photodiode connection failed"),
            "message={}",
            plugin.message
        );
        // Camera RAW, its bias sidecar, and the config all land together.
        let measurement_dir = folder.join("A1-row");
        let mut names: Vec<String> = std::fs::read_dir(&measurement_dir)
            .expect("measurement folder")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 3, "names={names:?}");
        assert!(names.iter().any(|name| name.ends_with(".raw")));
        assert!(names.iter().any(|name| name.ends_with("_config.toml")));
        assert!(
            !raw_path.exists(),
            "the RAW must be moved out of the host output folder"
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// PDQ receipts name the path *relative to the photodiode's data directory*,
    /// so gathering has to resolve it against the owner's published root before
    /// the file can be found and moved.
    #[test]
    fn a_relative_pdq_label_is_resolved_against_the_photodiode_data_directory() {
        let root = std::env::temp_dir().join(format!("a1-gather-{}", now_unix_ms()));
        let pd_root = root.join("pd-data");
        std::fs::create_dir_all(pd_root.join("A1-row")).expect("pd dirs");
        std::fs::write(pd_root.join("A1-row/run_pd.pdq"), b"pdq").expect("pdq");
        std::fs::write(pd_root.join("A1-row/run_pd.json"), b"{}").expect("pd sidecar");

        let mut photodiode = ready_photodiode();
        photodiode.data_dir = Some(pd_root.display().to_string());
        let mut plugin = StageAA1Plugin {
            output_folder: root.display().to_string(),
            photodiode: Some(photodiode),
            ..StageAA1Plugin::default()
        };
        plugin.recording.id = "A1-row".into();
        plugin.recording.stem = "run".into();
        plugin.recording.folder = root.display().to_string();
        // Exactly what the owner reports: a label, not a path.
        plugin.recording.pd_pdq_path = Some("A1-row/run_pd.pdq".into());
        plugin.recording.pd_sidecar_path = Some("A1-row/run_pd.json".into());

        plugin.gather_into_measurement_folder();

        let measurement_dir = root.join("A1-row");
        assert!(measurement_dir.join("run_pd.pdq").is_file());
        assert!(measurement_dir.join("run_pd.json").is_file());
        assert!(!pd_root.join("A1-row/run_pd.pdq").exists());
        // The sidecar records where the file actually ended up.
        assert_eq!(
            plugin.recording.pd_pdq_path.as_deref(),
            Some(measurement_dir.join("run_pd.pdq").display().to_string()).as_deref()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The host restarts the pipeline when A1 starts its own recording and
    /// reports it as SourceChanged. That must not wipe the row's science state.
    #[test]
    fn a_self_inflicted_source_change_keeps_the_rows_science_state() {
        let mut plugin = StageAA1Plugin {
            response_points: vec![ResponsePoint {
                measured_a: 1.0,
                q_on: 0.5,
                q_off: 0.4,
                cycles: 20,
                valid_pixels: 10,
            }],
            pilot_windows: Some((
                PhaseWindow {
                    start: 0.1,
                    end: 0.4,
                },
                PhaseWindow {
                    start: 0.6,
                    end: 0.9,
                },
            )),
            camera_markers_us: vec![0, 1_000],
            ..StageAA1Plugin::default()
        };
        plugin.recording.phase = RecPhase::Running;

        plugin.on_discontinuity(PluginDiscontinuity::SourceChanged);

        assert_eq!(plugin.response_points.len(), 1, "sweep points were wiped");
        assert!(plugin.pilot_windows.is_some(), "pilot windows were wiped");
        assert!(
            plugin.camera_markers_us.is_empty(),
            "the event timeline really did restart and must reset"
        );

        // Outside a recording the boundary still resets everything.
        plugin.recording = Recording::idle();
        plugin.on_discontinuity(PluginDiscontinuity::SourceChanged);
        assert!(plugin.response_points.is_empty());
        assert!(plugin.pilot_windows.is_none());
    }
}
