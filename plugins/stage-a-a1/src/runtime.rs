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
//!    `a` to settle, and records the point through the same coordinator. The
//!    **exact-event-count workflow** (ADR 013) reuses that path the other way round:
//!    the `a₀` **lock** trims the *commanded* depth closed-loop until the photodiode
//!    *measures* the one frozen depth `a₀`, and an **event-count point** replays that
//!    trimmed depth under the same lease so one atomic frequency point is recorded at
//!    exactly `a₀`.
//!
//!    Where that `a` comes from is one operator setting, [`DepthSource`] (ADR 020). The
//!    photodiode's measurement is the default and the source of record; it is also
//!    fail-closed on firmware phase-0 markers, so a bench that never receives them can
//!    fall back to the modulation owner's *commanded* calibrated depth and run the same
//!    workflow open loop. Every artefact that carries an `a` carries which source
//!    produced it.
//!
//! 2. **Live sanity quicklooks.** Folding the camera event stream on the modulation
//!    period `T` (defined by the firmware phase-0 `EXT_TRIGGER`), it renders the
//!    **rolling half-period response** `S_p(t)` (a live "are events appearing, is the
//!    ON/OFF timing sane?" indicator) and the **response probability** `q_p` curve
//!    (frozen-window Bernoulli statistic vs the measured `a`). The authoritative
//!    `q_p(a, f)` fit is computed offline from the recordings; the live plot is a
//!    quicklook.

use std::cell::RefCell;
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
    PluginServiceReply, PluginServiceRequest, RoiV1, SensorMonitoringV1, Series1dLine,
    Series1dPoint, Series1dV1, SettingItem, SettingKind, SettingsSchema, SettingsSection,
    StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1, TableSchema,
    TableValueType, CTX_GLOBAL_SETTINGS, CTX_SENSOR_MONITORING,
};
use serde::Serialize;
use serde_json::{json, Value};
use stage_a_plugin_contract::{
    ClientId, ConnectionStateV1, LeaseId, LeaseSnapshotV1, ModulationCommandV1,
    ModulationRequestV1, ModulationStateV1, OpticalTargetV1, PdqReceiptV1, PdqStartSpecV1,
    PhotodiodeCommandV1, PhotodiodeOpticalSummaryV1, PhotodiodeRequestV1, PhotodiodeResponseV1,
    PhotodiodeSummaryV1, RequestId, RunId, SemanticRevision, WaveformV1,
    CTX_STAGE_A_MODULATION_STATE_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
    SERVICE_STAGE_A_MODULATION_CONTROL_V1, SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
};

use crate::eta::{format_duration, Eta};
use crate::phase::{fold_events, fold_events_free_running, MarkerValidationConfig, PhaseFold};
use crate::protocol;
use crate::rates::{rolling_half_period_response, RollingResponsePoint};
use crate::response_curve::{auto_windows, response_probability, PhaseWindow, ResponsePoint, Roi};
use crate::sensor;
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
const A0_LOCK_DATASET_ID: &str = "stage-a-a1.a0-locks";
const A0_LOCK_VIEW_ID: &str = "stage-a-a1.a0-locks.view";

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

/// Closed-loop trials the `a₀` lock spends on one frequency before it gives up
/// and reports the best commanded depth it reached.
const A0_LOCK_MAX_TRIALS: u32 = 8;
/// Per-trial cap on the multiplicative correction of the commanded depth, so one
/// noisy photodiode reading cannot slam the drive across its whole range.
const A0_LOCK_MAX_STEP_RATIO: f64 = 2.0;
/// Independent photodiode readings taken per trial (fewer only when the
/// measurement deadline hits first). Their *median* is the trial's value and
/// their spread is the stability check — one estimator window already averages
/// many cycles, so repeating it is about catching drift, not reducing noise.
const A0_LOCK_SAMPLES: usize = 3;
/// Fraction of one estimator window that must pass between two readings for
/// them to count as independent. Consecutive `service_revision`s share almost
/// their whole window, so sampling per revision alone measures the publisher's
/// tick rate rather than the drive.
const A0_LOCK_SAMPLE_SPACING: f64 = 0.5;
/// Spread across a trial's readings, relative to its tolerance, above which the
/// operating point is called unstable instead of locked. A drifting `a` that
/// happens to cross the target on one reading is not a lock.
const A0_LOCK_MAX_SPREAD_TOLERANCES: f64 = 2.0;
/// Trials one `a₀` lock is *expected* to spend, for the time estimate only.
/// The cap above is [`A0_LOCK_MAX_TRIALS`], but a calibrated drive lands inside
/// the tolerance in two or three, and estimating every rung of a ladder at the
/// cap would put a night on a run that takes an hour.
const A0_LOCK_NOMINAL_TRIALS: f64 = 3.0;
/// Nominal measuring time of one lock trial on top of the operator's settle
/// dwell: the independent photodiode readings it takes a window apart.
const A0_LOCK_NOMINAL_TRIAL_S: f64 = 4.0;
/// Nominal firmware/table cost of retargeting the drive to a new frequency, on
/// top of the marker periods the confirmation then waits for.
const FREQ_RETARGET_NOMINAL_S: f64 = 2.0;
/// Clipping fraction above which a lock's measured `a` is called out as
/// unreliable in the operator message.
///
/// Deliberately far below the estimator's own `MAX_CLIP_FRACTION` (1 ‰, above
/// which it withholds `a` altogether): a threshold at or above that one could
/// never fire, because a published summary has already passed it.
const A0_LOCK_CLIP_WARNING: f64 = 0.000_2;
/// Closed range of commanded optical depths the modulation owner accepts.
const COMMANDED_A_MIN: f64 = 0.01;
const COMMANDED_A_MAX: f64 = 6.0;
/// Relative distance within which two frequencies are the same sweep point.
const FREQUENCY_MATCH_FRACTION: f64 = 0.01;
/// Lock table persisted in the output folder, so found depths survive a restart.
const A0_LOCK_FILE: &str = "a0_locks.json";
/// Frequency points a single run may visit, before the interleaved references.
const FREQ_SWEEP_MAX_POINTS: usize = 64;
/// How long the frequency sweep waits for the phase-0 trigger to report the
/// frequency it just commanded, before it gives that point up.
///
/// The drive is a firmware table rebuild plus however long the camera takes to
/// deliver two markers at the new period — at 0.1 Hz that is 20 s on its own.
const FREQ_CONFIRM_BASE_MS: u64 = 20_000;
/// Marker periods that must elapse at the *new* frequency before the sweep
/// believes the measured period. Below this the mean spacing is still a mixture
/// of the old and the new drive.
const FREQ_CONFIRM_CYCLES: f64 = 4.0;

/// Renew a held lease once less than this much of the owner's *granted* window
/// is left.
///
/// Both owners cap the TTL they hand out — a client that dies must not hold the
/// drive indefinitely, so the cap is a dead-man switch and is right. What that
/// means here is that the whole-run TTL a leased run asks for is emphatically
/// not what it gets: ask for forty minutes, be granted a minute. Renewing once
/// per point was therefore only ever correct for points shorter than the cap.
/// A longer one (the shipped example protocol has a 40 s row, and every row
/// also pays the start/stop handshake) ran past the granted deadline mid
/// recording, and the owner did what an expired lease must do — STOP, output
/// off. The run then lost the drive, the phase-0 trigger and the photodiode's
/// optical summary at once, and reported three unrelated-looking failures.
///
/// So the run renews against the deadline the owner actually advertises, not
/// against the one it asked for.
const LEASE_RENEW_MARGIN_MS: u64 = 20_000;
/// Shortest gap between two heartbeat renewals of the same lease. The owner's
/// snapshot lags a renewal by a tick or two, so without this the margin test
/// re-fires on every control tick until the new deadline comes back.
const LEASE_RENEW_MIN_INTERVAL_MS: u64 = 2_000;

/// Absolute/relative tolerance for "the measured `a` reached the sweep target".
fn sweep_tolerance(target_a: f64) -> f64 {
    (target_a * 0.10).max(0.05)
}

/// Commanded optical depth clamped to what the modulation owner accepts.
fn clamp_commanded_a(depth_a: f64) -> f64 {
    if depth_a.is_finite() {
        depth_a.clamp(COMMANDED_A_MIN, COMMANDED_A_MAX)
    } else {
        COMMANDED_A_MIN
    }
}

/// Wire encoding of a commanded optical depth for `SetOpticalDepth`.
fn depth_a_milli(depth_a: f64) -> u32 {
    (depth_a * 1_000.0).round().clamp(0.0, u32::MAX as f64) as u32
}

/// Whether two frequencies name the same sweep point (drive vs trigger readback
/// never agree to the last digit).
fn same_frequency(left: f64, right: f64) -> bool {
    let scale = left.abs().max(right.abs());
    (left - right).abs() <= (scale * FREQUENCY_MATCH_FRACTION).max(1e-6)
}

fn frequency_label(hz: f64) -> String {
    format!("{hz:.3} Hz")
}

/// Upper-cases the first character, so a blocker written as a sentence fragment
/// ("the total power …") can also stand as its own sentence in the status panel.
fn capitalize_first(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Compact file-safe frequency tag for an event-count point's stem:
/// `50 Hz → f50Hz`, `0.5 Hz → f0p5Hz`.
fn frequency_tag(hz: f64) -> String {
    let mut text = format!("{hz:.3}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    format!("f{}Hz", text.replace('.', "p"))
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
    /// One atomic frequency point of the exact-event-count workflow, recorded at
    /// the one frozen depth `a₀` the lock found for that frequency.
    EventCount,
}

impl RecRole {
    /// Filename-stem suffix, empty for a normal sweep point.
    fn suffix(self) -> &'static str {
        match self {
            RecRole::Normal => "",
            RecRole::Pilot => "_pilot",
            RecRole::Background => "_background",
            RecRole::EventCount => "_ec",
        }
    }

    fn label(self) -> &'static str {
        match self {
            RecRole::Normal => "point",
            RecRole::Pilot => "pilot",
            RecRole::Background => "background",
            RecRole::EventCount => "event-count point",
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
    /// Compacted sensor readout written into the measurement folder, if the
    /// host produced any telemetry for this run.
    sensor_readout_path: Option<String>,
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

/// What a leased sweep is for: the amplitude sweep of one `(I_k, f)` row, or one
/// atomic frequency point of the exact-event-count workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SweepKind {
    Amplitude,
    EventCount,
}

impl SweepKind {
    fn role(self) -> RecRole {
        match self {
            SweepKind::Amplitude => RecRole::Normal,
            SweepKind::EventCount => RecRole::EventCount,
        }
    }
}

/// One sweep point: what the drive is *commanded* to, and the
/// photodiode-measured `a` that point is supposed to produce.
///
/// The amplitude sweep asks for its own value open-loop, trusting the Pockels
/// calibration, so both are equal. An event-count point replays a commanded
/// depth the `a₀` lock already trimmed closed-loop against the *measured* depth,
/// so there its commanded depth is deliberately **not** the depth it expects to
/// measure — that difference is the drive roll-off the lock absorbed.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SweepPoint {
    commanded_a: f64,
    expected_a: f64,
}

/// One "record every point of the amplitude range" run: per point the sweep
/// retargets the leased modulation drive, waits for the photodiode-measured
/// `a` to settle, and hands off to the normal recording coordinator.
struct Sweep {
    phase: SweepPhase,
    kind: SweepKind,
    /// The points to record, in order.
    points: Vec<SweepPoint>,
    /// The `a₀` lock an event-count point replays; `None` for the amplitude sweep.
    lock: Option<A0LockPoint>,
    index: usize,
    lease_id: LeaseId,
    lease_granted: bool,
    lease_req: u64,
    /// False when the lease belongs to an enclosing run (the frequency sweep):
    /// then this run neither acquires nor releases it, so the operator's drive
    /// settings stay locked out across the whole ladder rather than only
    /// between its points.
    owns_lease: bool,
    depth_req: u64,
    depth_applied: bool,
    /// Instant the measured `a` first satisfied the tolerance, for the dwell.
    settled_since_ms: Option<u64>,
    /// Give-up deadline for the settle phase.
    settle_deadline_ms: u64,
    /// Whether the current point's recording actually started (vs. was
    /// refused by validation before it began).
    point_started: bool,
    /// Set only on the branch that runs out of points with every one recorded.
    ///
    /// An enclosing frequency ladder has to know whether the inner run it
    /// handed a rung to *finished* or gave up, and it cannot tell from the
    /// recording coordinator: a sweep that aborts on point 4 of 5 leaves
    /// `recording_completed_ok` true from point 3.
    completed_ok: bool,
    last_activity_ms: u64,
    stop_requested: bool,
    /// How much longer this run has to go — see [`crate::eta`].
    eta: Eta,
}

impl Sweep {
    fn point(&self) -> SweepPoint {
        self.points.get(self.index).copied().unwrap_or(SweepPoint {
            commanded_a: 0.0,
            expected_a: 0.0,
        })
    }

    /// The photodiode-measured `a` this point must settle at.
    fn target_a(&self) -> f64 {
        self.point().expected_a
    }

    /// The depth the drive is commanded to for this point.
    fn commanded_a(&self) -> f64 {
        self.point().commanded_a
    }

    fn total(&self) -> usize {
        self.points.len()
    }
}

/// Where the `a₀` lock is within its current closed-loop trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum A0LockPhase {
    /// AcquireLease sent to the modulation owner; waiting for the grant.
    AcquiringLease,
    /// SetOpticalDepth for the current trial sent; waiting for Applied.
    SettingDepth,
    /// Settling, then averaging fresh photodiode readings for this trial.
    Measuring,
}

/// One "find the commanded depth that makes the photodiode measure `a₀` at this
/// frequency" run. Iterates `commanded ← commanded · a₀/measured` under a
/// modulation lease and never records anything itself.
struct A0Lock {
    phase: A0LockPhase,
    /// The photodiode-measured log contrast the operator froze for the sweep.
    target_a: f64,
    /// Convergence band on `|measured − target|`.
    tolerance: f64,
    /// The depth the current trial commands.
    commanded_a: f64,
    /// Frequency this lock belongs to, captured when it started.
    frequency_hz: f64,
    /// 1-based trial counter, bounded by `A0_LOCK_MAX_TRIALS`.
    trial: u32,
    /// Independent photodiode readings collected for the current trial.
    samples: Vec<f64>,
    /// `service_revision` of the newest photodiode summary already sampled, so a
    /// slow publisher is not sampled once per control tick.
    sampled_revision: Option<u64>,
    /// Earliest instant the next reading may be taken: the settle dwell before
    /// the first, then one sample spacing after each.
    measure_from_ms: u64,
    /// Estimator window length (ms) the photodiode reported when this trial
    /// commanded its depth. Both the dwell and the sample spacing derive from
    /// it, because a reading taken sooner still contains the previous depth.
    window_ms: u64,
    /// Give-up deadline for the current trial's measurement.
    deadline_ms: u64,
    lease_id: LeaseId,
    lease_granted: bool,
    lease_req: u64,
    /// See [`Sweep::owns_lease`].
    owns_lease: bool,
    depth_req: u64,
    depth_applied: bool,
    last_activity_ms: u64,
    stop_requested: bool,
}

/// Where the modulation depth `a` that A1 works from comes from.
///
/// `a = ln(I_max / I_min)` is a property of the *light*, so the photodiode is
/// the only source that can state it (ADR 011, and the estimator's own module
/// docs). That is the default and stays the source of record.
///
/// The bench cannot always deliver it, though. The photodiode withholds `a`
/// whenever its estimator window cannot be proven to cover whole modulation
/// cycles, which needs firmware phase-0 markers on the stream port; without
/// them — no trigger cable, a firmware build that does not stamp them, a
/// frequency low enough that two cycles do not fit in the ring — every gate
/// that needs `a` refuses, and the whole a₀/sweep workflow is unreachable even
/// though the drive is calibrated and running.
///
/// [`DepthSource::Commanded`] is the pragmatic way through: the modulation
/// owner already inverts a *measured* Pockels transfer curve (`V_null`, `Vπ`)
/// to command a depth, and publishes that depth as
/// `OpticalDriveStateV1::depth_a_milli`. Taking `a` from there is open loop —
/// it is what the drive asked the cell for, not what the light did, so it
/// carries the calibration's error and any drift since — but it is a
/// calibrated number, not a datasheet one, and it lets the workflow run. Every
/// artefact that records an `a` records which source produced it, so a run
/// taken this way is never mistaken for a measured one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum DepthSource {
    /// The photodiode's measured excitation log-contrast.
    #[default]
    Photodiode,
    /// The depth the modulation owner's calibrated optical drive is commanding.
    Commanded,
}

impl DepthSource {
    fn from_index(index: u64) -> Self {
        match index {
            1 => Self::Commanded,
            _ => Self::Photodiode,
        }
    }

    fn index(self) -> u64 {
        match self {
            Self::Photodiode => 0,
            Self::Commanded => 1,
        }
    }

    /// Machine-readable tag written into sidecars and the lock table.
    fn label(self) -> &'static str {
        match self {
            Self::Photodiode => "photodiode_measured",
            Self::Commanded => "modulation_commanded",
        }
    }

    /// The verb the panel uses for a depth from this source: "measured a" is a
    /// claim about the light and must not be printed for a commanded one.
    fn verb(self) -> &'static str {
        match self {
            Self::Photodiode => "measured",
            Self::Commanded => "commanded",
        }
    }

    /// Whether holding one `a₀` across frequencies requires the closed-loop
    /// search (ADR 013), or whether commanding it is enough (ADR 021).
    ///
    /// The lock exists for one reason: the Pockels inversion is measured once
    /// and is therefore *static*, so the depth it actually delivers rolls off
    /// as `f` rises. Holding a **measured** `a₀` across a frequency ladder
    /// means re-finding the commanded depth that produces it at every point —
    /// `a_cmd ← a_cmd · a₀/a_measured`, a couple of trials per frequency.
    ///
    /// None of that applies to a **commanded** depth, because it *is* the
    /// number being commanded. A search would command `a₀`, read back `a₀`,
    /// converge on trial one, and store one identical row per frequency: pure
    /// ceremony between the operator and a recording, and worse than nothing
    /// once a stale row from a measured run warm-starts it (`begin_a0_lock`)
    /// and drags a closed-loop number into an open-loop point.
    fn needs_a0_lock(self) -> bool {
        matches!(self, Self::Photodiode)
    }
}

/// The result of one lock: the commanded depth that produced the frozen `a₀` at
/// one frequency. Persisted in `a0_locks.json` and replayed by event-count points.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct A0LockPoint {
    frequency_hz: f64,
    /// The frozen `a₀` the lock aimed at.
    target_a: f64,
    /// What the drive must be commanded to in order to *measure* `target_a`.
    commanded_a: f64,
    /// The `a` observed over the final trial, from `depth_source`.
    measured_a: f64,
    trials: u32,
    /// False when the lock ran out of trials or hit a drive limit; such a row is
    /// kept for the record but never arms an event-count recording.
    converged: bool,
    locked_at_unix_ms: u64,
    low_clip_fraction: Option<f64>,
    high_clip_fraction: Option<f64>,
    /// Which source produced `measured_a`. Lock tables written before the
    /// setting existed were all photodiode-measured, which is the default.
    #[serde(default)]
    depth_source: DepthSource,
}

/// Order the planned frequencies are actually visited in.
///
/// A Bode ladder recorded strictly low-to-high confounds frequency with
/// everything that drifts monotonically during the block — bleaching, thermal
/// drift of the Pockels bias, source ageing. The A1 checklist therefore asks
/// for a randomised or alternating schedule, and for the seed to be part of the
/// frozen session plan; both are reproduced in the sidecar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum FreqOrder {
    #[default]
    Ascending,
    Descending,
    /// Lowest, highest, second lowest, second highest, … — a deterministic
    /// alternation that decorrelates frequency from time without a seed.
    Alternating,
    /// Seeded shuffle; the seed is an operator setting and is recorded.
    Random,
}

impl FreqOrder {
    fn from_index(index: u64) -> Self {
        match index {
            1 => Self::Descending,
            2 => Self::Alternating,
            3 => Self::Random,
            _ => Self::Ascending,
        }
    }

    fn index(self) -> u64 {
        match self {
            Self::Ascending => 0,
            Self::Descending => 1,
            Self::Alternating => 2,
            Self::Random => 3,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
            Self::Alternating => "alternating",
            Self::Random => "random",
        }
    }
}

/// What the frequency ladder records at each of its frequencies.
///
/// The ladder is an outer loop over `f` that leases the drive once and hands
/// each confirmed frequency to an inner run. What that inner run *is* is the
/// only thing separating the bench's two multi-frequency experiments, so it is
/// one enum rather than two copies of the ladder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum FreqSweepMode {
    /// One event-count point at the frozen depth `a₀` (ADR 013 / ADR 014):
    /// the same depth everywhere, so a change in the event count is a
    /// frequency effect.
    #[default]
    A0Point,
    /// The whole depth sweep over `[min_a, max_a]` at every frequency
    /// (ADR 023) — one `q_p(a)` curve per `f`, i.e. the `q_p(a, f)` surface
    /// the offline `a50(f)` fit is read from.
    DepthSweep,
}

impl FreqSweepMode {
    fn label(self) -> &'static str {
        match self {
            Self::A0Point => "a₀ point",
            Self::DepthSweep => "depth sweep",
        }
    }

    /// Whether a rung has to find a depth before it can record one.
    ///
    /// Only the `a₀` experiment does: it replays a single depth that something
    /// has to have chosen. A depth sweep commands every `a` in its range
    /// itself and settles on each, so there is nothing for a lock to add — in
    /// either depth source.
    fn needs_armed_depth(self) -> bool {
        matches!(self, Self::A0Point)
    }
}

/// One stop of the frequency sweep.
#[derive(Debug, Clone, Copy, PartialEq)]
struct FreqSweepPoint {
    frequency_hz: f64,
    /// True for the interleaved low-frequency reference repeats, which exist to
    /// expose drift across the block rather than to add a new frequency.
    is_reference: bool,
}

/// Where the multi-frequency run is within its per-point cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreqSweepPhase {
    /// AcquireLease sent to the modulation owner; waiting for the grant.
    AcquiringLease,
    /// SetDriveFrequency for the current point sent; waiting for Applied.
    SettingFrequency,
    /// Waiting for the phase-0 trigger to actually report the new period.
    ConfirmingFrequency,
    /// The `a₀` lock owns this phase.
    Locking,
    /// The one-point event-count sweep owns this phase.
    Recording,
}

/// One "find a₀ and record a point at every frequency" run.
///
/// It is a supervisor, not a third copy of the machinery: per point it
/// retargets the leased drive's frequency, waits for the trigger to confirm it,
/// then hands off to the unchanged `a₀` lock and the unchanged event-count
/// point — both running on *this* run's lease, so the operator's drive settings
/// stay locked out from the first frequency to the last.
struct FreqSweep {
    phase: FreqSweepPhase,
    /// What each rung records — see [`FreqSweepMode`].
    mode: FreqSweepMode,
    points: Vec<FreqSweepPoint>,
    index: usize,
    lease_id: LeaseId,
    lease_granted: bool,
    lease_req: u64,
    freq_req: u64,
    freq_applied: bool,
    /// Give-up deadline for the trigger to confirm the commanded frequency.
    confirm_deadline_ms: u64,
    /// Why the current point is being given up, when that was decided in a
    /// service reply rather than in the tick. Carries the owner's own wording
    /// through to the skip message instead of replacing it with a timeout.
    skip_reason: Option<String>,
    /// Points whose `a₀` could not be locked or whose recording failed. Kept
    /// and reported rather than aborting the ladder: the remaining frequencies
    /// are still worth having, and the lock table already carries the detail.
    failed: Vec<f64>,
    recorded: usize,
    order: FreqOrder,
    seed: u64,
    last_activity_ms: u64,
    stop_requested: bool,
    /// How much longer the *ladder* has to go — see [`crate::eta`]. The inner
    /// run keeps one of its own; only the outermost is ever shown.
    eta: Eta,
}

impl FreqSweep {
    fn point(&self) -> Option<FreqSweepPoint> {
        self.points.get(self.index).copied()
    }

    fn frequency_hz(&self) -> f64 {
        self.point().map(|point| point.frequency_hz).unwrap_or(0.0)
    }
}

/// Where a protocol run is within its per-point cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProtocolPhase {
    /// AcquireLease sent to the modulation owner; waiting for the grant.
    AcquiringLease,
    /// The three retargets for this point are in flight; waiting for all of
    /// them to come back Applied.
    Retargeting,
    /// Dwelling for the point's own settle time before the recording starts.
    Settling,
    /// The recording coordinator owns this phase.
    Recording,
}

/// One protocol run: walk the parsed points, retargeting all three axes at
/// each, on a single lease held for the whole file.
///
/// It is a supervisor like [`FreqSweep`], not a fourth copy of the recording
/// machinery: it moves the drive and then hands off to the same
/// `begin_recording` every button uses. The difference from the sweep buttons
/// is that a protocol names *every* axis for *every* point, so nothing is left
/// implicitly at whatever the operator last armed.
struct ProtocolRun {
    plan: protocol::Protocol,
    phase: ProtocolPhase,
    index: usize,
    lease_id: LeaseId,
    lease_granted: bool,
    lease_req: u64,
    /// Request ids of the retargets in flight for the current point. A point
    /// only proceeds once this is empty: the three axes are applied
    /// independently, and recording after two of them would file the run under
    /// parameters the bench was not actually at.
    pending_reqs: Vec<u64>,
    /// Wall-clock instant the dwell ends.
    settle_until_ms: u64,
    /// Points whose retarget or recording failed, with the owner's own reason.
    ///
    /// Kept rather than aborting — the rest of the survey is still worth
    /// having — and kept *with the reason*, because the per-point message is
    /// overwritten by the next point within the same tick. Without this the
    /// only thing an unattended run could report at the end was a count.
    failed: Vec<(usize, String)>,
    recorded: usize,
    last_activity_ms: u64,
    stop_requested: bool,
    /// Why the current point is being given up, when that was decided in a
    /// service reply rather than in the tick.
    skip_reason: Option<String>,
    /// How much longer the survey has to go — see [`crate::eta`].
    eta: Eta,
}

impl ProtocolRun {
    fn point(&self) -> Option<&protocol::ProtocolPoint> {
        self.plan.points.get(self.index)
    }
}

/// On-disk form of the per-frequency lock table.
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize)]
struct A0LockTable {
    locks: Vec<A0LockPoint>,
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
    frame_width: u16,
    frame_height: u16,
    /// Memoised [`StageAA1Plugin::current_fold`], keyed on its inputs.
    fold_cache: RefCell<Option<(FoldKey, Option<PhaseFold>)>>,
    // -- host camera ROI/mask, mirrored from CTX_GLOBAL_SETTINGS --
    host_roi: Option<RoiV1>,
    masked_pixels: HashSet<(u16, u16)>,
    /// Latest sensor-measured die temperature, pixel dead time and scene
    /// illumination, mirrored from `CTX_SENSOR_MONITORING` every frame.
    ///
    /// Provenance only — never an input to any result. The host publishes it
    /// solely while streaming from a camera with a monitoring block, so replay
    /// and offline re-runs of the same data carry `None`, and a plugin whose
    /// *answers* depended on it would disagree with itself between the two.
    sensor: Option<SensorMonitoringV1>,
    /// [`Self::sensor`] frozen when the current recording started.
    ///
    /// Written into the sidecar in preference to the live value: these drift
    /// (the die warms, the room lights change), so the number that belongs to a
    /// run is the one that held when it began, not the one that happens to be
    /// current when the file is finalized seconds later.
    sensor_at_start: Option<SensorMonitoringV1>,
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
    /// Where the depth `a` comes from — the photodiode's measurement, or the
    /// modulation owner's commanded depth. See [`DepthSource`].
    depth_source: DepthSource,
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
    /// Whether the most recent inner sweep ran out of points with all of them
    /// recorded, as opposed to giving up. Read by the frequency ladder to
    /// decide between advancing and skipping the rung. See [`Sweep::completed_ok`].
    last_sweep_completed_ok: bool,
    // -- exact event-count depth a₀ (ADR 013) --
    /// The one photodiode-measured log contrast held across the frequency sweep.
    a0_target: f64,
    /// Convergence band on `|measured a − a₀|` for the lock and for an
    /// event-count point's settle check.
    a0_tolerance: f64,
    /// Latched by the Find a₀ button, consumed next control tick.
    a0_lock_pending: bool,
    /// Latched by the Record a₀ point button, consumed next control tick.
    a0_point_pending: bool,
    a0_lock: Option<A0Lock>,
    // -- multi-frequency run over the a₀ ladder --
    /// Frequency range and resolution of the planned ladder. Log-spaced: a Bode
    /// ladder is read per decade, not per hertz.
    min_f: f64,
    max_f: f64,
    freq_count: u32,
    freq_order: FreqOrder,
    freq_seed: u64,
    /// Insert the lowest planned frequency again after every N points, so drift
    /// across the block shows up as a disagreement between its repeats. 0 = off.
    freq_reference_every: u32,
    /// Latched by whichever frequency-ladder button was pressed, carrying what
    /// that button asked for. Consumed next control tick.
    freq_sweep_pending: Option<FreqSweepMode>,
    freq_sweep: Option<FreqSweep>,
    // -- declarative protocol runs --
    /// Path of the TOML protocol file to run.
    protocol_path: String,
    /// Latched by the Run protocol button, consumed next control tick.
    protocol_pending: bool,
    /// Recording length for the *next* run, overriding the panel's setting.
    ///
    /// The protocol's own `duration_s` has to win, or a survey's lengths would
    /// silently come from the UI and the file would not describe what it
    /// produced. It cannot be written into `duration_s` itself: the host
    /// re-applies the whole settings snapshot from the UI mirror on every pass,
    /// so an operator setting assigned on the worker is reverted within the
    /// frame — the same trap that made the modulation Apply button look dead.
    pending_duration_s: Option<i64>,
    protocol: Option<ProtocolRun>,
    /// One converged (or attempted) lock per frequency, newest per frequency
    /// wins; mirrored to `a0_locks.json` in the output folder.
    a0_locks: Vec<A0LockPoint>,
    /// Output folder the lock table was last read for, so it is re-read only
    /// when the experiment folder changes.
    loaded_locks_folder: Option<String>,
    /// Whether the last finished recording produced no sensor readout, so the
    /// panel can name the host switch that governs it. Observed rather than
    /// asked: the host does not publish whether it is recording telemetry.
    last_run_had_no_readout: bool,
    // -- lease heartbeats (see LEASE_RENEW_MARGIN_MS) --
    /// When the modulation lease was last renewed by the heartbeat.
    mod_renewed_ms: u64,
    /// When the photodiode lease was last renewed by the heartbeat.
    pd_renewed_ms: u64,
    // -- momentary-button press forwarding (see PressLatch) --
    press_start: PressLatch,
    press_pilot: PressLatch,
    press_background: PressLatch,
    press_stop: PressLatch,
    press_sweep: PressLatch,
    press_clear: PressLatch,
    press_record_point: PressLatch,
    press_clear_curve: PressLatch,
    press_find_a0: PressLatch,
    press_freq_sweep: PressLatch,
    press_freq_depth_sweep: PressLatch,
    press_run_protocol: PressLatch,
    press_record_a0: PressLatch,
    press_clear_a0: PressLatch,
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
            fold_cache: RefCell::new(None),
            frame_width: 0,
            frame_height: 0,
            host_roi: None,
            masked_pixels: HashSet::new(),
            sensor: None,
            sensor_at_start: None,
            window_floor: DEFAULT_WINDOW_FLOOR,
            response_points: Vec::new(),
            pilot_windows: None,
            background_floor: None,
            output_folder: String::new(),
            measurement_id: generate_measurement_id(),
            depth_source: DepthSource::Photodiode,
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
            last_sweep_completed_ok: false,
            // No numerical a₀ is frozen in the repository: this default is a
            // placeholder the operator replaces with the scout result.
            a0_target: 0.5,
            a0_tolerance: 0.02,
            a0_lock_pending: false,
            a0_point_pending: false,
            a0_lock: None,
            min_f: 1.0,
            max_f: 100.0,
            freq_count: 7,
            freq_order: FreqOrder::Alternating,
            freq_seed: 1,
            freq_reference_every: 0,
            freq_sweep_pending: None,
            freq_sweep: None,
            protocol_path: String::new(),
            protocol_pending: false,
            pending_duration_s: None,
            protocol: None,
            a0_locks: Vec::new(),
            loaded_locks_folder: None,
            last_run_had_no_readout: false,
            mod_renewed_ms: 0,
            pd_renewed_ms: 0,
            press_start: PressLatch::default(),
            press_pilot: PressLatch::default(),
            press_background: PressLatch::default(),
            press_stop: PressLatch::default(),
            press_sweep: PressLatch::default(),
            press_clear: PressLatch::default(),
            press_record_point: PressLatch::default(),
            press_clear_curve: PressLatch::default(),
            press_find_a0: PressLatch::default(),
            press_freq_sweep: PressLatch::default(),
            press_freq_depth_sweep: PressLatch::default(),
            press_run_protocol: PressLatch::default(),
            press_record_a0: PressLatch::default(),
            press_clear_a0: PressLatch::default(),
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
            sensor_readout_path: None,
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
            RecPhase::Idle => "not recording",
            RecPhase::StartingCamera => "starting the camera",
            RecPhase::ConnectingPhotodiode => "connecting the photodiode",
            RecPhase::AcquiringLease => "reserving the photodiode",
            RecPhase::StartingPhotodiode => "starting the photodiode",
            RecPhase::Running => "recording",
            RecPhase::StoppingPhotodiode => "saving the photodiode data",
            RecPhase::StoppingCamera => "saving the camera data",
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

/// Fingerprint of everything the phase fold is computed from. Cheap to build
/// (no scan of the event buffer) and exact enough that a stale fold cannot
/// survive a change to any input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FoldKey {
    period_us_bits: u64,
    event_count: usize,
    first_event_us: Option<u64>,
    last_event_us: Option<u64>,
    marker_count: usize,
    first_marker_us: Option<u64>,
    last_marker_us: Option<u64>,
    roi: Option<Roi>,
    masked_count: usize,
}

impl StageAA1Plugin {
    fn bump(&mut self) {
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
    }

    /// Releases the live analysis buffers and the fold memoised from them.
    /// Returns whether anything was actually held.
    ///
    /// Assigning fresh `Vec`s rather than calling `clear()` is deliberate: the
    /// event buffer reaches millions of entries, and `clear()` keeps every byte
    /// of that capacity reserved.
    ///
    /// This is what switching Live analysis off has to do. Merely stopping the
    /// *filling* left the last window's events live for every later
    /// `current_fold()`, and the control tick re-folds them — so turning the
    /// toggle off left the plugin folding millions of stale events on every
    /// tick, forever. That is the lag that outlived the switch.
    fn drop_live_buffers(&mut self) -> bool {
        let held = !self.camera_events.is_empty() || !self.camera_markers_us.is_empty();
        self.camera_events = Vec::new();
        self.event_scratch = Vec::new();
        self.camera_markers_us = Vec::new();
        self.fold_cache.replace(None);
        held
    }

    /// One Stop for everything the Record section can start.
    ///
    /// Whatever is in flight — a single recording, a depth sweep, an `a₀`
    /// lock, a frequency ladder or a protocol — asks it to wind down at its
    /// next safe point, and any latched-but-not-yet-started press is dropped so
    /// the stop is not immediately undone by a queued start.
    fn request_stop(&mut self) {
        if self.recording.is_active() {
            self.recording.stop_requested = true;
        }
        if let Some(sweep) = self.sweep.as_mut() {
            sweep.stop_requested = true;
            self.message = "Sweep stop requested".into();
        }
        if let Some(lock) = self.a0_lock.as_mut() {
            lock.stop_requested = true;
            self.message = "a₀ lock stop requested".into();
        }
        // Before the protocol, so the outer runner's wording wins over the
        // child it happens to be driving.
        if let Some(sweep) = self.freq_sweep.as_mut() {
            sweep.stop_requested = true;
            self.message = "Frequency sweep stop requested".into();
        }
        if let Some(protocol) = self.protocol.as_mut() {
            protocol.stop_requested = true;
            self.message = "Protocol stop requested".into();
        }
        self.sweep_pending = false;
        self.a0_lock_pending = false;
        self.a0_point_pending = false;
        self.freq_sweep_pending = None;
        self.protocol_pending = false;
    }

    /// Whether anything the Record section started is still in flight.
    ///
    /// The same set [`Self::request_stop`] winds down. A discontinuity that
    /// arrives between two points of a run is still *inside* that run, so it
    /// must not be treated as an idle-time reset.
    fn automation_active(&self) -> bool {
        self.recording.is_active()
            || self.sweep.is_some()
            || self.a0_lock.is_some()
            || self.freq_sweep.is_some()
            || self.protocol.is_some()
    }

    /// The modulation lease this run holds, and how much longer it still needs
    /// it. Outermost runner first — a nested run inherits the enclosing lease
    /// id, so the outermost one names the lease and owns the remaining time.
    ///
    /// `None` until the owner has granted it: renewing a lease that does not
    /// exist yet is rejected, and the acquire is already in flight.
    fn held_modulation_lease(&self) -> Option<(LeaseId, u64)> {
        if let Some(run) = self.protocol.as_ref().filter(|run| run.lease_granted) {
            return Some((
                run.lease_id.clone(),
                Self::protocol_lease_ttl_ms(&run.plan, run.index),
            ));
        }
        if let Some(sweep) = self.freq_sweep.as_ref().filter(|s| s.lease_granted) {
            let remaining = sweep.points.len().saturating_sub(sweep.index);
            return Some((
                sweep.lease_id.clone(),
                self.freq_sweep_lease_ttl_ms(remaining, sweep.mode),
            ));
        }
        if let Some(lock) = self.a0_lock.as_ref().filter(|l| l.lease_granted) {
            return Some((lock.lease_id.clone(), self.a0_lock_lease_ttl_ms()));
        }
        if let Some(sweep) = self.sweep.as_ref().filter(|s| s.lease_granted) {
            let remaining = sweep.total().saturating_sub(sweep.index);
            return Some((sweep.lease_id.clone(), self.sweep_lease_ttl_ms(remaining)));
        }
        None
    }

    /// Whether `lease` is the one A1 is holding right now, per the owner's own
    /// snapshot. Renewing on our own bookkeeping alone would keep re-asking
    /// after the owner had already dropped it.
    fn owner_holds(lease: Option<&LeaseSnapshotV1>, held: &LeaseId) -> Option<u64> {
        let lease = lease?;
        (&lease.lease_id == held && lease.holder.as_str() == A1_PLUGIN_ID)
            .then_some(lease.expires_at_unix_ms)
    }

    /// Keep both leases alive against the deadline each owner advertises.
    ///
    /// Runs on every control tick, ahead of the runners: the owners cap the TTL
    /// they grant well below the length of a survey (see
    /// [`LEASE_RENEW_MARGIN_MS`]), so a run that renewed only when it moved to
    /// its next point lost the drive in the middle of any point longer than the
    /// cap.
    fn drive_lease_heartbeat(&mut self, context: &mut impl RecordingControl) {
        let now_ms = now_unix_ms();
        let due = |last_ms: u64, expires_at: u64| {
            now_ms.saturating_sub(last_ms) >= LEASE_RENEW_MIN_INTERVAL_MS
                && expires_at.saturating_sub(now_ms) <= LEASE_RENEW_MARGIN_MS
        };

        if let Some((lease_id, ttl_ms)) = self.held_modulation_lease() {
            let expires_at = Self::owner_holds(
                self.modulation.as_ref().and_then(|s| s.lease.as_ref()),
                &lease_id,
            );
            if expires_at.is_some_and(|at| due(self.mod_renewed_ms, at)) {
                let request =
                    self.modulation_request(ModulationCommandV1::RenewLease { ttl_ms }, &lease_id);
                context.request_service(&request);
                self.mod_renewed_ms = now_ms;
            }
        }

        // The photodiode lease covers one recording, and A1 asks for its
        // duration plus slack — which the owner caps just as hard, so any
        // recording longer than the cap used to be finalized underneath itself
        // as `LeaseExpired` and left no optical summary for the sidecar.
        if self.recording.is_active() && self.recording.lease_granted {
            let held = self.recording.lease_id.clone();
            let expires_at = Self::owner_holds(
                self.photodiode.as_ref().and_then(|s| s.lease.as_ref()),
                &held,
            );
            if expires_at.is_some_and(|at| due(self.pd_renewed_ms, at)) {
                let ttl_ms = self.photodiode_lease_ttl_ms();
                let request = self.photodiode_request(PhotodiodeCommandV1::RenewLease { ttl_ms });
                context.request_service(&request);
                self.pd_renewed_ms = now_ms;
            }
        }
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

    /// Modulation frequency implied by [`Self::period_us`].
    fn frequency_hz(&self) -> Option<f64> {
        self.period_us().map(|period| 1_000_000.0 / period)
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

    /// Why there is no modulation frequency to work from, phrased as the
    /// operator action that fixes it. `None` means the frequency is known.
    ///
    /// A frequency comes from two independent places — the phase-0 trigger
    /// markers, or the drive the modulation plugin has *acknowledged*. Neither
    /// has anything to do with whether that plugin is connected, which is a
    /// separate check ([`Self::modulation_connected`]). Reporting "connect the
    /// modulation plugin" for a missing frequency told operators their bench
    /// was unplugged when it was not: the usual cause is simply that no
    /// periodic drive has been armed yet.
    fn frequency_blocker(&self) -> Option<String> {
        if self.frequency_hz().is_some() {
            return None;
        }
        let Some(state) = self.modulation.as_ref() else {
            return Some(
                "the modulation plugin is not reporting status — enable it in the plugin list"
                    .into(),
            );
        };
        if !matches!(state.connection, ConnectionStateV1::Connected { .. }) {
            return Some(format!(
                "the modulation plugin is {} — connect it",
                connection_label(&state.connection)
            ));
        }
        // Connected, so the question is what it is being asked to drive.
        let Some(target) = state.acknowledged.as_ref() else {
            return Some(
                "the modulation plugin has not applied a drive yet — set one up there and apply it"
                    .into(),
            );
        };
        match target.waveform.as_ref() {
            Some(WaveformV1::Periodic {
                frequency_millihz, ..
            }) if *frequency_millihz == 0 => {
                Some("the modulation drive frequency is set to 0 — raise it".into())
            }
            Some(WaveformV1::Periodic { .. }) => None,
            Some(_) => Some(
                "the modulation drive is not a repeating waveform, so it has no frequency — \
                 choose a periodic one"
                    .into(),
            ),
            None => Some(
                "the modulation plugin has not applied a waveform yet — set one up there and \
                 apply it"
                    .into(),
            ),
        }
    }

    fn frequency_source(&self) -> &'static str {
        if self.measured_period_us().is_some() {
            "trigger"
        } else {
            "modulation"
        }
    }

    /// The current phase fold, memoised.
    ///
    /// Called several times per repaint (`rolling_dataset`, `latest_rolling`
    /// from both `status_dataset` and `status_entries`, `current_windows`,
    /// `current_response`). Each fold allocates a `Vec<FoldedEvent>` over up to
    /// `MAX_EVENTS` events, so refolding per call threw away hundreds of
    /// megabytes per repaint at bench event rates. The cache is keyed on a
    /// cheap fingerprint of everything the fold reads, so it invalidates
    /// exactly when the inputs move rather than on every `bump()`.
    fn current_fold(&self) -> Option<PhaseFold> {
        let key = self.fold_key()?;
        if let Ok(cache) = self.fold_cache.try_borrow() {
            if let Some((cached_key, fold)) = cache.as_ref() {
                if *cached_key == key {
                    return fold.clone();
                }
            }
        }
        let fold = self.compute_fold();
        if let Ok(mut cache) = self.fold_cache.try_borrow_mut() {
            *cache = Some((key, fold.clone()));
        }
        fold
    }

    /// Fingerprint of every input [`Self::compute_fold`] reads. `None` when
    /// there is no period, i.e. no fold to compute.
    fn fold_key(&self) -> Option<FoldKey> {
        let period_us = self.period_us()?;
        Some(FoldKey {
            period_us_bits: period_us.to_bits(),
            event_count: self.camera_events.len(),
            first_event_us: self.camera_events.first().map(|event| event.timestamp_us),
            last_event_us: self.camera_events.last().map(|event| event.timestamp_us),
            marker_count: self.camera_markers_us.len(),
            first_marker_us: self.camera_markers_us.first().copied(),
            last_marker_us: self.camera_markers_us.last().copied(),
            roi: self.roi(),
            masked_count: self.masked_pixels.len(),
        })
    }

    fn compute_fold(&self) -> Option<PhaseFold> {
        let period_us = self.period_us()?;
        // Fold only the events the analysis is normalised over. `q_p` already
        // restricts to ROI minus masked pixels; the rolling response divides by
        // the same count, so its numerator has to be restricted too or it
        // counts events from outside the ROI against an ROI-sized denominator.
        let events = self.roi_filtered_events();
        let marker_fold = self.is_marker_anchored().then(|| {
            let expected_hz = 1_000_000.0 / period_us;
            fold_events(
                &events,
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
            .or_else(|| fold_events_free_running(&events, period_us))
    }

    /// The analysis-window events restricted to the ROI, masked pixels removed.
    /// Without an ROI the whole frame is the ROI, so this is a clone.
    fn roi_filtered_events(&self) -> Vec<CameraEvent> {
        let Some(roi) = self.roi() else {
            return self.camera_events.clone();
        };
        if roi.area() == usize::from(self.frame_width) * usize::from(self.frame_height)
            && self.masked_pixels.is_empty()
        {
            return self.camera_events.clone();
        }
        self.camera_events
            .iter()
            .filter(|event| {
                roi.contains(event.x, event.y) && !self.masked_pixels.contains(&(event.x, event.y))
            })
            .copied()
            .collect()
    }

    /// Fresh optical summary from a connected photodiode owner.
    fn fresh_optical_summary(&self) -> Option<&PhotodiodeOpticalSummaryV1> {
        let state = self.photodiode.as_ref()?;
        if !matches!(state.connection, ConnectionStateV1::Connected { .. })
            || state.freshness.is_stale_at(now_unix_ms())
        {
            return None;
        }
        state.optical_summary.as_ref()
    }

    /// Fresh optical modulation depth `a` published by the photodiode plugin.
    fn photodiode_a(&self) -> Option<f64> {
        self.fresh_optical_summary()
            .map(|summary| summary.measured_log_contrast)
    }

    /// The depth `a` the modulation owner's calibrated optical drive is
    /// currently commanding, from its published inversion provenance.
    ///
    /// `None` unless the owner is connected and has a calibrated optical drive
    /// armed: `optical_drive` is published only for `OPTICAL_LOG_SINE` /
    /// `OPTICAL_LINEAR_SINE` under an identified transfer calibration, which is
    /// exactly the case in which a commanded `a` means anything at all. A
    /// manual DAC band or a constant level publishes nothing here, and must not
    /// be turned into a depth.
    ///
    /// Deliberately not freshness-gated. The published depth is what this
    /// plugin's own `SetOpticalDepth` wrote into the owner a moment ago, not a
    /// reading off the bench, so it does not go stale the way a measurement
    /// does — and gating it on the device poll would make the sweep's settle
    /// check flap between "settled" and "no a" on a slow reply.
    fn commanded_a(&self) -> Option<f64> {
        let state = self.modulation.as_ref()?;
        if !matches!(state.connection, ConnectionStateV1::Connected { .. }) {
            return None;
        }
        let depth = f64::from(state.optical_drive.as_ref()?.depth_a_milli) / 1_000.0;
        (depth > 0.0).then_some(depth)
    }

    /// The modulation depth `a` A1 works from, per the operator's chosen
    /// [`DepthSource`]. Every gate, sweep settle check, plot and sidecar reads
    /// this one accessor, so the source is chosen in exactly one place.
    fn depth_a(&self) -> Option<f64> {
        match self.depth_source {
            DepthSource::Photodiode => self.photodiode_a(),
            DepthSource::Commanded => self.commanded_a(),
        }
    }

    /// Why there is no `a` to work from, phrased as the operator action that
    /// fixes it.
    ///
    /// Every gate that needs `a` — the a₀ lock, the amplitude sweep, the
    /// frequency ladder — used to refuse with one fixed sentence naming the two
    /// most common causes. When the real cause was a third thing (a railed
    /// window, too few trigger markers, a stale snapshot) that sentence sent the
    /// operator to re-check an anchor that was already fine. Ask the owner
    /// instead, and only fall back to the local view of the snapshot.
    ///
    /// `None` means `a` is available.
    fn depth_a_blocker(&self) -> Option<String> {
        match self.depth_source {
            DepthSource::Photodiode => self.photodiode_a_blocker(),
            DepthSource::Commanded => self.commanded_a_blocker(),
        }
    }

    fn photodiode_a_blocker(&self) -> Option<String> {
        if self.photodiode_a().is_some() {
            return None;
        }
        // Every branch names the photodiode gate to fix *and* the way past it,
        // because a bench that cannot produce a measured `a` at all — no
        // trigger markers, say — otherwise leaves the operator with a correct
        // diagnosis and no next step.
        let fallback = " (or switch \"Depth a source\" to the commanded drive to work open loop)";
        let Some(state) = self.photodiode.as_ref() else {
            return Some(format!(
                "the photodiode plugin is not reporting status — enable it and connect the \
                 detector{fallback}"
            ));
        };
        if !matches!(state.connection, ConnectionStateV1::Connected { .. }) {
            return Some(format!(
                "the photodiode is {} — connect it{fallback}",
                connection_label(&state.connection)
            ));
        }
        if state.freshness.is_stale_at(now_unix_ms()) {
            return Some(format!(
                "the photodiode status snapshot is stale — check that the stream is \
                 running{fallback}"
            ));
        }
        // The owner's own words: it is the only side that knows which estimator
        // gate rejected the window.
        if let Some(reason) = state.optical_unavailable.as_deref() {
            return Some(format!("{reason}{fallback}"));
        }
        Some(format!(
            "the photodiode is streaming no samples yet — start the stream{fallback}"
        ))
    }

    fn commanded_a_blocker(&self) -> Option<String> {
        if self.commanded_a().is_some() {
            return None;
        }
        let Some(state) = self.modulation.as_ref() else {
            return Some(
                "the modulation plugin is not reporting status — enable it to read the commanded \
                 depth"
                    .into(),
            );
        };
        if !matches!(state.connection, ConnectionStateV1::Connected { .. }) {
            return Some(format!(
                "the modulation plugin is {} — connect it",
                connection_label(&state.connection)
            ));
        }
        Some(
            "the modulation plugin is not running a calibrated optical drive — apply a Pockels \
             calibration and arm OPTICAL_LOG_SINE, or a commanded depth means nothing"
                .into(),
        )
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
                // Drop whatever was loaded for this measurement: leaving it in
                // place let `write_sidecar` record windows from an *earlier*
                // pilot as if they had just been frozen from this run.
                self.pilot_windows = None;
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

    /// Records one response-curve point at the current depth `a`.
    fn record_response_point(&mut self) -> Result<(), String> {
        let measured_a = self.depth_a().ok_or_else(|| {
            format!(
                "no modulation depth a available: {}",
                self.depth_a_blocker()
                    .unwrap_or_else(|| "no depth source is reporting".into())
            )
        })?;
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
            x_label: format!(
                "Modulation depth a = ln(I_max / I_min) ({})",
                self.depth_source.verb()
            ),
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
        // Same denominator as `q_p` (ROI minus masked), against the ROI-filtered
        // fold — the two are shown side by side and must mean the same thing.
        let Some(valid_pixels) = self.valid_pixel_count() else {
            return empty();
        };
        let line = |polarity: Polarity| {
            rolling_half_period_response(&fold, polarity, valid_pixels, &sample_times, None)
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
        let valid_pixels = self.valid_pixel_count()?;
        let value = |polarity| {
            rolling_half_period_response(&fold, polarity, valid_pixels, &at, None)
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
                    self.depth_a().map_or_else(
                        || "—".into(),
                        |a| format!("{a:.3} ({})", self.depth_source.verb()),
                    ),
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
            meta.insert(
                "sweep_commanded_a".into(),
                format!("{:.6}", sweep.commanded_a()),
            );
            meta.insert("sweep_point_index".into(), (sweep.index + 1).to_string());
            meta.insert("sweep_point_total".into(), sweep.total().to_string());
        }
        if let Some(lock) = self.sweep.as_ref().and_then(|sweep| sweep.lock.as_ref()) {
            meta.insert("a0_target".into(), format!("{:.6}", lock.target_a));
            meta.insert("a0_commanded_a".into(), format!("{:.6}", lock.commanded_a));
            meta.insert(
                "a0_lock_measured_a".into(),
                format!("{:.6}", lock.measured_a),
            );
            meta.insert(
                "a0_lock_depth_source".into(),
                lock.depth_source.label().into(),
            );
            meta.insert(
                "a0_lock_frequency_hz".into(),
                format!("{:.6}", lock.frequency_hz),
            );
        }
        // The depth this run was driven and judged by, always tagged with where
        // it came from. `measured_a` keeps its historical meaning — a number the
        // photodiode actually measured — so an open-loop run simply does not
        // carry one, rather than carrying a commanded value under that name.
        meta.insert("depth_a_source".into(), self.depth_source.label().into());
        if let Some(a) = self.depth_a() {
            meta.insert("depth_a".into(), format!("{a:.6}"));
        }
        if let Some(a) = self.photodiode_a() {
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
        // How the drive was actually shaped, not just how fast it ran. The PDQ
        // is the photodiode's own record and travels on its own — a trace whose
        // sidecar names a frequency and two DAC codes cannot say which lobe, or
        // which operating point `ū`, produced it. All of this is already
        // published by the modulation owner; it was simply never written down
        // here, so it lived only in the A1 `_config.toml` next door.
        if let Some(waveform) = self
            .modulation
            .as_ref()
            .and_then(|state| state.acknowledged.as_ref())
            .and_then(|target| target.waveform.as_ref())
        {
            meta.insert("modulation_waveform".into(), waveform_label(waveform));
        }
        if let Some(state) = self.modulation.as_ref() {
            if let Some(calibration) = state.calibration_id.as_ref() {
                meta.insert("pockels_calibration_id".into(), calibration.clone());
            }
            if let Some(drive) = state.optical_drive.as_ref() {
                meta.insert(
                    "optical_target".into(),
                    match drive.target {
                        OpticalTargetV1::LogSine => "log_sine".into(),
                        OpticalTargetV1::LinearSine => "linear_sine".into(),
                    },
                );
                // `ū` is a whole axis of the survey, and the one no other key
                // here implies: two rows can share `f` and `a` and differ only
                // in the operating point they were driven around.
                meta.insert(
                    "requested_mean_u".into(),
                    format!("{:.6}", f64::from(drive.requested_mean_u_milli) / 1_000.0),
                );
                meta.insert(
                    "resolved_mean_u".into(),
                    format!("{:.6}", f64::from(drive.resolved_mean_u_milli) / 1_000.0),
                );
                meta.insert(
                    "commanded_a".into(),
                    format!("{:.6}", f64::from(drive.depth_a_milli) / 1_000.0),
                );
                meta.insert("v_null_dac".into(), drive.v_null_dac.to_string());
                meta.insert("v_peak_dac".into(), drive.v_peak_dac.to_string());
            }
        }
        // Which row of which protocol this run is. Written on both legs so the
        // PDQ and the RAW can each be traced back to the line of the file that
        // asked for them, without joining through the A1 sidecar.
        if let Some(run) = self
            .protocol
            .as_ref()
            .filter(|run| run.phase == ProtocolPhase::Recording)
        {
            if let Some(point) = run.point() {
                // Both are free text out of the operator's file. The photodiode
                // owner rejects the whole recording over an oversized metadata
                // value, so a chatty block name must not be able to cost a run.
                meta.insert("protocol_name".into(), clamp_metadata(&run.plan.name));
                meta.insert("protocol_block".into(), clamp_metadata(&point.block));
                meta.insert("protocol_point_tag".into(), point.tag());
                meta.insert("protocol_point_index".into(), (run.index + 1).to_string());
                meta.insert(
                    "protocol_point_total".into(),
                    run.plan.points.len().to_string(),
                );
                meta.insert("protocol_mean_u".into(), format!("{:.6}", point.mean_u));
                meta.insert(
                    "protocol_frequency_hz".into(),
                    format!("{:.6}", point.frequency_hz),
                );
                meta.insert("protocol_depth_a".into(), format!("{:.6}", point.depth_a));
                meta.insert("protocol_settle_s".into(), format!("{:.3}", point.settle_s));
            }
        }
        if let Some(n) = self.valid_pixel_count() {
            meta.insert("n_valid".into(), n.to_string());
        }
        // Bench conditions, on every run and every role. Each key appears only
        // when the sensor actually reported that quantity — an absent reading
        // must not arrive downstream as 0 °C or 0 lux.
        if let Some(sensor) = self.recorded_sensor() {
            if let Some(celsius) = sensor.temperature_c {
                meta.insert("sensor_temperature_c".into(), format!("{celsius:.2}"));
            }
            if let Some(dead_time_us) = sensor.pixel_dead_time_us {
                meta.insert(
                    "sensor_pixel_dead_time_us".into(),
                    format!("{dead_time_us:.3}"),
                );
            }
            if let Some(lux) = sensor.illumination_lux {
                meta.insert("sensor_illumination_lux".into(), format!("{lux:.3}"));
            }
            meta.insert(
                "sensor_reading_age_s".into(),
                format!("{:.3}", sensor.age_s),
            );
        }
        meta
    }

    /// The sensor reading that belongs to the run being written: the one frozen
    /// when it started, falling back to the latest if the recording began
    /// before any frame carried one.
    fn recorded_sensor(&self) -> Option<SensorMonitoringV1> {
        self.sensor_at_start.or(self.sensor)
    }

    /// The measurement id to file this run under, generating one when the
    /// operator has not typed anything.
    ///
    /// A blank id used to refuse the recording. It never had to: the id only
    /// names a folder and a file stem, and the plugin already ships a generated
    /// default for exactly that reason. Filling it in here (and writing it back,
    /// so the panel shows what was used) means an operator who wants their data
    /// grouped can say so, and one who just wants to record can press record.
    /// Tested on the raw field, not on `sanitize_stem`'s output: the sanitizer
    /// substitutes `A1` for anything that reduces to nothing, so asking it
    /// whether the id was blank always answers no — and every unnamed run would
    /// silently share one folder called `A1`.
    fn ensure_measurement_id(&mut self) -> String {
        if self.measurement_id.trim().is_empty() {
            self.measurement_id = generate_measurement_id();
            self.bump();
        }
        sanitize_stem(self.measurement_id.trim())
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
            self.note("Pick an output folder first — that is where the files go");
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
        // Freeze the bench conditions this run begins under, before any of the
        // start handshake has had time to move them.
        self.sensor_at_start = self.sensor;
        let id = self.ensure_measurement_id();
        // Sweep points get a stable per-point tag so the row's files sort by
        // sweep order as well as by timestamp. Event-count points instead carry
        // their frequency, because one measurement id spans the whole frequency
        // sweep at the single frozen depth a₀.
        let live_hz = self.frequency_hz();
        // A depth sweep nested inside the frequency ladder repeats its point
        // indices at every rung, so `_p03` alone would collide across
        // frequencies within one measurement id. Prefix the ladder's frequency
        // so the whole q_p(a, f) surface sorts by f, then by depth.
        let nested_freq_tag = self
            .freq_sweep
            .as_ref()
            .filter(|sweep| sweep.mode == FreqSweepMode::DepthSweep)
            .map(|sweep| format!("_{}", frequency_tag(sweep.frequency_hz())))
            .unwrap_or_default();
        let sweep_tag = self
            .sweep
            .as_ref()
            .filter(|sweep| sweep.phase == SweepPhase::Recording)
            .map(|sweep| match sweep.kind {
                SweepKind::Amplitude => {
                    format!("{nested_freq_tag}_p{:02}", sweep.index + 1)
                }
                SweepKind::EventCount => {
                    let hz = sweep
                        .lock
                        .as_ref()
                        .map(|lock| lock.frequency_hz)
                        .or(live_hz)
                        .unwrap_or_default();
                    format!("_{}", frequency_tag(hz))
                }
            })
            .unwrap_or_default();
        // A protocol row is not a sweep point, so `sweep_tag` is empty for one:
        // nothing armed in the panel says which of the survey's points this is.
        // Without a tag of its own, every row of a protocol lands under the same
        // stem but for the second it started, and the operating point a file was
        // recorded at can only be recovered by opening its sidecar. The row
        // names all three axes, so its files can too — the 1-based point index
        // first, so they sort in protocol order rather than by `ū`.
        let protocol_tag = self
            .protocol
            .as_ref()
            .filter(|run| run.phase == ProtocolPhase::Recording)
            .and_then(|run| {
                let point = run.point()?;
                let width = run.plan.points.len().to_string().len().max(2);
                Some(format!(
                    "_p{:0width$}_{}",
                    run.index + 1,
                    point.tag(),
                    width = width
                ))
            })
            .unwrap_or_default();
        let stem = format!(
            "{id}_{}{}{sweep_tag}{protocol_tag}",
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
        recording.duration_s = self
            .pending_duration_s
            .take()
            .unwrap_or(self.duration_s)
            .max(1) as u64;
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
            // Both keep the row's pilot-frozen windows and background floor.
            RecRole::Normal | RecRole::EventCount => {}
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

    /// How much longer the photodiode is needed: the recording's own length
    /// plus the start/stop handshake. The owner caps what it grants, so the
    /// heartbeat re-asks — see [`LEASE_RENEW_MARGIN_MS`].
    fn photodiode_lease_ttl_ms(&self) -> u64 {
        self.recording
            .duration_s
            .saturating_mul(1_000)
            .saturating_add(60_000)
    }

    fn acquire_photodiode(&mut self, context: &mut impl RecordingControl) {
        let ttl_ms = self.photodiode_lease_ttl_ms();
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
            // The host's sensor telemetry is written as another sibling of the
            // RAW and used to be left behind entirely, which separated a run
            // from the bench conditions it was taken under at the first move.
            // It is rewritten column-wise on the way in — see `sensor`.
            self.recording.sensor_readout_path = self.gather_sensor_readout(&dir, &raw);
            self.last_run_had_no_readout = self.recording.sensor_readout_path.is_none();
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

    /// Compacts the host's sensor-telemetry CSV into the measurement folder,
    /// under the recording's own stem, and removes the original.
    ///
    /// Best-effort throughout: a missing or unreadable telemetry file is normal
    /// (replay, a camera with no monitoring block, a host that did not poll)
    /// and must not cost the operator the recording that has just finished.
    fn gather_sensor_readout(&self, dir: &Path, raw: &str) -> Option<String> {
        let source = Path::new(raw)
            .file_stem()
            .map(|stem| {
                Path::new(raw)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(format!("{}.sensor-monitoring.csv", stem.to_string_lossy()))
            })
            .filter(|path| path.exists())?;
        let text = std::fs::read_to_string(&source).ok()?;
        let readout = sensor::parse_csv(&text);
        if readout.is_empty() {
            // Nothing worth keeping, but the wide original is still clutter in
            // the host's capture folder.
            let _ = std::fs::remove_file(&source);
            return None;
        }
        let destination = dir.join(format!("{}.sensor.json", self.recording.stem));
        let json = readout.to_json(&self.recording.id, &self.recording.stem);
        if std::fs::write(&destination, json).is_err() {
            return None;
        }
        let _ = std::fs::remove_file(&source);
        Some(destination.display().to_string())
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
    /// The amplitude sweep trusts the calibration, so each point commands the
    /// very depth it expects to measure.
    fn sweep_points(&self) -> Vec<SweepPoint> {
        let count = self.sweep_count.clamp(2, 64) as usize;
        let span = self.max_a - self.min_a;
        (0..count)
            .map(|index| {
                let depth_a = self.min_a + span * index as f64 / (count - 1) as f64;
                SweepPoint {
                    commanded_a: depth_a,
                    expected_a: depth_a,
                }
            })
            .collect()
    }

    // ---- how long a run is planned to take -----------------------------
    //
    // These are deliberately *not* the lease TTLs below. A TTL is a worst case
    // — it has to cover the settle timeout of every point, or the owner takes
    // the drive back mid-run — while an estimate the operator plans an evening
    // around has to be the likely case. Quoting the TTL would tell someone
    // their five-minute sweep needs half an hour. The measured pace in
    // [`crate::eta`] closes whatever gap is left.

    /// What one recorded point costs: the recording itself, the operator's
    /// settle dwell, and the start/finalize handshake around it.
    fn planned_point_s(&self) -> f64 {
        self.duration_s.max(1) as f64 + self.settle_s.max(0.0) + crate::eta::POINT_OVERHEAD_S
    }

    /// What one closed-loop `a₀` lock costs, or zero where the depth is
    /// commanded and no lock runs at all (ADR 021).
    fn planned_a0_lock_s(&self) -> f64 {
        if !self.depth_source.needs_a0_lock() {
            return 0.0;
        }
        A0_LOCK_NOMINAL_TRIALS * (self.settle_s.max(0.0) + A0_LOCK_NOMINAL_TRIAL_S)
    }

    /// What one rung of the frequency ladder costs at `frequency_hz`:
    /// retargeting the drive, waiting for the trigger to confirm the new period
    /// — which is a few cycles, so it is the low frequencies that dominate a
    /// ladder — and then whatever that rung records.
    fn planned_rung_s(&self, mode: FreqSweepMode, frequency_hz: f64) -> f64 {
        let confirm_s = if frequency_hz > 0.0 {
            (FREQ_CONFIRM_CYCLES / frequency_hz).min(FREQ_CONFIRM_BASE_MS as f64 / 1_000.0)
        } else {
            0.0
        };
        let inner_s = match mode {
            FreqSweepMode::A0Point => self.planned_a0_lock_s() + self.planned_point_s(),
            FreqSweepMode::DepthSweep => {
                self.sweep_count.clamp(2, 64) as f64 * self.planned_point_s()
            }
        };
        FREQ_RETARGET_NOMINAL_S + confirm_s + inner_s
    }

    /// Planned seconds a run still has left, counting the point in flight.
    /// `None` when nothing is running.
    fn planned_remaining_s(&self) -> Option<f64> {
        // Outermost first: a ladder's estimate already contains the inner
        // sweep it handed the current rung to, and a protocol contains both.
        if let Some(run) = self.protocol.as_ref() {
            let remaining_points = run.plan.points.len().saturating_sub(run.index);
            return Some(
                run.plan.remaining_seconds(run.index)
                    + remaining_points as f64 * crate::eta::POINT_OVERHEAD_S,
            );
        }
        if let Some(sweep) = self.freq_sweep.as_ref() {
            let from = sweep.index.min(sweep.points.len());
            return Some(
                sweep.points[from..]
                    .iter()
                    .map(|point| self.planned_rung_s(sweep.mode, point.frequency_hz))
                    .sum(),
            );
        }
        if let Some(sweep) = self.sweep.as_ref() {
            let remaining = sweep.total().saturating_sub(sweep.index);
            return Some(remaining as f64 * self.planned_point_s());
        }
        None
    }

    /// The estimate itself: the [`Eta`] of the outermost run in flight, paired
    /// with what its plan still asks for.
    fn run_eta(&self) -> Option<(Eta, f64)> {
        let planned_remaining_s = self.planned_remaining_s()?;
        let eta = if let Some(run) = self.protocol.as_ref() {
            run.eta
        } else if let Some(sweep) = self.freq_sweep.as_ref() {
            sweep.eta
        } else {
            self.sweep.as_ref()?.eta
        };
        Some((eta, planned_remaining_s))
    }

    /// One line of "how much longer", for whichever run the operator started.
    ///
    /// The total is elapsed + remaining rather than a stored plan number, so it
    /// is wall clock throughout and grows honestly when a run runs long.
    fn estimated_time_line(&self, now_ms: u64) -> Option<String> {
        let (eta, planned_remaining_s) = self.run_eta()?;
        let remaining_s = eta.remaining_s(now_ms, planned_remaining_s);
        let total_s = eta.elapsed_s(now_ms) + remaining_s;
        let mut line = format!(
            "Estimated time: ≈ {} left of ≈ {}",
            format_duration(remaining_s),
            format_duration(total_s)
        );
        // Long enough that the operator will leave the bench: say when to come
        // back, in the same UTC every file of the run is stamped with. Minutes
        // are noise below that — a five-minute sweep is watched, not planned
        // around.
        if remaining_s >= 600.0 {
            line.push_str(&format!(
                " — done by {}",
                format_clock_utc(now_ms / 1_000 + remaining_s as u64)
            ));
        }
        // Nothing has finished yet, so this is the plan's own time with a fixed
        // allowance for the bench. Say so, rather than let a number that is
        // about to grow look like a measurement.
        if eta.pace().is_none() {
            line.push_str(" (from the plan until the first point finishes)");
        }
        Some(line)
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
    fn begin_sweep(&mut self, context: &mut impl RecordingControl) {
        if self.recording.is_active() || self.sweep.is_some() {
            self.message = "A recording or sweep is already running".into();
            return;
        }
        if self.output_folder.trim().is_empty() {
            self.message = "Pick an output folder first — that is where the files go".into();
            return;
        }
        if !self.modulation_connected() {
            self.message = "The modulation plugin is not connected — connect it to drive the \
                            depth"
                .into();
            return;
        }
        if self
            .modulation
            .as_ref()
            .and_then(|state| state.calibration_id.as_deref())
            .is_none()
        {
            self.message = "Run the Pockels calibration in the modulation plugin first — without \
                            it a commanded depth means nothing"
                .into();
            return;
        }
        // Same wording every other gate uses, from the same helper: the owner
        // knows which estimator gate withheld `a`, and a fixed sentence here
        // used to send the operator after the wrong thing.
        if let Some(reason) = self.depth_a_blocker() {
            self.message = format!(
                "The sweep needs a {} depth a, but {reason}",
                self.depth_source.verb()
            );
            return;
        }
        // The sweep ends in a recording, so ask the recording's own question now
        // rather than after the drive has already moved to point 1.
        if let Some(blocker) = self.photodiode_blocker() {
            self.message = blocker;
            return;
        }
        if self.min_a.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            self.message = "Set Sweep min a above 0 — a = 0 is the background reference, which \
                            has its own button"
                .into();
            return;
        }
        if self.max_a.partial_cmp(&self.min_a) != Some(std::cmp::Ordering::Greater) {
            self.message = "Sweep max a must be larger than Sweep min a".into();
            return;
        }
        let points = self.sweep_points();
        let message = format!(
            "Sweep: acquiring modulation lease for {} points, ≈ {} of bench time…",
            points.len(),
            format_duration(points.len() as f64 * self.planned_point_s())
        );
        self.begin_leased_sweep(context, SweepKind::Amplitude, points, None, None, message);
    }

    /// Shared entry point for both leased recording runs (amplitude sweep and
    /// single event-count point): validate the destination and the owner, then
    /// acquire the modulation lease that holds the drive for the whole run.
    fn begin_leased_sweep(
        &mut self,
        context: &mut impl RecordingControl,
        kind: SweepKind,
        points: Vec<SweepPoint>,
        lock: Option<A0LockPoint>,
        inherited_lease: Option<LeaseId>,
        message: String,
    ) {
        if self.recording.is_active() || self.sweep.is_some() || self.a0_lock.is_some() {
            self.message = "A recording, sweep or a₀ lock is already running".into();
            return;
        }
        if self.output_folder.trim().is_empty() {
            self.message = "Pick an output folder first — that is where the files go".into();
            return;
        }
        if !self.modulation_connected() {
            self.message =
                "The modulation plugin is not connected — connect it to drive the depth".into();
            return;
        }
        if points.is_empty() {
            self.message = "Nothing to record: the run has no points".into();
            return;
        }
        let now_ms = now_unix_ms();
        // A verdict belongs to the run that produced it. Clearing it here means
        // an enclosing ladder can never read the previous rung's outcome if
        // this one ends without reaching `finish_sweep`.
        self.last_sweep_completed_ok = false;
        let owns_lease = inherited_lease.is_none();
        let lease_id = inherited_lease.unwrap_or_else(|| {
            LeaseId::new(format!("a1-sweep-{}", format_compact_utc(now_ms / 1_000)))
        });
        let mut lease_req = 0;
        if owns_lease {
            let ttl_ms = self.sweep_lease_ttl_ms(points.len());
            let request =
                self.modulation_request(ModulationCommandV1::AcquireLease { ttl_ms }, &lease_id);
            lease_req = request.request_id;
            context.request_service(&request);
        }
        self.sweep = Some(Sweep {
            phase: SweepPhase::AcquiringLease,
            kind,
            points,
            lock,
            index: 0,
            lease_id,
            // An inherited lease is already granted; the first tick goes
            // straight to retargeting the depth.
            lease_granted: !owns_lease,
            lease_req,
            owns_lease,
            depth_req: 0,
            depth_applied: false,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: false,
            completed_ok: false,
            last_activity_ms: now_ms,
            stop_requested: false,
            eta: Eta::new(now_ms),
        });
        self.message = message;
    }

    /// Record one atomic frequency point of the exact-event-count workflow.
    ///
    /// The armed lock's commanded depth is re-applied under a modulation lease —
    /// which also locks the operator's drive settings out for the whole point, so
    /// the amplitude provably cannot change during the recorded interval — and the
    /// point is then recorded through the same coordinator as every other run.
    fn begin_a0_point(
        &mut self,
        context: &mut impl RecordingControl,
        inherited_lease: Option<LeaseId>,
    ) {
        let Some(lock) = self.armed_a0() else {
            self.message = format!(
                "Cannot record the a₀ point: {}",
                self.armed_a0_blocker()
                    .unwrap_or_else(|| "no depth is armed".into())
            );
            return;
        };
        let points = vec![SweepPoint {
            commanded_a: lock.commanded_a,
            expected_a: lock.target_a,
        }];
        let message = format!(
            "Event-count point at {}: leasing the drive at commanded a = {:.3} (a₀ = {:.3})…",
            frequency_label(lock.frequency_hz),
            lock.commanded_a,
            lock.target_a
        );
        self.begin_leased_sweep(
            context,
            SweepKind::EventCount,
            points,
            Some(lock),
            inherited_lease,
            message,
        );
    }

    /// Release the modulation lease (if held) and clear the sweep.
    fn finish_sweep(&mut self, context: &mut impl RecordingControl, message: String) {
        if let Some(sweep) = self.sweep.take() {
            self.last_sweep_completed_ok = sweep.completed_ok;
            if sweep.owns_lease && sweep.lease_granted {
                let request = self.modulation_request(
                    ModulationCommandV1::ReleaseLease {
                        safe_off: false,
                        reason: "a1 sweep finished".into(),
                    },
                    &sweep.lease_id,
                );
                context.request_service(&request);
            }
        }
        self.message = message;
    }

    /// Renew the modulation lease and retarget the drive at the current point.
    fn send_sweep_depth(&mut self, context: &mut impl RecordingControl) {
        let Some(sweep) = self.sweep.as_ref() else {
            return;
        };
        let lease_id = sweep.lease_id.clone();
        let remaining = sweep.total().saturating_sub(sweep.index);
        let commanded_a = sweep.commanded_a();
        let target_a = sweep.target_a();
        let index = sweep.index;
        let total = sweep.total();

        let ttl_ms = self.sweep_lease_ttl_ms(remaining);
        let renew = self.modulation_request(ModulationCommandV1::RenewLease { ttl_ms }, &lease_id);
        context.request_service(&renew);

        let depth = self.modulation_request(
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: depth_a_milli(commanded_a),
            },
            &lease_id,
        );
        let depth_req = depth.request_id;
        context.request_service(&depth);

        let now_ms = now_unix_ms();
        if let Some(sweep) = self.sweep.as_mut() {
            sweep.phase = SweepPhase::SettingDepth;
            sweep.depth_req = depth_req;
            sweep.depth_applied = false;
            sweep.settled_since_ms = None;
            sweep.point_started = false;
            sweep.last_activity_ms = now_ms;
        }
        self.message = if commanded_a == target_a {
            format!(
                "Sweep point {}/{total}: retargeting drive to a = {target_a:.3}…",
                index + 1
            )
        } else {
            format!(
                "Event-count point: commanding a = {commanded_a:.3} for a {} a₀ = {target_a:.3}…",
                self.depth_source.verb()
            )
        };
    }

    /// Advance the amplitude sweep one control tick. Runs before
    /// `drive_recording`, so a point's recording starts on the same tick.
    fn drive_sweep(&mut self, context: &mut impl RecordingControl) {
        if self.sweep.is_none() {
            if std::mem::take(&mut self.sweep_pending) {
                self.begin_sweep(context);
            } else if std::mem::take(&mut self.a0_point_pending) {
                self.begin_a0_point(context, None);
            }
            return;
        }
        self.sweep_pending = false;
        self.a0_point_pending = false;
        let now_ms = now_unix_ms();
        let (
            phase,
            kind,
            stop_requested,
            lease_granted,
            depth_applied,
            last_activity_ms,
            index,
            total,
        ) = {
            let sweep = self.sweep.as_ref().expect("sweep checked above");
            (
                sweep.phase,
                sweep.kind,
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
                // The amplitude sweep drives open-loop and accepts the coarse
                // calibration band; an event-count point replays a depth that was
                // already trimmed against `a₀`, so it holds the lock's band.
                let tolerance = match kind {
                    SweepKind::Amplitude => sweep_tolerance(target),
                    SweepKind::EventCount => self.a0_tolerance.max(1e-3),
                };
                // With `DepthSource::Commanded` this compares the commanded
                // depth against itself and settles as soon as the owner has
                // applied it — which is the honest answer for an open-loop
                // sweep: nothing on the bench can contradict the command. The
                // operator's settle dwell below still applies, so the drive
                // gets its physical time to move either way.
                let settled = self
                    .depth_a()
                    .is_some_and(|measured| (measured - target).abs() <= tolerance);
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
                        settle_timed_out = true;
                    }
                    if start_recording {
                        sweep.phase = SweepPhase::Recording;
                    }
                }
                if settle_timed_out {
                    self.finish_sweep(
                        context,
                        format!(
                            "Sweep aborted at point {}/{}: no fresh, settled optical a at \
                             target {target:.3} before timeout",
                            index + 1,
                            total
                        ),
                    );
                    return;
                }
                if start_recording {
                    self.pending_role = Some(kind.role());
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
                    let message = match kind {
                        SweepKind::Amplitude => format!("Sweep complete: {total} points recorded"),
                        SweepKind::EventCount => self.message.clone(),
                    };
                    if let Some(sweep) = self.sweep.as_mut() {
                        sweep.completed_ok = true;
                    }
                    self.finish_sweep(context, message);
                } else {
                    let planned_s = self.planned_point_s();
                    if let Some(sweep) = self.sweep.as_mut() {
                        sweep.eta.point_done(now_ms, planned_s);
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

    // ---- exact event-count depth a₀ (ADR 013) ------------------------------

    /// The lock stored for `hz`, whether or not it converged.
    fn lock_for_frequency(&self, hz: f64) -> Option<&A0LockPoint> {
        self.a0_locks
            .iter()
            .find(|lock| same_frequency(lock.frequency_hz, hz))
    }

    /// The lock that applies to the drive right now: same frequency, converged,
    /// and aimed at the `a₀` currently entered.
    ///
    /// "Aimed at the same `a₀`" is judged against the operator's own convergence
    /// tolerance, not on exact equality. The a₀ field is a drag control with a
    /// 0.01 step, so a strict comparison disarmed a lock the operator had just
    /// found the moment they nudged the slider — and then asked them to press
    /// Find a₀ again, which is what they had done.
    fn armed_lock(&self) -> Option<&A0LockPoint> {
        let hz = self.frequency_hz()?;
        let tolerance = self.a0_tolerance.max(1e-3);
        self.lock_for_frequency(hz)
            .filter(|lock| lock.converged && (lock.target_a - self.a0_target).abs() <= tolerance)
    }

    /// The depth an `a₀` recording would be made at right now — the one
    /// question every consumer of the lock table actually asks.
    ///
    /// With a measured depth source this is a stored, converged lock: the
    /// commanded depth that was *found* to produce `a₀` at this frequency, and
    /// there is no answer until [`Self::begin_a0_lock`] has found one.
    ///
    /// With a commanded depth source there is nothing to look up. `a₀` is
    /// commanded directly, at every frequency, so the answer is always
    /// available and is synthesised here rather than round-tripped through a
    /// table of identical rows ([`DepthSource::needs_a0_lock`]). `trials: 0`
    /// records honestly that no search happened.
    fn armed_a0(&self) -> Option<A0LockPoint> {
        if self.depth_source.needs_a0_lock() {
            return self.armed_lock().cloned();
        }
        let hz = self.frequency_hz()?;
        let target = self.a0_target;
        (COMMANDED_A_MIN..=COMMANDED_A_MAX)
            .contains(&target)
            .then(|| A0LockPoint {
                frequency_hz: hz,
                target_a: target,
                commanded_a: clamp_commanded_a(target),
                measured_a: target,
                trials: 0,
                converged: true,
                locked_at_unix_ms: now_unix_ms(),
                low_clip_fraction: None,
                high_clip_fraction: None,
                depth_source: self.depth_source,
            })
    }

    /// Why no `a₀` recording can be made right now, phrased as the operator
    /// action that fixes it. `None` means [`Self::armed_a0`] has an answer.
    fn armed_a0_blocker(&self) -> Option<String> {
        if self.depth_source.needs_a0_lock() {
            return self.armed_lock_blocker();
        }
        if self.armed_a0().is_some() {
            return None;
        }
        if self.frequency_hz().is_none() {
            return Some(format!(
                "there is no modulation frequency yet: {}",
                self.frequency_blocker()
                    .unwrap_or_else(|| "no drive is armed".into())
            ));
        }
        Some(format!(
            "a₀ = {:.3} is outside the drivable {COMMANDED_A_MIN}..={COMMANDED_A_MAX}",
            self.a0_target
        ))
    }

    /// Why the stored locks do not arm a recording at the current frequency,
    /// phrased as the operator action that fixes it.
    ///
    /// The three causes — no lock at this frequency, a lock that did not
    /// converge, a lock aimed at a different a₀ — used to share one sentence
    /// telling the operator to press Find a₀, which only helps for the first.
    fn armed_lock_blocker(&self) -> Option<String> {
        if self.armed_lock().is_some() {
            return None;
        }
        let Some(hz) = self.frequency_hz() else {
            return Some(format!(
                "there is no modulation frequency yet: {}",
                self.frequency_blocker()
                    .unwrap_or_else(|| "no drive is armed".into())
            ));
        };
        let label = frequency_label(hz);
        let Some(lock) = self.lock_for_frequency(hz) else {
            return Some(format!(
                "no depth has been found for {label} yet — press Find a₀ at this frequency"
            ));
        };
        if !lock.converged {
            return Some(format!(
                "the last Find a₀ at {label} did not reach a₀ (it stopped at a measured {:.3}) — \
                 press Find a₀ again, or widen the a₀ tolerance",
                lock.measured_a
            ));
        }
        Some(format!(
            "the depth found for {label} was aimed at a₀ = {:.3}, and a₀ is now {:.3} — press \
             Find a₀ again at the new a₀",
            lock.target_a, self.a0_target
        ))
    }

    fn a0_locks_path(&self) -> Option<PathBuf> {
        let folder = self.output_folder.trim();
        (!folder.is_empty()).then(|| Path::new(folder).join(A0_LOCK_FILE))
    }

    /// Store a finished lock, replacing any earlier one at the same frequency,
    /// and mirror the table to disk. Returns a save failure for the caller to
    /// append to its own message.
    fn store_lock(&mut self, lock: A0LockPoint) -> Result<(), String> {
        self.a0_locks
            .retain(|existing| !same_frequency(existing.frequency_hz, lock.frequency_hz));
        self.a0_locks.push(lock);
        self.a0_locks
            .sort_by(|left, right| left.frequency_hz.total_cmp(&right.frequency_hz));
        self.save_a0_locks()
    }

    /// Persist the lock table next to the recordings, so the found depths survive
    /// a restart and can be cited offline.
    ///
    /// Returns the failure so the caller can append it to its own message: a
    /// lock the operator can see on screen but that never reached disk is a
    /// lock they will not have after a restart.
    fn save_a0_locks(&mut self) -> Result<(), String> {
        let Some(path) = self.a0_locks_path() else {
            return Ok(());
        };
        let table = A0LockTable {
            locks: self.a0_locks.clone(),
        };
        let written = serde_json::to_string_pretty(&table)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&path, text).map_err(|error| error.to_string())
            });
        written.map_err(|error| format!("a₀ lock table save failed: {error}"))
    }

    /// Re-read the lock table when the experiment folder changes.
    fn load_a0_locks(&mut self) {
        let folder = self.output_folder.trim().to_string();
        if self.loaded_locks_folder.as_deref() == Some(folder.as_str()) {
            return;
        }
        self.loaded_locks_folder = Some(folder);
        self.a0_locks.clear();
        let Some(path) = self.a0_locks_path() else {
            return;
        };
        if let Some(table) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<A0LockTable>(&text).ok())
        {
            self.a0_locks = table.locks;
        }
    }

    /// Worst-case lock duration, used as the modulation lease TTL.
    fn a0_lock_lease_ttl_ms(&self) -> u64 {
        let per_trial_ms = (self.settle_s.max(0.0) * 1_000.0) as u64 + SWEEP_SETTLE_TIMEOUT_MS;
        u64::from(A0_LOCK_MAX_TRIALS)
            .saturating_mul(per_trial_ms)
            .saturating_add(60_000)
    }

    /// Kick off the closed-loop `a₀` lock at the current frequency.
    fn begin_a0_lock(
        &mut self,
        context: &mut impl RecordingControl,
        inherited_lease: Option<LeaseId>,
    ) {
        if self.recording.is_active() || self.sweep.is_some() || self.a0_lock.is_some() {
            self.message = "A recording, sweep or a₀ lock is already running".into();
            return;
        }
        // There is nothing to search for when `a` *is* the command: the search
        // would command a₀, read back a₀ and stop. Say so instead of spending
        // a lease and a trial to arrive back where the operator already is.
        if !self.depth_source.needs_a0_lock() {
            self.message = format!(
                "No search needed: with the depth coming from the commanded drive, a₀ = {:.3} is \
                 simply commanded at every frequency. Press \"Record a₀ point\", or \"Record all \
                 frequencies\" for the whole ladder.",
                self.a0_target
            );
            return;
        }
        if !self.modulation_connected() {
            self.message = "Modulation owner is not connected — cannot find a₀".into();
            return;
        }
        // The lock table belongs to the experiment folder, and it is re-read
        // whenever that folder changes: without one, a lock found now would be
        // dropped the moment the operator picks the destination.
        if self.output_folder.trim().is_empty() {
            self.message = "Set an output folder before finding a₀".into();
            return;
        }
        let Some(hz) = self.frequency_hz() else {
            self.message = format!(
                "Cannot find a₀ without a modulation frequency: {}",
                self.frequency_blocker()
                    .unwrap_or_else(|| "no drive is armed".into())
            );
            return;
        };
        if let Some(reason) = self.depth_a_blocker() {
            self.message = format!(
                "Cannot find a₀ without a {} a: {reason}",
                self.depth_source.verb()
            );
            return;
        }
        // Refuse before touching the drive, not after eight trials of chasing a
        // truncated estimate upwards.
        if let Err(reason) = self.optical_window_covers_a_cycle(hz) {
            self.message = format!("Cannot find a₀ at {}: {reason}", frequency_label(hz));
            return;
        }
        let target = self.a0_target;
        if !(COMMANDED_A_MIN..=COMMANDED_A_MAX).contains(&target) {
            self.message = format!(
                "a₀ = {target:.3} is outside the drivable {COMMANDED_A_MIN}..={COMMANDED_A_MAX}"
            );
            return;
        }
        // Warm start from an earlier lock at this frequency; otherwise trust the
        // Pockels calibration for the first guess (command exactly `a₀`).
        let start = self
            .lock_for_frequency(hz)
            .map(|lock| lock.commanded_a)
            .unwrap_or(target);
        let now_ms = now_unix_ms();
        let owns_lease = inherited_lease.is_none();
        let lease_id = inherited_lease.unwrap_or_else(|| {
            LeaseId::new(format!("a1-a0-{}", format_compact_utc(now_ms / 1_000)))
        });
        let mut lease_req = 0;
        if owns_lease {
            let ttl_ms = self.a0_lock_lease_ttl_ms();
            let request =
                self.modulation_request(ModulationCommandV1::AcquireLease { ttl_ms }, &lease_id);
            lease_req = request.request_id;
            context.request_service(&request);
        }
        self.a0_lock = Some(A0Lock {
            phase: A0LockPhase::AcquiringLease,
            target_a: target,
            tolerance: self.a0_tolerance.max(1e-3),
            commanded_a: clamp_commanded_a(start),
            frequency_hz: hz,
            trial: 1,
            samples: Vec::new(),
            sampled_revision: None,
            measure_from_ms: 0,
            window_ms: 0,
            deadline_ms: 0,
            lease_id,
            lease_granted: !owns_lease,
            lease_req,
            owns_lease,
            depth_req: 0,
            depth_applied: false,
            last_activity_ms: now_ms,
            stop_requested: false,
        });
        self.message = if owns_lease {
            format!(
                "a₀ lock at {}: acquiring the modulation lease…",
                frequency_label(hz)
            )
        } else {
            format!(
                "a₀ lock at {}: trimming the drive depth…",
                frequency_label(hz)
            )
        };
    }

    /// Renew the lease and command the current trial's depth.
    fn send_a0_depth(&mut self, context: &mut impl RecordingControl) {
        let Some(lock) = self.a0_lock.as_ref() else {
            return;
        };
        let lease_id = lock.lease_id.clone();
        let commanded = lock.commanded_a;
        let trial = lock.trial;
        let target = lock.target_a;

        let ttl_ms = self.a0_lock_lease_ttl_ms();
        let renew = self.modulation_request(ModulationCommandV1::RenewLease { ttl_ms }, &lease_id);
        context.request_service(&renew);
        let depth = self.modulation_request(
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: depth_a_milli(commanded),
            },
            &lease_id,
        );
        let depth_req = depth.request_id;
        context.request_service(&depth);

        let now_ms = now_unix_ms();
        if let Some(lock) = self.a0_lock.as_mut() {
            lock.phase = A0LockPhase::SettingDepth;
            lock.depth_req = depth_req;
            lock.depth_applied = false;
            lock.samples.clear();
            lock.sampled_revision = None;
            lock.last_activity_ms = now_ms;
        }
        self.message = format!(
            "a₀ lock trial {trial}/{A0_LOCK_MAX_TRIALS}: commanding a = {commanded:.3} for a {} \
             a₀ = {target:.3}…",
            self.depth_source.verb()
        );
    }

    /// Release the modulation lease and clear the lock.
    ///
    /// Never `safe_off`: the drive must stay exactly where the lock left it, so
    /// the event-count point that follows records at `a₀`.
    fn finish_a0_lock(&mut self, context: &mut impl RecordingControl, message: String) {
        if let Some(lock) = self.a0_lock.take() {
            if lock.owns_lease && lock.lease_granted {
                let request = self.modulation_request(
                    ModulationCommandV1::ReleaseLease {
                        safe_off: false,
                        reason: "a1 a0 lock finished".into(),
                    },
                    &lock.lease_id,
                );
                context.request_service(&request);
            }
        }
        self.message = message;
    }

    /// Length of the photodiode's contrast estimator window, in milliseconds.
    ///
    /// This is the time a commanded depth needs to fully replace the previous
    /// one inside the estimate. Owners that predate the field do not publish
    /// it; then only the operator's settle dwell is available.
    fn optical_window_seconds(&self) -> Option<f64> {
        // A window only bounds a depth that is read out of it. A commanded
        // depth is not, so there is nothing here to wait for or to check.
        if self.depth_source != DepthSource::Photodiode {
            return None;
        }
        self.photodiode
            .as_ref()?
            .optical_summary
            .as_ref()?
            .window_seconds
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
    }

    /// [`Self::optical_window_seconds`] rounded up to the millisecond the lock's
    /// timers work in.
    fn optical_window_ms(&self) -> Option<u64> {
        self.optical_window_seconds()
            .map(|seconds| (seconds * 1_000.0).ceil() as u64)
    }

    /// Whether the photodiode's estimator window spans at least one full
    /// modulation cycle at `hz`, i.e. whether the published `a` can be a
    /// peak-to-peak measurement at all.
    ///
    /// The owner refuses on its own when its markers can prove the window is
    /// too short. It cannot when it has no marker stream — but A1 always knows
    /// the frequency, from its own phase-0 triggers or the armed drive, so the
    /// check is repeated here where the knowledge is. Getting this wrong is not
    /// a small error: a sub-cycle window *under*-reports `a`, and the lock
    /// divides by it, so it would drive the depth up until it rails.
    fn optical_window_covers_a_cycle(&self, hz: f64) -> Result<(), String> {
        let Some(window_seconds) = self.optical_window_seconds() else {
            return Ok(());
        };
        let cycles = window_seconds * hz;
        if cycles >= 1.0 {
            return Ok(());
        }
        Err(format!(
            "the photodiode estimates a over {window_seconds:.4} s, only {cycles:.2} cycles at \
             {} — a is a peak-to-peak quantity and would be under-reported. Raise the photodiode \
             cache length to at least {:.0} s",
            frequency_label(hz),
            (2.0 / hz).ceil().max(1.0),
        ))
    }

    /// Take one reading per *independent* photodiode window.
    ///
    /// Two constraints, both about the estimator window rather than the
    /// publisher: a reading must come from a summary that did not exist when
    /// the depth was commanded (`sampled_revision`), and consecutive readings
    /// must be at least [`A0_LOCK_SAMPLE_SPACING`] of a window apart —
    /// otherwise they share nearly all their samples and three of them say no
    /// more than one.
    fn sample_a0_measurement(&mut self, now_ms: u64) {
        let Some((revision, measured)) = self.depth_reading() else {
            return;
        };
        let spacing_ms = self.a0_sample_spacing_ms();
        // The stale-window rule exists because the photodiode's estimate mixes
        // samples from before and after the depth changed. A commanded depth is
        // not read out of a window at all — it is the value that was just
        // applied — so holding it to the same rule would only make the trial
        // depend on the modulation owner's device-poll cadence, and time out
        // whenever that owner had nothing new to say.
        let requires_new_revision = self.depth_source == DepthSource::Photodiode;
        let Some(lock) = self.a0_lock.as_mut() else {
            return;
        };
        if now_ms < lock.measure_from_ms
            || (requires_new_revision && lock.sampled_revision == Some(revision))
        {
            return;
        }
        lock.sampled_revision = Some(revision);
        lock.samples.push(measured);
        lock.measure_from_ms = now_ms.saturating_add(spacing_ms);
    }

    /// One depth reading from the active [`DepthSource`], tagged with the
    /// publishing owner's service revision.
    ///
    /// The revision is what makes a reading *independent*: the lock only counts
    /// values published after it commanded the depth, so a trial never averages
    /// in the previous one. Both owners bump their revision on every state
    /// change, so the same rule works for either source.
    fn depth_reading(&self) -> Option<(u64, f64)> {
        match self.depth_source {
            DepthSource::Photodiode => self.photodiode.as_ref().and_then(|summary| {
                summary
                    .optical_summary
                    .as_ref()
                    .map(|optical| (summary.service_revision, optical.measured_log_contrast))
            }),
            DepthSource::Commanded => self.commanded_a().map(|a| {
                (
                    self.modulation.as_ref().map_or(0, |s| s.service_revision),
                    a,
                )
            }),
        }
    }

    /// Minimum gap between two readings of one trial.
    fn a0_sample_spacing_ms(&self) -> u64 {
        let window_ms = self
            .a0_lock
            .as_ref()
            .map(|lock| lock.window_ms)
            .unwrap_or_default();
        ((window_ms as f64) * A0_LOCK_SAMPLE_SPACING).ceil() as u64
    }

    /// Photodiode clipping note for a lock message, empty when the windows are clean.
    fn clip_warning(&self) -> String {
        // A clipped detector window says nothing about a commanded depth, and
        // appending it to that lock's message would suggest it did.
        if self.depth_source != DepthSource::Photodiode {
            return String::new();
        }
        let Some(optical) = self
            .photodiode
            .as_ref()
            .and_then(|summary| summary.optical_summary.as_ref())
        else {
            return String::new();
        };
        if optical.low_clip_fraction.max(optical.high_clip_fraction) <= A0_LOCK_CLIP_WARNING {
            return String::new();
        }
        format!(
            " — warning: photodiode clipping (low {:.1} %, high {:.1} %), the measured a is a \
             truncated estimate",
            optical.low_clip_fraction * 100.0,
            optical.high_clip_fraction * 100.0
        )
    }

    /// Close out one trial: converged, out of trials, at a drive limit, or one
    /// more multiplicative correction.
    fn evaluate_a0_trial(&mut self, context: &mut impl RecordingControl) {
        let Some(lock) = self.a0_lock.as_ref() else {
            return;
        };
        let (target, tolerance, commanded, trial, hz) = (
            lock.target_a,
            lock.tolerance,
            lock.commanded_a,
            lock.trial,
            lock.frequency_hz,
        );
        let mut readings = lock.samples.clone();
        if readings.is_empty() {
            // The owner withholds `a` for a stated reason (clipping, no
            // headroom, a bad `I_tot` anchor, a sub-cycle window). Ask the
            // blocker for it rather than leaving the operator with "nothing
            // happened" — and it answers for whichever source is selected.
            let reason = self
                .depth_a_blocker()
                .unwrap_or_else(|| "it published nothing while the lock was measuring".into());
            self.finish_a0_lock(
                context,
                format!("a₀ lock aborted: no depth a arrived while measuring — {reason}"),
            );
            return;
        }
        readings.sort_by(f64::total_cmp);
        let measured = readings[readings.len() / 2];
        let spread = readings[readings.len() - 1] - readings[0];
        if measured <= 0.0 {
            self.finish_a0_lock(
                context,
                format!(
                    "a₀ lock aborted: the {} a = {measured:.3} — check the I_tot anchor and that \
                     the drive is modulating",
                    self.depth_source.verb()
                ),
            );
            return;
        }
        // A drifting `a` that happens to cross the target on one reading is not
        // a lock: the next action would record at whatever it drifted to.
        if readings.len() > 1 && spread > tolerance * A0_LOCK_MAX_SPREAD_TOLERANCES {
            self.finish_a0_lock(
                context,
                format!(
                    "a₀ lock aborted at {}: the observed a is not settled — {} readings spread \
                     {spread:.3} across {}× the ±{tolerance:.3} tolerance (median {measured:.3}). \
                     Increase Sweep settle (s) or check the drive and the I_tot anchor",
                    frequency_label(hz),
                    readings.len(),
                    A0_LOCK_MAX_SPREAD_TOLERANCES,
                ),
            );
            return;
        }

        let converged = (measured - target).abs() <= tolerance;
        // The delivered optical depth is proportional to the commanded one to
        // first order, so one gain correction per trial converges in a couple of
        // steps even where the drive rolls off at high frequency.
        let ratio = (target / measured).clamp(1.0 / A0_LOCK_MAX_STEP_RATIO, A0_LOCK_MAX_STEP_RATIO);
        let next = clamp_commanded_a(commanded * ratio);
        let railed = !converged && (next - commanded).abs() < 1e-9;
        let exhausted = trial >= A0_LOCK_MAX_TRIALS;

        if !converged && !railed && !exhausted {
            if let Some(lock) = self.a0_lock.as_mut() {
                lock.commanded_a = next;
                lock.trial += 1;
            }
            self.message = format!(
                "a₀ lock trial {trial}: {} a = {measured:.3} vs a₀ = {target:.3} — correcting the \
                 commanded depth to {next:.3}",
                self.depth_source.verb()
            );
            self.send_a0_depth(context);
            return;
        }

        let optical = self
            .photodiode
            .as_ref()
            .and_then(|summary| summary.optical_summary.as_ref());
        let saved = self.store_lock(A0LockPoint {
            frequency_hz: hz,
            target_a: target,
            commanded_a: commanded,
            measured_a: measured,
            trials: trial,
            converged,
            locked_at_unix_ms: now_unix_ms(),
            low_clip_fraction: optical.map(|optical| optical.low_clip_fraction),
            high_clip_fraction: optical.map(|optical| optical.high_clip_fraction),
            depth_source: self.depth_source,
        });
        let label = frequency_label(hz);
        // "measures" is a claim about the light. Open loop the lock has only
        // confirmed that the drive accepted the depth, so say that instead.
        let verb = match self.depth_source {
            DepthSource::Photodiode => "measures",
            DepthSource::Commanded => "is commanded as",
        };
        let message = if converged {
            format!(
                "a₀ locked at {label}: commanded a = {commanded:.3} {verb} a = {measured:.3} \
                 (a₀ = {target:.3}, {trial} trial(s)){}",
                self.clip_warning()
            )
        } else if railed {
            format!(
                "a₀ lock stopped at {label}: commanded a = {commanded:.3} is at the drivable limit \
                 and only {verb} a = {measured:.3} — lower a₀ or the operating point I_k"
            )
        } else {
            format!(
                "a₀ lock did not converge at {label}: best commanded a = {commanded:.3} {verb} \
                 a = {measured:.3} after {trial} trials — widen the tolerance or check the drive"
            )
        };
        // A lock the operator can see but that never reached disk is a lock
        // they will not have after a restart — say so on the same line.
        let message = match saved {
            Ok(()) => message,
            Err(error) => format!("{message} — {error}"),
        };
        self.finish_a0_lock(context, message);
    }

    /// Advance the `a₀` lock one control tick.
    fn drive_a0_lock(&mut self, context: &mut impl RecordingControl) {
        if self.a0_lock.is_none() {
            if std::mem::take(&mut self.a0_lock_pending) {
                self.begin_a0_lock(context, None);
            }
            return;
        }
        self.a0_lock_pending = false;
        let now_ms = now_unix_ms();
        let (phase, stop_requested, lease_granted, depth_applied, last_activity_ms) = {
            let lock = self.a0_lock.as_ref().expect("lock checked above");
            (
                lock.phase,
                lock.stop_requested,
                lock.lease_granted,
                lock.depth_applied,
                lock.last_activity_ms,
            )
        };
        if stop_requested {
            let message = if self.message.is_empty() {
                "a₀ lock stopped".into()
            } else {
                self.message.clone()
            };
            self.finish_a0_lock(context, message);
            return;
        }
        match phase {
            A0LockPhase::AcquiringLease => {
                if lease_granted {
                    self.send_a0_depth(context);
                } else if now_ms.saturating_sub(last_activity_ms) > REPLY_TIMEOUT_MS {
                    self.finish_a0_lock(
                        context,
                        "a₀ lock aborted: timed out acquiring the modulation lease".into(),
                    );
                }
            }
            A0LockPhase::SettingDepth => {
                if depth_applied {
                    // The drive settles for the operator's dwell, and the
                    // photodiode's own estimator window has to roll over before
                    // the published `a` is free of the previous depth. Waiting
                    // for only the shorter of the two silently measures a
                    // mixture — with the 0.82 s default window that is every
                    // settle below ~1 s, and it gets worse at low frequency
                    // where the window grows to cover whole cycles.
                    let window_ms = self.optical_window_ms().unwrap_or_default();
                    let dwell_ms = ((self.settle_s.max(0.0) * 1_000.0) as u64).max(window_ms);
                    // Only summaries published *after* this depth was commanded
                    // count, so the trial never averages the previous depth.
                    let published = self.depth_reading().map(|(revision, _)| revision);
                    if let Some(lock) = self.a0_lock.as_mut() {
                        lock.phase = A0LockPhase::Measuring;
                        lock.window_ms = window_ms;
                        lock.measure_from_ms = now_ms.saturating_add(dwell_ms);
                        // The deadline has to outlast the readings it is
                        // waiting for, or a low-frequency point times out
                        // before its first independent sample can exist.
                        let sampling_ms =
                            (window_ms as f64 * A0_LOCK_SAMPLE_SPACING * A0_LOCK_SAMPLES as f64)
                                .ceil() as u64;
                        lock.deadline_ms = lock
                            .measure_from_ms
                            .saturating_add(SWEEP_SETTLE_TIMEOUT_MS.max(sampling_ms * 2));
                        lock.samples.clear();
                        lock.sampled_revision = published;
                    }
                } else if now_ms.saturating_sub(last_activity_ms) > REPLY_TIMEOUT_MS {
                    self.finish_a0_lock(
                        context,
                        "a₀ lock aborted: timed out retargeting the modulation drive".into(),
                    );
                }
            }
            A0LockPhase::Measuring => {
                self.sample_a0_measurement(now_ms);
                let ready = self.a0_lock.as_ref().is_some_and(|lock| {
                    lock.samples.len() >= A0_LOCK_SAMPLES || now_ms >= lock.deadline_ms
                });
                if ready {
                    self.evaluate_a0_trial(context);
                }
            }
        }
    }

    /// Routes modulation-service replies belonging to the `a₀` lock. Returns true
    /// when the reply was consumed.
    fn on_a0_lock_reply(&mut self, reply: &PluginServiceReply) -> bool {
        let Some((lease_req, depth_req)) = self
            .a0_lock
            .as_ref()
            .map(|lock| (lock.lease_req, lock.depth_req))
        else {
            return false;
        };
        let abort = |this: &mut Self, message: String| {
            this.message = message;
            if let Some(lock) = this.a0_lock.as_mut() {
                lock.stop_requested = true;
            }
        };
        if reply.request_id == lease_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(lock) = self.a0_lock.as_mut() {
                        lock.lease_granted = true;
                        lock.last_activity_ms = now_unix_ms();
                    }
                }
                PluginServiceOutcome::Rejected { message, .. } => abort(
                    self,
                    format!("a₀ lock aborted: modulation lease rejected: {message}"),
                ),
            }
            true
        } else if reply.request_id == depth_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(lock) = self.a0_lock.as_mut() {
                        lock.depth_applied = true;
                        lock.last_activity_ms = now_unix_ms();
                    }
                }
                // The owner refuses a depth its calibrated drive cannot express
                // (lobe ceiling, DAC limit) — that *is* the "a₀ unreachable at
                // this operating point" answer, so surface its wording verbatim.
                PluginServiceOutcome::Rejected { message, .. } => abort(
                    self,
                    format!("a₀ lock aborted: the drive rejected the commanded depth: {message}"),
                ),
            }
            true
        } else {
            false
        }
    }

    // ---- multi-frequency a₀ ladder -----------------------------------------

    /// The planned frequency ladder, log-spaced and inclusive of both ends.
    ///
    /// Log spacing because `|H(f)|` is read per decade: a linear ladder spends
    /// most of its points where the response is flat and none where it rolls
    /// off.
    fn planned_frequencies(&self) -> Vec<f64> {
        let count = self.freq_count.clamp(1, FREQ_SWEEP_MAX_POINTS as u32) as usize;
        if count == 1 {
            return vec![self.min_f];
        }
        let (low, high) = (self.min_f.ln(), self.max_f.ln());
        (0..count)
            .map(|index| (low + (high - low) * index as f64 / (count - 1) as f64).exp())
            .collect()
    }

    /// The planned ladder in the order it will actually be visited, with the
    /// interleaved low-frequency reference repeats inserted.
    fn freq_sweep_points(&self) -> Vec<FreqSweepPoint> {
        let mut ladder = self.planned_frequencies();
        match self.freq_order {
            FreqOrder::Ascending => {}
            FreqOrder::Descending => ladder.reverse(),
            FreqOrder::Alternating => {
                // Lowest, highest, second lowest, second highest, …
                let mut out = Vec::with_capacity(ladder.len());
                let (mut low, mut high) = (0usize, ladder.len());
                while low < high {
                    out.push(ladder[low]);
                    low += 1;
                    if low < high {
                        high -= 1;
                        out.push(ladder[high]);
                    }
                }
                ladder = out;
            }
            FreqOrder::Random => {
                // A seeded Fisher-Yates with a small xorshift, so the executed
                // order is reproducible from the seed recorded in the sidecar.
                let mut state = self.freq_seed.max(1);
                let mut next = || {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state
                };
                for index in (1..ladder.len()).rev() {
                    ladder.swap(index, (next() % (index as u64 + 1)) as usize);
                }
            }
        }
        let reference_hz = self.planned_frequencies().first().copied();
        let every = self.freq_reference_every as usize;
        let mut points = Vec::with_capacity(ladder.len() * 2);
        for (visited, frequency_hz) in ladder.into_iter().enumerate() {
            points.push(FreqSweepPoint {
                frequency_hz,
                is_reference: false,
            });
            // Interleave the low-frequency reference so drift across the block
            // shows up as a disagreement between its repeats (A1 checklist,
            // "interleave a low-frequency reference to expose drift").
            if let Some(reference_hz) = reference_hz.filter(|_| every > 0) {
                if (visited + 1) % every == 0 {
                    points.push(FreqSweepPoint {
                        frequency_hz: reference_hz,
                        is_reference: true,
                    });
                }
            }
        }
        points
    }

    /// Lease TTL for the whole ladder: what one rung costs, times the rungs
    /// left. A depth-sweep rung is a whole inner sweep, so it is the expensive
    /// one by a factor of the point count — a TTL sized for an `a₀` point would
    /// expire mid-curve and hand the drive back to the operator's settings.
    fn freq_sweep_lease_ttl_ms(&self, remaining_points: usize, mode: FreqSweepMode) -> u64 {
        let inner_ms = match mode {
            FreqSweepMode::A0Point => self
                .a0_lock_lease_ttl_ms()
                .saturating_add(self.sweep_lease_ttl_ms(1)),
            FreqSweepMode::DepthSweep => {
                self.sweep_lease_ttl_ms(self.sweep_count.clamp(2, 64) as usize)
            }
        };
        let per_point_ms = inner_ms.saturating_add(FREQ_CONFIRM_BASE_MS);
        (remaining_points as u64)
            .saturating_mul(per_point_ms)
            .saturating_add(60_000)
    }

    /// Kick off the multi-frequency run: validate the whole plan, then lease.
    ///
    /// Everything checkable is checked *here*, before the drive moves: a plan
    /// that cannot work at its lowest frequency should say so in a message, not
    /// two hours into a block.
    fn begin_freq_sweep(&mut self, context: &mut impl RecordingControl, mode: FreqSweepMode) {
        if self.recording.is_active()
            || self.sweep.is_some()
            || self.a0_lock.is_some()
            || self.freq_sweep.is_some()
        {
            self.message = "A recording, sweep or a₀ lock is already running".into();
            return;
        }
        if self.output_folder.trim().is_empty() {
            self.message = "Pick an output folder first — that is where the files go".into();
            return;
        }
        if !self.modulation_connected() {
            self.message = "The modulation plugin is not connected — connect it to drive the \
                            frequency"
                .into();
            return;
        }
        // Every point of the ladder ends in a recording, so ask the recording's
        // own question here. It used to be asked for the first time three stages
        // in, at point 1, after the drive had already been retargeted — which is
        // how the panel came to read "Recording: idle" mid-ladder.
        if let Some(blocker) = self.photodiode_blocker() {
            self.message = blocker;
            return;
        }
        // Written through `partial_cmp` so a NaN from the settings drag is
        // rejected rather than silently passing a negated comparison.
        let range_ok = self.min_f.partial_cmp(&0.0) == Some(std::cmp::Ordering::Greater)
            && matches!(
                self.max_f.partial_cmp(&self.min_f),
                Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
            );
        if !range_ok {
            self.message = "Set Sweep min f above 0 and Sweep max f at or above it".into();
            return;
        }
        if let Some(reason) = self.depth_a_blocker() {
            self.message = format!(
                "The frequency sweep needs a {} depth a, but {reason}",
                self.depth_source.verb()
            );
            return;
        }
        // Each mode reads a different depth setting, so each validates its own.
        match mode {
            FreqSweepMode::A0Point => {
                let target = self.a0_target;
                if !(COMMANDED_A_MIN..=COMMANDED_A_MAX).contains(&target) {
                    self.message = format!(
                        "a₀ = {target:.3} is outside the drivable \
                         {COMMANDED_A_MIN}..={COMMANDED_A_MAX}"
                    );
                    return;
                }
            }
            FreqSweepMode::DepthSweep => {
                // Same two questions `begin_sweep` asks of the depth range,
                // asked here before the drive moves rather than at the first
                // rung — a ladder that cannot record its inner sweep should say
                // so on the button press.
                if self.min_a.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                    self.message = "Set Sweep min a above 0 — a = 0 is the background reference, \
                                    which has its own button"
                        .into();
                    return;
                }
                if self.max_a.partial_cmp(&self.min_a) != Some(std::cmp::Ordering::Greater) {
                    self.message = "Sweep max a must be larger than Sweep min a".into();
                    return;
                }
            }
        }
        // The photodiode estimates `a` over one window for all frequencies, so
        // the *lowest* planned frequency decides whether the ladder is
        // measurable at all. Refuse the plan, not its 9th point.
        if let Err(reason) = self.optical_window_covers_a_cycle(self.min_f) {
            self.message = format!("Frequency sweep refused at its lowest point: {reason}");
            return;
        }
        // Only the measured source needs the camera trigger: it is what confirms
        // each commanded frequency and what anchors the fold the point is scored
        // in. Commanded mode confirms against the modulation owner instead
        // (ADR 021), so requiring markers here would refuse a ladder that can
        // run perfectly well.
        if self.depth_source.needs_a0_lock() && !self.is_marker_anchored() {
            // Without the phase-0 trigger there is nothing that can confirm the
            // drive actually reached a commanded frequency, and the fold has no
            // anchor either. Live analysis off and a missing trigger cable look
            // identical from the marker count, so name whichever one it is.
            self.message = if self.live {
                "No phase-0 trigger markers — the sweep cannot confirm a commanded frequency. \
                 Check the EXT_TRIGGER wiring from the Teensy to the camera"
                    .into()
            } else {
                "Live analysis is off, so no phase-0 markers are ingested and the sweep cannot \
                 confirm a commanded frequency. Enable Live analysis"
                    .into()
            };
            return;
        }
        let points = self.freq_sweep_points();
        if points.is_empty() {
            self.message = "Nothing to sweep: the frequency ladder has no points".into();
            return;
        }
        let now_ms = now_unix_ms();
        let lease_id = LeaseId::new(format!("a1-fsweep-{}", format_compact_utc(now_ms / 1_000)));
        let ttl_ms = self.freq_sweep_lease_ttl_ms(points.len(), mode);
        let request =
            self.modulation_request(ModulationCommandV1::AcquireLease { ttl_ms }, &lease_id);
        let lease_req = request.request_id;
        context.request_service(&request);
        let total = points.len();
        let planned_s: f64 = points
            .iter()
            .map(|point| self.planned_rung_s(mode, point.frequency_hz))
            .sum();
        self.freq_sweep = Some(FreqSweep {
            phase: FreqSweepPhase::AcquiringLease,
            mode,
            points,
            index: 0,
            lease_id,
            lease_granted: false,
            lease_req,
            freq_req: 0,
            freq_applied: false,
            confirm_deadline_ms: 0,
            skip_reason: None,
            failed: Vec::new(),
            recorded: 0,
            order: self.freq_order,
            seed: self.freq_seed,
            last_activity_ms: now_ms,
            stop_requested: false,
            eta: Eta::new(now_ms),
        });
        let estimate = format_duration(planned_s);
        self.message = match mode {
            FreqSweepMode::A0Point => format!(
                "Frequency sweep: acquiring the modulation lease for {total} points ({} order), \
                 ≈ {estimate} of bench time…",
                self.freq_order.label()
            ),
            FreqSweepMode::DepthSweep => format!(
                "Depth sweep at every frequency: acquiring the modulation lease for {total} × {} \
                 recordings ({} order), ≈ {estimate} of bench time…",
                self.sweep_count.clamp(2, 64),
                self.freq_order.label()
            ),
        };
    }

    /// Release the ladder's lease (if this run holds it) and clear the sweep.
    ///
    /// `safe_off = false` as everywhere else: stopping the drive is the owner's
    /// lease-expiry job, not a sweep's. Releasing does hand the operator's own
    /// frequency and depth back, because the owner parks them on the first
    /// retarget.
    fn finish_freq_sweep(&mut self, context: &mut impl RecordingControl, message: String) {
        if let Some(sweep) = self.freq_sweep.take() {
            if sweep.lease_granted {
                let request = self.modulation_request(
                    ModulationCommandV1::ReleaseLease {
                        safe_off: false,
                        reason: "a1 frequency sweep finished".into(),
                    },
                    &sweep.lease_id,
                );
                context.request_service(&request);
            }
        }
        self.message = message;
    }

    /// Renew the ladder's lease and retarget the drive at the current point.
    fn send_freq_sweep_frequency(&mut self, context: &mut impl RecordingControl) {
        let Some(sweep) = self.freq_sweep.as_ref() else {
            return;
        };
        let lease_id = sweep.lease_id.clone();
        let remaining = sweep.points.len().saturating_sub(sweep.index);
        let hz = sweep.frequency_hz();
        let (index, total) = (sweep.index, sweep.points.len());
        let is_reference = sweep.point().is_some_and(|point| point.is_reference);
        let mode = sweep.mode;

        let ttl_ms = self.freq_sweep_lease_ttl_ms(remaining, mode);
        let renew = self.modulation_request(ModulationCommandV1::RenewLease { ttl_ms }, &lease_id);
        context.request_service(&renew);
        let request = self.modulation_request(
            ModulationCommandV1::SetDriveFrequency {
                frequency_millihz: (hz * 1_000.0).round().max(0.0) as u64,
            },
            &lease_id,
        );
        let freq_req = request.request_id;
        context.request_service(&request);

        // The retained markers and events belong to the *previous* frequency:
        // the measured period is their mean spacing, so leaving them in place
        // would confirm the new frequency against a mixture of the two. The
        // pilot windows are frozen at a phase of the old period and are not
        // transferable either — a point recorded against them would be scored
        // in the wrong window.
        self.camera_markers_us.clear();
        self.camera_events.clear();
        self.fold_cache.replace(None);
        self.pilot_windows = None;

        let now_ms = now_unix_ms();
        if let Some(sweep) = self.freq_sweep.as_mut() {
            sweep.phase = FreqSweepPhase::SettingFrequency;
            sweep.freq_req = freq_req;
            sweep.freq_applied = false;
            sweep.skip_reason = None;
            sweep.last_activity_ms = now_ms;
        }
        self.message = format!(
            "Frequency sweep {}/{total}: retargeting the drive to {}{}…",
            index + 1,
            frequency_label(hz),
            if is_reference { " (reference)" } else { "" },
        );
    }

    /// Hand the current ladder point to the one-point event-count recording.
    ///
    /// Reached either from the finished `a₀` search (measured depth source) or
    /// straight from the confirmed frequency (commanded source, where there is
    /// no search). Both need the same question answered — *what depth is armed
    /// at this frequency?* — so both ask [`Self::armed_a0`] and neither knows
    /// which regime it is in.
    fn start_freq_sweep_recording(&mut self, context: &mut impl RecordingControl) {
        let Some((mode, hz, index, total)) = self.freq_sweep.as_ref().map(|sweep| {
            (
                sweep.mode,
                sweep.frequency_hz(),
                sweep.index,
                sweep.points.len(),
            )
        }) else {
            return;
        };
        // A non-converged lock is stored but never arms a recording, so this is
        // the single question worth asking for an `a₀` rung.
        if mode.needs_armed_depth() && self.armed_a0().is_none() {
            let reason = self
                .armed_a0_blocker()
                .unwrap_or_else(|| self.message.clone());
            self.fail_freq_sweep_point(context, reason);
            return;
        }
        if let Some(sweep) = self.freq_sweep.as_mut() {
            sweep.phase = FreqSweepPhase::Recording;
        }
        let lease = self.freq_sweep.as_ref().map(|sweep| sweep.lease_id.clone());
        let label = frequency_label(hz);
        match mode {
            FreqSweepMode::A0Point => self.begin_a0_point(context, lease),
            // The inner run is the *unchanged* amplitude sweep, handed this
            // ladder's lease so the operator's drive settings stay locked out
            // from the first frequency to the last rather than being handed
            // back between rungs.
            FreqSweepMode::DepthSweep => {
                let points = self.sweep_points();
                let message = format!(
                    "Depth sweep {}/{total} at {label}: {} depths…",
                    index + 1,
                    points.len()
                );
                self.begin_leased_sweep(
                    context,
                    SweepKind::Amplitude,
                    points,
                    None,
                    lease,
                    message,
                );
            }
        }
        if self.sweep.is_none() {
            let reason = self.message.clone();
            self.fail_freq_sweep_point(context, reason);
            return;
        }
        if mode == FreqSweepMode::A0Point {
            self.message = format!(
                "Frequency sweep {}/{total}: recording the a₀ point at {label}…",
                index + 1
            );
        }
    }

    /// Give up on the current point and move to the next one.
    ///
    /// A frequency that cannot be locked or recorded does not end the ladder:
    /// the remaining points are still worth having, and the failure is already
    /// in the lock table. It is reported in the final summary.
    fn fail_freq_sweep_point(&mut self, context: &mut impl RecordingControl, reason: String) {
        let hz = self
            .freq_sweep
            .as_ref()
            .map(FreqSweep::frequency_hz)
            .unwrap_or_default();
        if let Some(sweep) = self.freq_sweep.as_mut() {
            sweep.failed.push(hz);
        }
        self.message = format!(
            "Frequency sweep: skipping {} — {reason}",
            frequency_label(hz)
        );
        self.advance_freq_sweep(context);
    }

    /// Move to the next ladder point, or finish with a summary.
    fn advance_freq_sweep(&mut self, context: &mut impl RecordingControl) {
        // The rung that just ended is what the estimate learns from, so its
        // planned cost has to be read before the index moves past it.
        let planned_s = self
            .freq_sweep
            .as_ref()
            .and_then(|sweep| sweep.point().map(|point| (sweep.mode, point.frequency_hz)))
            .map(|(mode, frequency_hz)| self.planned_rung_s(mode, frequency_hz))
            .unwrap_or(0.0);
        let now_ms = now_unix_ms();
        let done = match self.freq_sweep.as_mut() {
            Some(sweep) => {
                sweep.eta.point_done(now_ms, planned_s);
                sweep.index += 1;
                sweep.index >= sweep.points.len()
            }
            None => return,
        };
        if !done {
            self.send_freq_sweep_frequency(context);
            return;
        }
        let Some((mode, recorded, failed, total, order, seed)) =
            self.freq_sweep.as_ref().map(|sweep| {
                (
                    sweep.mode,
                    sweep.recorded,
                    sweep.failed.clone(),
                    sweep.points.len(),
                    sweep.order,
                    sweep.seed,
                )
            })
        else {
            return;
        };
        let mut message = match mode {
            FreqSweepMode::A0Point => format!(
                "Frequency sweep complete: {recorded}/{total} points recorded ({} order, seed \
                 {seed})",
                order.label()
            ),
            FreqSweepMode::DepthSweep => format!(
                "Depth sweep at every frequency complete: {recorded}/{total} frequencies × {} \
                 depths recorded ({} order, seed {seed})",
                self.sweep_count.clamp(2, 64),
                order.label()
            ),
        };
        if !failed.is_empty() {
            let list = failed
                .iter()
                .map(|hz| frequency_label(*hz))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!(
                " — {} skipped: {list}{}",
                failed.len(),
                if mode.needs_armed_depth() && self.depth_source.needs_a0_lock() {
                    ". See the a₀ lock table"
                } else {
                    ""
                }
            ));
        }
        self.finish_freq_sweep(context, message);
    }

    // ---- declarative protocol runs -------------------------------------

    /// How much longer the whole protocol still needs the drive, plus a minute
    /// of slack.
    ///
    /// One lease for the whole file, like the frequency ladder: handing the
    /// drive back to the operator's armed settings mid-survey would let the
    /// remaining points record against them without saying so. This is what
    /// A1 *asks* for, not what it gets — the owner caps the TTL it grants, and
    /// [`Self::drive_lease_heartbeat`] is what actually keeps the lease alive.
    fn protocol_lease_ttl_ms(plan: &protocol::Protocol, from: usize) -> u64 {
        let remaining = plan.remaining_seconds(from);
        // Doubled: every point also spends time on the start/finalize
        // handshake, which is not in the protocol's own numbers.
        ((remaining * 2_000.0) as u64).saturating_add(60_000)
    }

    /// Load, validate and start the protocol named in the settings.
    ///
    /// Everything checkable is checked here, before the drive moves — the
    /// whole point of a protocol is that it runs unattended, so a file that
    /// cannot work should say so on the button press rather than at 3 a.m.
    fn begin_protocol(&mut self, context: &mut impl RecordingControl) {
        if self.recording.is_active()
            || self.sweep.is_some()
            || self.a0_lock.is_some()
            || self.freq_sweep.is_some()
            || self.protocol.is_some()
        {
            self.message = "A recording, sweep or protocol is already running".into();
            return;
        }
        if self.output_folder.trim().is_empty() {
            self.message = "Pick an output folder first — that is where the files go".into();
            return;
        }
        let path = self.protocol_path.trim().to_owned();
        if path.is_empty() {
            self.message = "Choose a protocol file first".into();
            return;
        }
        if !self.modulation_connected() {
            self.message =
                "The modulation plugin is not connected — connect it to drive the protocol".into();
            return;
        }
        if let Some(blocker) = self.photodiode_blocker() {
            self.message = blocker;
            return;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                self.message = format!("Cannot read {path}: {error}");
                return;
            }
        };
        let plan = match protocol::parse_file(&path, &text) {
            Ok(plan) => plan,
            Err(error) => {
                self.message = format!("Protocol rejected — {error}");
                return;
            }
        };
        // The photodiode measures `a` over one window for every frequency, so
        // the lowest frequency in the file decides whether the survey is
        // measurable at all. Refuse the plan, not its 40th point.
        let lowest = plan
            .points
            .iter()
            .map(|point| point.frequency_hz)
            .fold(f64::INFINITY, f64::min);
        if lowest.is_finite() {
            if let Err(reason) = self.optical_window_covers_a_cycle(lowest) {
                self.message = format!(
                    "Protocol refused at its lowest frequency ({}): {reason}",
                    frequency_label(lowest)
                );
                return;
            }
        }

        let now_ms = now_unix_ms();
        let lease_id = LeaseId::new(format!(
            "a1-protocol-{}",
            format_compact_utc(now_ms / 1_000)
        ));
        let ttl_ms = Self::protocol_lease_ttl_ms(&plan, 0);
        let request =
            self.modulation_request(ModulationCommandV1::AcquireLease { ttl_ms }, &lease_id);
        let lease_req = request.request_id;
        context.request_service(&request);

        let (means, frequencies, depths) = plan.axis_counts();
        let total = plan.points.len();
        // The plan's own seconds plus what the bench charges per point — the
        // same number the panel then counts down, so the estimate on the button
        // press and the estimate a minute later are not two different figures.
        let estimate =
            format_duration(plan.total_seconds() + total as f64 * crate::eta::POINT_OVERHEAD_S);
        self.message = format!(
            "Protocol '{}': {total} recordings ({means} × ū, {frequencies} × f, {depths} × a), \
             ≈ {estimate} of bench time — acquiring the modulation lease…",
            plan.name
        );
        self.protocol = Some(ProtocolRun {
            plan,
            phase: ProtocolPhase::AcquiringLease,
            index: 0,
            lease_id,
            lease_granted: false,
            lease_req,
            pending_reqs: Vec::new(),
            settle_until_ms: 0,
            failed: Vec::new(),
            recorded: 0,
            last_activity_ms: now_ms,
            stop_requested: false,
            skip_reason: None,
            eta: Eta::new(now_ms),
        });
    }

    /// Release the protocol's lease (if this run holds it) and clear it.
    fn finish_protocol(&mut self, context: &mut impl RecordingControl, message: String) {
        if let Some(run) = self.protocol.take() {
            if run.lease_granted {
                let request = self.modulation_request(
                    ModulationCommandV1::ReleaseLease {
                        safe_off: false,
                        reason: "a1 protocol finished".into(),
                    },
                    &run.lease_id,
                );
                context.request_service(&request);
            }
        }
        self.message = message;
    }

    /// Renew the lease and retarget all three axes at the current point.
    fn send_protocol_point(&mut self, context: &mut impl RecordingControl) {
        let Some(run) = self.protocol.as_ref() else {
            return;
        };
        let Some(point) = run.point().cloned() else {
            return;
        };
        let lease_id = run.lease_id.clone();
        let (index, total) = (run.index, run.plan.points.len());
        let ttl_ms = Self::protocol_lease_ttl_ms(&run.plan, index);

        let renew = self.modulation_request(ModulationCommandV1::RenewLease { ttl_ms }, &lease_id);
        context.request_service(&renew);

        // All three axes, every point. A protocol states the whole operating
        // condition, so nothing is left at whatever the previous point or the
        // operator happened to leave behind.
        let mut pending = Vec::with_capacity(3);
        for command in [
            ModulationCommandV1::SetOperatingPoint {
                mean_u_milli: (point.mean_u * 1_000.0).round().clamp(0.0, 1_000.0) as u32,
            },
            ModulationCommandV1::SetDriveFrequency {
                frequency_millihz: (point.frequency_hz * 1_000.0).round().max(0.0) as u64,
            },
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: depth_a_milli(point.depth_a),
            },
        ] {
            let request = self.modulation_request(command, &lease_id);
            pending.push(request.request_id);
            context.request_service(&request);
        }

        // The retained markers and events belong to the previous point's
        // frequency; the measured period is their mean spacing, so leaving
        // them would confirm this point against a mixture of the two. Pilot
        // windows are frozen at a phase of the old period and do not transfer.
        self.camera_markers_us.clear();
        self.camera_events.clear();
        self.fold_cache.replace(None);
        self.pilot_windows = None;

        let now_ms = now_unix_ms();
        if let Some(run) = self.protocol.as_mut() {
            run.phase = ProtocolPhase::Retargeting;
            run.pending_reqs = pending;
            run.skip_reason = None;
            run.last_activity_ms = now_ms;
        }
        self.message = format!(
            "Protocol {}/{total} [{}]: ū={:.2}, f={}, a={:.2}…",
            index + 1,
            point.block,
            point.mean_u,
            frequency_label(point.frequency_hz),
            point.depth_a,
        );
    }

    /// Give up on the current point and move to the next.
    fn fail_protocol_point(&mut self, context: &mut impl RecordingControl, reason: String) {
        let Some(run) = self.protocol.as_mut() else {
            return;
        };
        let index = run.index;
        run.failed.push((index, reason.clone()));
        let point = run.plan.points[index].clone();
        self.message = format!(
            "Protocol point {} (ū={:.2}, f={}, a={:.2}) skipped: {reason}",
            index + 1,
            point.mean_u,
            frequency_label(point.frequency_hz),
            point.depth_a,
        );
        self.advance_protocol(context);
    }

    /// Step to the next point, or finish.
    fn advance_protocol(&mut self, context: &mut impl RecordingControl) {
        let Some(run) = self.protocol.as_mut() else {
            return;
        };
        let now_ms = now_unix_ms();
        // What the row that just ended was planned to cost, before the index
        // moves past it — that difference is the whole estimate.
        let planned_s = run
            .point()
            .map(|point| point.duration_s as f64 + point.settle_s + crate::eta::POINT_OVERHEAD_S)
            .unwrap_or(0.0);
        run.eta.point_done(now_ms, planned_s);
        run.index += 1;
        run.last_activity_ms = now_ms;
        if run.index < run.plan.points.len() && !run.stop_requested {
            self.send_protocol_point(context);
            return;
        }
        let (name, recorded, failed, total) = (
            run.plan.name.clone(),
            run.recorded,
            run.failed.clone(),
            run.plan.points.len(),
        );
        let stopped = run.stop_requested;
        let mut message = format!(
            "Protocol '{name}' {}: {recorded}/{total} recorded",
            if stopped { "stopped" } else { "finished" }
        );
        if !failed.is_empty() {
            // Name the reasons, not just the count: an unattended run's whole
            // report is this one line.
            let mut reasons: Vec<String> = failed
                .iter()
                .map(|(_, reason)| reason.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            reasons.truncate(3);
            message.push_str(&format!(
                " — {} skipped ({})",
                failed.len(),
                reasons.join("; ")
            ));
        }
        self.finish_protocol(context, message);
    }

    /// Advance a protocol run one control tick. Runs outermost: a point it
    /// starts hands off to the recording coordinator on the same tick.
    fn drive_protocol(&mut self, context: &mut impl RecordingControl) {
        if self.protocol.is_none() {
            if self.protocol_pending {
                self.protocol_pending = false;
                self.begin_protocol(context);
            }
            return;
        }
        if self.protocol_pending {
            // Say so rather than swallowing the press: Stop is a different
            // button, and a silently ignored one reads as a dead control.
            self.protocol_pending = false;
            self.message = "A protocol is already running — press Stop to end it".into();
        }
        let now_ms = now_unix_ms();
        let (phase, stop_requested, lease_granted, retargets_left, settle_until_ms, last_activity) = {
            let run = self.protocol.as_ref().expect("run checked above");
            (
                run.phase,
                run.stop_requested,
                run.lease_granted,
                run.pending_reqs.len(),
                run.settle_until_ms,
                run.last_activity_ms,
            )
        };

        // A stop waits for the recording in flight to wind down, then ends the
        // run — a protocol that abandoned a half-written file would leave a
        // truncated RAW behind.
        if stop_requested {
            if self.recording.is_active() {
                self.recording.stop_requested = true;
                return;
            }
            self.advance_protocol(context);
            return;
        }

        match phase {
            ProtocolPhase::AcquiringLease => {
                if !lease_granted {
                    if now_ms.saturating_sub(last_activity) > REPLY_TIMEOUT_MS {
                        self.finish_protocol(
                            context,
                            "Protocol aborted: the modulation plugin did not grant the lease"
                                .into(),
                        );
                    }
                    return;
                }
                self.send_protocol_point(context);
            }
            ProtocolPhase::Retargeting => {
                if let Some(reason) = self
                    .protocol
                    .as_mut()
                    .and_then(|run| run.skip_reason.take())
                {
                    self.fail_protocol_point(context, reason);
                    return;
                }
                if retargets_left > 0 {
                    if now_ms.saturating_sub(last_activity) > REPLY_TIMEOUT_MS {
                        self.fail_protocol_point(
                            context,
                            "the modulation plugin did not apply the requested drive".into(),
                        );
                    }
                    return;
                }
                let settle_ms = self
                    .protocol
                    .as_ref()
                    .and_then(|run| run.point())
                    .map(|point| (point.settle_s * 1_000.0).round().max(0.0) as u64)
                    .unwrap_or(0);
                if let Some(run) = self.protocol.as_mut() {
                    run.phase = ProtocolPhase::Settling;
                    run.settle_until_ms = now_ms.saturating_add(settle_ms);
                    run.last_activity_ms = now_ms;
                }
            }
            ProtocolPhase::Settling => {
                if now_ms < settle_until_ms {
                    return;
                }
                let duration_s = self
                    .protocol
                    .as_ref()
                    .and_then(|run| run.point())
                    .map(|point| point.duration_s)
                    .unwrap_or(self.duration_s);
                // The point's own duration wins over the panel's: the file says
                // how long each point runs, and a survey whose lengths silently
                // came from the UI would not be reproducible from the protocol
                // alone. Through the override, not `duration_s` — see
                // `pending_duration_s`.
                self.pending_duration_s = Some(duration_s);
                if let Some(run) = self.protocol.as_mut() {
                    run.phase = ProtocolPhase::Recording;
                    run.last_activity_ms = now_ms;
                }
                // The row says what it is: a protocol can carry its own
                // background and pilot, so a survey does not need two button
                // presses before it can be started.
                let role = self
                    .protocol
                    .as_ref()
                    .and_then(|run| run.point())
                    .map(|point| match point.role {
                        protocol::PointRole::Normal => RecRole::Normal,
                        protocol::PointRole::Pilot => RecRole::Pilot,
                        protocol::PointRole::Background => RecRole::Background,
                    })
                    .unwrap_or(RecRole::Normal);
                self.begin_recording(context, role);
                // `begin_recording` refuses through `message` rather than a
                // result, so a point that never started has to be caught here
                // or the run would wait on a recording that does not exist.
                if !self.recording.is_active() {
                    let reason = self.message.clone();
                    self.fail_protocol_point(context, reason);
                }
            }
            ProtocolPhase::Recording => {
                if self.recording.is_active() {
                    return;
                }
                if self.recording_completed_ok {
                    if let Some(run) = self.protocol.as_mut() {
                        run.recorded += 1;
                    }
                    self.advance_protocol(context);
                } else {
                    let reason = self.message.clone();
                    self.fail_protocol_point(context, reason);
                }
            }
        }
    }

    /// Routes modulation-service replies belonging to the protocol run.
    fn on_protocol_reply(&mut self, reply: &PluginServiceReply) -> bool {
        let Some(run) = self.protocol.as_ref() else {
            return false;
        };
        let lease_req = run.lease_req;
        let is_retarget = run.pending_reqs.contains(&reply.request_id);
        if reply.request_id == lease_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(run) = self.protocol.as_mut() {
                        run.lease_granted = true;
                        run.last_activity_ms = now_unix_ms();
                    }
                }
                PluginServiceOutcome::Rejected { message, .. } => {
                    self.message =
                        format!("Protocol aborted: modulation lease rejected: {message}");
                    if let Some(run) = self.protocol.as_mut() {
                        run.stop_requested = true;
                    }
                }
            }
            return true;
        }
        if !is_retarget {
            return false;
        }
        let now_ms = now_unix_ms();
        match &reply.outcome {
            PluginServiceOutcome::Accepted { .. } => {
                if let Some(run) = self.protocol.as_mut() {
                    run.pending_reqs.retain(|id| *id != reply.request_id);
                    run.last_activity_ms = now_ms;
                }
            }
            PluginServiceOutcome::Rejected { message, .. } => {
                // Carry the owner's own wording through to the skip message:
                // "ū=0.90 rejected: peak exceeds the lobe ceiling" tells the
                // operator which line of the file to fix, "retarget failed"
                // does not.
                if let Some(run) = self.protocol.as_mut() {
                    run.pending_reqs.clear();
                    run.skip_reason = Some(message.clone());
                    run.last_activity_ms = now_ms;
                }
            }
        }
        true
    }

    /// Advance the multi-frequency run one control tick. Runs before the lock
    /// and the point sweep, so a child it starts runs on the same tick.
    fn drive_freq_sweep(&mut self, context: &mut impl RecordingControl) {
        if self.freq_sweep.is_none() {
            if let Some(mode) = self.freq_sweep_pending.take() {
                self.begin_freq_sweep(context, mode);
            }
            return;
        }
        self.freq_sweep_pending = None;
        let now_ms = now_unix_ms();
        let (phase, stop_requested, lease_granted, freq_applied, last_activity_ms, index, total) = {
            let sweep = self.freq_sweep.as_ref().expect("sweep checked above");
            (
                sweep.phase,
                sweep.stop_requested,
                sweep.lease_granted,
                sweep.freq_applied,
                sweep.last_activity_ms,
                sweep.index,
                sweep.points.len(),
            )
        };
        // A stop propagates into whichever child is running; the ladder ends
        // once that child has let go.
        if stop_requested {
            if let Some(lock) = self.a0_lock.as_mut() {
                lock.stop_requested = true;
                return;
            }
            if let Some(sweep) = self.sweep.as_mut() {
                sweep.stop_requested = true;
                return;
            }
            let message = if self.message.is_empty() {
                "Frequency sweep stopped".into()
            } else {
                self.message.clone()
            };
            self.finish_freq_sweep(context, message);
            return;
        }
        match phase {
            FreqSweepPhase::AcquiringLease => {
                if lease_granted {
                    self.send_freq_sweep_frequency(context);
                } else if now_ms.saturating_sub(last_activity_ms) > REPLY_TIMEOUT_MS {
                    self.finish_freq_sweep(
                        context,
                        "Frequency sweep aborted: timed out acquiring the modulation lease".into(),
                    );
                }
            }
            FreqSweepPhase::SettingFrequency => {
                // A refused frequency is a property of this point, not of the
                // ladder; the owner already said why.
                if let Some(reason) = self
                    .freq_sweep
                    .as_mut()
                    .and_then(|sweep| sweep.skip_reason.take())
                {
                    self.fail_freq_sweep_point(context, reason);
                } else if freq_applied {
                    let hz = self
                        .freq_sweep
                        .as_ref()
                        .map(FreqSweep::frequency_hz)
                        .unwrap_or_default();
                    // Confirming needs whole cycles at the *new* period, so the
                    // budget has to scale with it: 4 cycles at 0.1 Hz is 40 s.
                    let cycles_ms = if hz > 0.0 {
                        (FREQ_CONFIRM_CYCLES / hz * 1_000.0).ceil() as u64
                    } else {
                        0
                    };
                    if let Some(sweep) = self.freq_sweep.as_mut() {
                        sweep.phase = FreqSweepPhase::ConfirmingFrequency;
                        sweep.confirm_deadline_ms =
                            now_ms.saturating_add(FREQ_CONFIRM_BASE_MS.max(cycles_ms * 3));
                    }
                    self.message = format!(
                        "Frequency sweep {}/{total}: waiting for the trigger to report {}…",
                        index + 1,
                        frequency_label(hz),
                    );
                } else if now_ms.saturating_sub(last_activity_ms) > REPLY_TIMEOUT_MS {
                    self.finish_freq_sweep(
                        context,
                        "Frequency sweep aborted: timed out retargeting the drive frequency".into(),
                    );
                }
            }
            FreqSweepPhase::ConfirmingFrequency => {
                let hz = self
                    .freq_sweep
                    .as_ref()
                    .map(FreqSweep::frequency_hz)
                    .unwrap_or_default();
                // Which side is entitled to say the drive really reached the new
                // frequency.
                //
                // Measured mode holds out for the camera's phase-0 markers: they
                // *define* the period, the fold that scores the point is anchored
                // on them, and an ACK from the firmware only says the table was
                // accepted, not that the light is modulating at that rate. Enough
                // markers must have arrived at the new period for their mean
                // spacing to mean anything.
                //
                // Commanded mode asks the modulation owner instead. That is the
                // same contract it already relies on for the depth — if the
                // owner's acknowledged waveform is trusted to state `a`, it is
                // trusted to state `f` — and it does not strand a bench whose
                // camera trigger is not wired, which is the whole reason the
                // commanded source exists (ADR 021). The live fold goes
                // free-running without markers; the recorded RAW and PDQ, which
                // are what the offline fit reads, are unaffected.
                let confirmed = if self.depth_source.needs_a0_lock() {
                    self.camera_markers_us.len() as f64 >= FREQ_CONFIRM_CYCLES
                        && self
                            .frequency_hz()
                            .is_some_and(|measured| same_frequency(measured, hz))
                } else {
                    self.acknowledged_frequency_hz()
                        .is_some_and(|acknowledged| same_frequency(acknowledged, hz))
                };
                let deadline = self
                    .freq_sweep
                    .as_ref()
                    .map(|sweep| sweep.confirm_deadline_ms)
                    .unwrap_or_default();
                if confirmed {
                    if let Err(reason) = self.optical_window_covers_a_cycle(hz) {
                        self.fail_freq_sweep_point(context, reason);
                        return;
                    }
                    // The search stands between the frequency and the recording
                    // in exactly one case: an `a₀` rung whose depth is measured.
                    // A depth sweep commands and settles every `a` in its range
                    // itself, so there is nothing for a lock to contribute at
                    // any frequency, in either depth source.
                    let needs_search = self
                        .freq_sweep
                        .as_ref()
                        .is_some_and(|sweep| sweep.mode.needs_armed_depth())
                        && self.depth_source.needs_a0_lock();
                    if !needs_search {
                        self.start_freq_sweep_recording(context);
                        return;
                    }
                    if let Some(sweep) = self.freq_sweep.as_mut() {
                        sweep.phase = FreqSweepPhase::Locking;
                    }
                    let lease = self.freq_sweep.as_ref().map(|sweep| sweep.lease_id.clone());
                    self.begin_a0_lock(context, lease);
                    if self.a0_lock.is_none() {
                        // `begin_a0_lock` refused and said why; keep its wording.
                        let reason = self.message.clone();
                        self.fail_freq_sweep_point(context, reason);
                    }
                } else if now_ms >= deadline {
                    let reason = if self.depth_source.needs_a0_lock() {
                        let measured = self
                            .frequency_hz()
                            .map_or_else(|| "—".into(), frequency_label);
                        format!(
                            "the trigger never reported it (measured {measured} from {} markers)",
                            self.camera_markers_us.len()
                        )
                    } else {
                        let acknowledged = self
                            .acknowledged_frequency_hz()
                            .map_or_else(|| "—".into(), frequency_label);
                        format!(
                            "the modulation plugin never acknowledged it (its armed drive still \
                             reads {acknowledged})"
                        )
                    };
                    self.fail_freq_sweep_point(context, reason);
                }
            }
            FreqSweepPhase::Locking => {
                if self.a0_lock.is_some() {
                    return;
                }
                self.start_freq_sweep_recording(context);
            }
            FreqSweepPhase::Recording => {
                if self.sweep.is_some() || self.recording.is_active() {
                    return;
                }
                // The inner run's own verdict, not the last recording's. A
                // depth sweep that gives up on point 4 of 5 leaves
                // `recording_completed_ok` true from point 3, which would have
                // counted a half-recorded curve as a finished rung.
                if self.last_sweep_completed_ok {
                    if let Some(sweep) = self.freq_sweep.as_mut() {
                        sweep.recorded += 1;
                    }
                    self.advance_freq_sweep(context);
                } else {
                    let reason = self.message.clone();
                    self.fail_freq_sweep_point(context, reason);
                }
            }
        }
    }

    /// Routes modulation-service replies belonging to the frequency sweep.
    fn on_freq_sweep_reply(&mut self, reply: &PluginServiceReply) -> bool {
        let Some((lease_req, freq_req)) = self
            .freq_sweep
            .as_ref()
            .map(|sweep| (sweep.lease_req, sweep.freq_req))
        else {
            return false;
        };
        let abort = |this: &mut Self, message: String| {
            this.message = message;
            if let Some(sweep) = this.freq_sweep.as_mut() {
                sweep.stop_requested = true;
            }
        };
        if reply.request_id == lease_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(sweep) = self.freq_sweep.as_mut() {
                        sweep.lease_granted = true;
                        sweep.last_activity_ms = now_unix_ms();
                    }
                }
                PluginServiceOutcome::Rejected { message, .. } => abort(
                    self,
                    format!("Frequency sweep aborted: modulation lease rejected: {message}"),
                ),
            }
            true
        } else if reply.request_id == freq_req {
            match &reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    if let Some(sweep) = self.freq_sweep.as_mut() {
                        sweep.freq_applied = true;
                        sweep.last_activity_ms = now_unix_ms();
                    }
                }
                // A refused frequency is a property of this point, not of the
                // ladder: skip it and keep the remaining decades. The skip runs
                // on the next tick, through the one path that advances the
                // ladder, carrying the owner's wording.
                PluginServiceOutcome::Rejected { message, .. } => {
                    if let Some(sweep) = self.freq_sweep.as_mut() {
                        sweep.skip_reason =
                            Some(format!("the drive rejected the frequency: {message}"));
                    }
                }
            }
            true
        } else {
            false
        }
    }

    fn a0_locks_dataset(&self) -> TableDatasetV1 {
        let column = |id: &str, values: Vec<String>| TableColumnData {
            column_id: id.into(),
            values: TableColumnValues::String(values),
        };
        let map = |select: fn(&A0LockPoint) -> String| {
            self.a0_locks.iter().map(select).collect::<Vec<_>>()
        };
        TableDatasetV1 {
            columns: vec![
                column("frequency", map(|lock| frequency_label(lock.frequency_hz))),
                column("target_a", map(|lock| format!("{:.3}", lock.target_a))),
                column(
                    "commanded_a",
                    map(|lock| format!("{:.3}", lock.commanded_a)),
                ),
                column("measured_a", map(|lock| format!("{:.3}", lock.measured_a))),
                column("depth_source", map(|lock| lock.depth_source.verb().into())),
                column("trials", map(|lock| lock.trials.to_string())),
                column(
                    "state",
                    map(|lock| {
                        if lock.converged {
                            "locked".into()
                        } else {
                            "not converged".into()
                        }
                    }),
                ),
                column(
                    "locked_at",
                    map(|lock| format_iso_utc(lock.locked_at_unix_ms / 1_000)),
                ),
            ],
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
        if self.on_sweep_reply(reply)
            || self.on_a0_lock_reply(reply)
            || self.on_freq_sweep_reply(reply)
            || self.on_protocol_reply(reply)
        {
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
        let mod_optical = self
            .modulation
            .as_ref()
            .and_then(|state| state.optical_drive.as_ref());
        let optical = self.fresh_optical_summary();
        if optical.is_none() {
            return Err(
                "cannot write a quantitative A1 sidecar without a fresh photodiode optical \
                 summary from a confirmed I_tot anchor"
                    .into(),
            );
        }
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
            depth_a_source: self.depth_source.label().into(),
            depth_a: self.depth_a(),
            sweep: {
                let point = self
                    .sweep
                    .as_ref()
                    .filter(|sweep| sweep.phase == SweepPhase::Recording);
                SweepSidecar {
                    min_a: self.min_a,
                    max_a: self.max_a,
                    requested_a: point.map(Sweep::target_a),
                    commanded_a: point.map(Sweep::commanded_a),
                    point_index: point.map(|sweep| sweep.index + 1),
                    point_total: point.map(Sweep::total),
                }
            },
            a0_lock: self
                .sweep
                .as_ref()
                .and_then(|sweep| sweep.lock.as_ref())
                .map(|lock| A0LockSidecar {
                    target_a: lock.target_a,
                    commanded_a: lock.commanded_a,
                    measured_a_at_lock: lock.measured_a,
                    depth_source: lock.depth_source.label().into(),
                    frequency_hz_at_lock: lock.frequency_hz,
                    trials: lock.trials,
                    converged: lock.converged,
                    locked_at_utc: format_iso_utc(lock.locked_at_unix_ms / 1_000),
                }),
            frequency_sweep: self.freq_sweep.as_ref().and_then(|sweep| {
                sweep.point().map(|point| FreqSweepSidecar {
                    min_f: self.min_f,
                    max_f: self.max_f,
                    planned_points: self.freq_count as usize,
                    point_index: sweep.index + 1,
                    point_total: sweep.points.len(),
                    order: sweep.order.label().into(),
                    seed: sweep.seed,
                    is_reference: point.is_reference,
                    requested_frequency_hz: point.frequency_hz,
                })
            }),
            protocol: self
                .protocol
                .as_ref()
                .filter(|run| run.phase == ProtocolPhase::Recording)
                .and_then(|run| {
                    let point = run.point()?;
                    Some(ProtocolSidecar {
                        name: run.plan.name.clone(),
                        block: point.block.clone(),
                        point_index: run.index + 1,
                        point_total: run.plan.points.len(),
                        point_tag: point.tag(),
                        requested_mean_u: point.mean_u,
                        requested_frequency_hz: point.frequency_hz,
                        requested_depth_a: point.depth_a,
                        settle_s: point.settle_s,
                        duration_s: point.duration_s,
                    })
                }),
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
                calibration_id: self
                    .modulation
                    .as_ref()
                    .and_then(|state| state.calibration_id.clone()),
                optical_target: mod_optical.map(|drive| match drive.target {
                    OpticalTargetV1::LogSine => "log_sine".into(),
                    OpticalTargetV1::LinearSine => "linear_sine".into(),
                }),
                requested_mean_u: mod_optical
                    .map(|drive| f64::from(drive.requested_mean_u_milli) / 1_000.0),
                resolved_mean_u: mod_optical
                    .map(|drive| f64::from(drive.resolved_mean_u_milli) / 1_000.0),
                internal_u: mod_optical.map(|drive| f64::from(drive.internal_u_milli) / 1_000.0),
                requested_a: mod_optical.map(|drive| f64::from(drive.depth_a_milli) / 1_000.0),
                v_null_dac: mod_optical.map(|drive| drive.v_null_dac),
                v_peak_dac: mod_optical.map(|drive| drive.v_peak_dac),
                center_dac: a1_config.map(|c| c.center_dac),
                amplitude_dac: a1_config.map(|c| c.amplitude_dac),
                waveform: modulation
                    .and_then(|t| t.waveform.as_ref())
                    .map(waveform_label),
            },
            optical: OpticalSidecar {
                measured_a: optical.map(|o| o.measured_log_contrast),
                geometric_mean_excitation_volts: optical
                    .map(|o| (o.excitation_min_volts * o.excitation_max_volts).sqrt()),
                excitation_min_volts: optical.map(|o| o.excitation_min_volts),
                excitation_max_volts: optical.map(|o| o.excitation_max_volts),
                excitation_headroom_volts: optical.map(|o| o.excitation_headroom_volts),
                low_clip_fraction: optical.map(|o| o.low_clip_fraction),
                high_clip_fraction: optical.map(|o| o.high_clip_fraction),
                measured_frequency_hz: optical.and_then(|o| o.measured_frequency_hz),
                adc_calibration_id: optical.map(|o| o.calibration.adc_calibration_id.clone()),
                dark_id: optical.map(|o| o.calibration.dark_id.clone()),
                total_power_anchor_id: optical.map(|o| o.calibration.anchor_id.clone()),
                dark_volts: optical.map(|o| o.calibration.dark_volts),
                total_power_volts: optical.map(|o| o.calibration.total_power_volts),
            },
            camera: CameraSidecar {
                roi_x: roi.x,
                roi_y: roi.y,
                roi_width: roi.width,
                roi_height: roi.height,
                masked_pixels: self.masked_pixels.len(),
                n_valid: self.valid_pixel_count(),
            },
            sensor: self.recorded_sensor().map(|sensor| {
                let codes = sensor.bias_codes.map(|readback| readback.current);
                SensorSidecar {
                    temperature_c: sensor.temperature_c,
                    pixel_dead_time_us: sensor.pixel_dead_time_us,
                    illumination_lux: sensor.illumination_lux,
                    reading_age_s: sensor.age_s,
                    bias_diff_on: codes.map(|c| c.diff_on),
                    bias_diff_off: codes.map(|c| c.diff_off),
                    bias_fo: codes.map(|c| c.fo),
                    bias_hpf: codes.map(|c| c.hpf),
                    bias_refr: codes.map(|c| c.refr),
                }
            }),
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
                sensor_readout: self.recording.sensor_readout_path.clone(),
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
    /// The modulation depth this run was driven and judged by, and which source
    /// produced it (`photodiode_measured` / `modulation_commanded`).
    ///
    /// Written on every run, so offline analysis never has to infer the depth's
    /// provenance from which of `optical.measured_a` and `modulation.requested_a`
    /// happens to be present. A commanded depth is an open-loop number carrying
    /// the Pockels calibration's error; a fit that mixes the two sources without
    /// looking here would silently mix two error budgets.
    depth_a_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    depth_a: Option<f64>,
    sweep: SweepSidecar,
    /// Present on **event-count** points: the `a₀` lock this point replayed.
    #[serde(skip_serializing_if = "Option::is_none")]
    a0_lock: Option<A0LockSidecar>,
    /// Present on points recorded by the automatic frequency ladder.
    #[serde(skip_serializing_if = "Option::is_none")]
    frequency_sweep: Option<FreqSweepSidecar>,
    /// Present on points recorded by a declarative protocol run: the line of
    /// the file that asked for this recording, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    protocol: Option<ProtocolSidecar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pilot: Option<PilotSidecar>,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<BackgroundSidecar>,
    modulation: ModulationSidecar,
    optical: OpticalSidecar,
    camera: CameraSidecar,
    /// Absent when the host had no camera able to measure these (replay,
    /// imports, a sensor without a monitoring block).
    #[serde(skip_serializing_if = "Option::is_none")]
    sensor: Option<SensorSidecar>,
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
    /// The depth the drive was *commanded* to for this point. Equal to
    /// `requested_a` on the amplitude sweep; on an event-count point it is the
    /// `a₀`-locked depth, which differs by the drive roll-off at that frequency.
    #[serde(skip_serializing_if = "Option::is_none")]
    commanded_a: Option<f64>,
    /// 1-based point position within the sweep; absent on manual recordings.
    #[serde(skip_serializing_if = "Option::is_none")]
    point_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    point_total: Option<usize>,
}

/// The `a₀` lock an **event-count** point replayed: the closed-loop trim that
/// made the photodiode measure the frozen `a₀` at this frequency.
#[derive(Serialize)]
struct A0LockSidecar {
    target_a: f64,
    commanded_a: f64,
    measured_a_at_lock: f64,
    /// Which source `measured_a_at_lock` came from — see `depth_a_source`.
    depth_source: String,
    frequency_hz_at_lock: f64,
    trials: u32,
    converged: bool,
    locked_at_utc: String,
}

/// The automatic frequency ladder this point belongs to.
///
/// The executed order and its seed are part of the frozen session schedule the
/// A1 checklist asks for, so they belong in every point rather than only in an
/// operator's notebook: a block is only interpretable if you can tell which
/// frequency was recorded when.
#[derive(Serialize)]
struct FreqSweepSidecar {
    min_f: f64,
    max_f: f64,
    planned_points: usize,
    /// Position in the *executed* order, references included.
    point_index: usize,
    point_total: usize,
    order: String,
    seed: u64,
    /// True for the interleaved low-frequency reference repeats.
    is_reference: bool,
    /// The ladder asked for this frequency; `[trigger] measured_frequency_hz`
    /// is what the phase-0 markers reported when the point was recorded.
    requested_frequency_hz: f64,
}

/// The protocol row this recording is, as the file asked for it.
///
/// A protocol names every axis explicitly, which is exactly what makes its rows
/// indistinguishable once they are on disk: the panel is not armed at anything
/// that would separate them, so without this section the only difference
/// between two rows' sidecars is the second they started. What is written here
/// is the *request* — what the drive reported back is in `[modulation]`, and
/// what the photodiode measured is in `[optical]`, so a row that failed to
/// reach its point can still be told apart from one that hit it.
#[derive(Serialize)]
struct ProtocolSidecar {
    /// `name` from the protocol file.
    name: String,
    /// The `[[block]]` name, or a CSV `label`, this row came from.
    block: String,
    /// 1-based position within the whole expanded protocol.
    point_index: usize,
    point_total: usize,
    /// The same fragment that appears in this recording's file names.
    point_tag: String,
    /// Normalized cycle-mean lobe point `ū` — the `I_k` axis.
    requested_mean_u: f64,
    requested_frequency_hz: f64,
    requested_depth_a: f64,
    settle_s: f64,
    /// The row's own duration, which overrides the panel's for the run.
    duration_s: i64,
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
    calibration_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optical_target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_mean_u: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_mean_u: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    internal_u: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    requested_a: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    v_null_dac: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    v_peak_dac: Option<u16>,
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
    geometric_mean_excitation_volts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excitation_min_volts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excitation_max_volts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    excitation_headroom_volts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    low_clip_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    high_clip_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_frequency_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adc_calibration_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dark_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_power_anchor_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dark_volts: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_power_volts: Option<f64>,
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

/// Bench conditions the sensor measured for itself at the start of the run.
///
/// Provenance, never an input: the `q_p(a, f)` response depends on the pixel
/// dead time and on the scene illumination, and the die temperature moves the
/// biases, so a row that cannot be compared to another has to be identifiable
/// as such afterwards. Present only when the host was streaming from a camera
/// with a monitoring block — an offline re-run of the same RAW has no sensor to
/// ask, and every field stays absent rather than becoming zero.
#[derive(Serialize)]
struct SensorSidecar {
    /// Sensor die temperature, °C.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature_c: Option<f32>,
    /// Measured pixel dead time (refractory period), µs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pixel_dead_time_us: Option<f32>,
    /// Scene illumination integrated by the sensor, lux.
    #[serde(skip_serializing_if = "Option::is_none")]
    illumination_lux: Option<f32>,
    /// Seconds between the host's last read of these values and the moment the
    /// recording started — the host polls at a few hertz, so this is never 0.
    reading_age_s: f64,
    /// Absolute programmed bias codes, and the per-unit factory trim the
    /// host's relative offsets are expressed against.
    #[serde(skip_serializing_if = "Option::is_none")]
    bias_diff_on: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bias_diff_off: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bias_fo: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bias_hpf: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bias_refr: Option<u8>,
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
    /// Compacted per-channel sensor readout for this run — the die
    /// temperature, pixel dead time and illumination the host polled while it
    /// was recording.
    ///
    /// Absent whenever the host wrote no telemetry companion. Usually that is
    /// the host's own **Record sensor monitoring** switch being off, not a
    /// camera without a monitoring block: the switch governs the whole file
    /// and A1 cannot ask for it. The single-point readings in `[sensor]` come
    /// from the context bus and are there either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    sensor_readout: Option<String>,
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

/// Trims free text down to what a recording-metadata value may carry.
///
/// The photodiode owner bounds every value at 1 KiB and refuses the whole
/// `BeginRecording` if one is over — a limit worth staying well clear of, since
/// these strings come from a protocol file nobody validated for length.
fn clamp_metadata(text: &str) -> String {
    const MAX_CHARS: usize = 120;
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_owned();
    }
    trimmed.chars().take(MAX_CHARS).collect()
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

/// Wall-clock time of day, for "come back at". UTC like every other timestamp
/// this plugin writes — a local time here and UTC in the filenames would be two
/// clocks in one panel.
fn format_clock_utc(unix_secs: u64) -> String {
    let (_, _, _, hh, mm, _) = ymd_hms(unix_secs);
    format!("{hh:02}:{mm:02} UTC")
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
                //
                // Every runner counts, not just a recording in flight: a
                // protocol or ladder spends the gap between two points
                // retargeting the drive, and the stop boundary of the point
                // just finished lands squarely in it. Asking only about the
                // recording wiped the survey's own pilot windows and
                // background floor between every pair of points.
                if self.automation_active() {
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
        // Mirrored above the `live` gate on purpose: these are recorded with
        // every run, and recordings are made with Live analysis off just as
        // often as with it on. Absent whenever the host has no camera that can
        // measure them (replay, imports, a sensor without a monitoring block),
        // which stays `None` rather than becoming a zero.
        if let Some(monitoring) = context
            .get::<SensorMonitoringV1>(CTX_SENSOR_MONITORING)
            .ok()
            .flatten()
        {
            self.sensor = Some(monitoring);
        }
        if !self.live {
            // Drop the analysis buffers on the way out, not just stop filling
            // them — see `drop_live_buffers`.
            if self.drop_live_buffers() {
                self.bump();
            }
            return;
        }

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
        } else {
            // Fallback (no retained history available): accumulate the
            // best-effort preview-frame events, then trim to the same analysis
            // window the exact path uses. Without the trim the buffer grew to
            // MAX_EVENTS and then stopped accepting anything at all, so the
            // fold silently spanned an ever-widening window and finally froze
            // on a stale 4M-event buffer while the plots still looked live.
            self.camera_events
                .extend(frame.events().iter().map(ffi_to_camera_event));
            let window_start = window_end.saturating_sub(window_us);
            let keep_from = self
                .camera_events
                .partition_point(|event| event.timestamp_us < window_start);
            if keep_from > 0 {
                self.camera_events.drain(..keep_from);
            }
            // Hard ceiling as well: a window longer than the event buffer can
            // hold must drop the oldest events, not stop taking new ones.
            if self.camera_events.len() > MAX_EVENTS {
                let excess = self.camera_events.len() - MAX_EVENTS;
                self.camera_events.drain(..excess);
            }
            self.camera_markers_us
                .retain(|&marker| marker >= window_start);
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
            self.load_a0_locks();
        }
        // Before the runners: a lease that lapses is not the runners' problem
        // to notice, and the owner safe-offs the drive the moment it does.
        self.drive_lease_heartbeat(context);
        // Outermost first: the protocol and the frequency sweep each start the
        // stage below them, and each of those starts its own next stage, so one
        // tick carries a hand-off all the way down. They are mutually exclusive
        // at the top, guarded where they begin.
        self.drive_protocol(context);
        self.drive_freq_sweep(context);
        self.drive_a0_lock(context);
        self.drive_sweep(context);
        self.drive_recording(context);
        // The fold reflects live snapshots (T, a) even between frames.
        self.bump();
    }

    fn settings_schema(&self) -> SettingsSchema {
        // The record buttons stay disabled until the recording has a
        // destination, instead of failing with a status message after a click.
        let can_record = !self.output_folder.trim().is_empty();
        // Deliberately *not* gated on "is something running": `settings_schema`
        // is rendered by the UI mirror, and every run — the recording, the
        // sweeps, the ladder, the protocol — lives on the live worker, which is
        // the only instance the host calls `process_control` on. A mirror
        // reading its own always-idle state would disable nothing and mislead
        // the next reader into thinking it did. The authoritative interlocks
        // stay worker-side, where each `begin_*` refuses with a message that
        // names what is already running (ADR 010, same reason as the modulation
        // plugin's `calibration_offered`).
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Live analysis".into(),
                    description: Some(
                        "A live look at the camera events folded against the modulation cycle. \
                         Nothing is saved from here — but almost everything else reads it, so \
                         it belongs at the top rather than buried below the recording controls.\n\n\
                         Leave it ON. The frequency sweep and the protocol both need it to see \
                         the phase-0 trigger, and the response curve below scores its points \
                         out of the same buffer."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "live".into(),
                            label: "Live analysis".into(),
                            tooltip: Some(
                                "On: read incoming events and triggers and update the plots. \
                                 Off: the buffer is dropped and nothing is read at all — the \
                                 frequency sweep and the protocol will refuse to start."
                                    .into(),
                            ),
                            kind: SettingKind::Bool { default: self.live },
                        },
                        SettingItem {
                            key: "analysis_window_ms".into(),
                            label: "Analysis window (ms)".into(),
                            tooltip: Some(
                                "How far back the live plots look. Longer covers more cycles but \
                                 folds more events on every update — if the plots feel heavy, \
                                 shorten this first."
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
                            tooltip: Some("Empties the buffer and resets the live plots.".into()),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "window_floor".into(),
                            label: "Window width threshold".into(),
                            tooltip: Some(
                                "How the bright and dark windows are found automatically: each \
                                 one widens out from its busiest moment until the event rate \
                                 drops below this fraction of the peak. 0.10 = stop at 10 %."
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
                            label: "Add response-curve point (at the current depth)".into(),
                            tooltip: Some(
                                "Adds one dot to the response curve, using the events in the \
                                 live buffer and the depth measured right now. A preview to \
                                 check the shape looks sensible — saves no files, and the real \
                                 fit is done afterwards from the recorded ones."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "clear_curve".into(),
                            label: "Clear the response curve".into(),
                            tooltip: Some("Removes all the dots from the curve.".into()),
                            kind: SettingKind::Button { enabled: true },
                        },
                    ],
                },
                SettingsSection {
                    label: "Modulation depth a".into(),
                    description: Some(
                        "Where the depth a comes from. Everything that needs a depth — the \
                         sweeps, the protocol, the response curve — reads this one setting.\n\n\
                         The photodiode is the honest source: it watches the light itself. But \
                         it only reports a depth when it can prove its window covers whole \
                         modulation cycles, which needs the phase-0 trigger markers on the \
                         photodiode's own stream. Without them (no trigger, or a frequency so \
                         low that two cycles do not fit in the photodiode's cache) it reports \
                         nothing and every button refuses.\n\n\
                         The commanded drive gets you running in that case: the modulation \
                         plugin already inverts your measured Pockels curve to command a depth, \
                         so the number is calibrated — it just is not checked against the light. \
                         Runs recorded this way are tagged as such in their description file."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![SettingItem {
                        key: "depth_source".into(),
                        label: "Depth a source".into(),
                        tooltip: Some(
                            "Photodiode: use the depth the photodiode measures (accurate, needs \
                             the trigger markers). Modulation drive: use the depth the modulation \
                             plugin is commanding (works without the photodiode, but open loop — \
                             it is not verified against the light)."
                                .into(),
                        ),
                        kind: SettingKind::Enum {
                            variants: vec![
                                "photodiode (measured)".into(),
                                "modulation drive (commanded, open loop)".into(),
                            ],
                            default: self.depth_source.index() as usize,
                        },
                    }],
                },
                SettingsSection {
                    label: "Record".into(),
                    description: Some(
                        "Everything that writes files, in one place. Each measurement gets its \
                         own folder holding the camera file, the photodiode file, the sensor \
                         readout and a description file, all sharing one name.\n\n\
                         Four ways to run, all using the settings below and whatever the \
                         modulation plugin currently has armed for the axes they do not sweep:\n\
                         • Record once — one recording, exactly as the bench stands now.\n\
                         • Sweep a — a range of depths at the armed frequency.\n\
                         • Sweep f — a range of frequencies at the armed depth.\n\
                         • Sweep a × f — every depth at every frequency (the q_p(a, f) surface).\n\n\
                         You need an output folder, a connected photodiode, and the modulation \
                         plugin running the light. The measurement id is filled in for you if \
                         you leave it blank."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "output_folder".into(),
                            label: "Output folder".into(),
                            tooltip: Some(
                                "Where the recordings go. Each measurement gets its own subfolder \
                                 in here. Required."
                                    .into(),
                            ),
                            kind: SettingKind::Path {
                                dialog: PathDialogKind::Directory,
                                default: self.output_folder.clone(),
                            },
                        },
                        SettingItem {
                            key: "measurement_id".into(),
                            label: "Measurement id".into(),
                            tooltip: Some(
                                "Names the subfolder and every file in it. Use one id for all the \
                                 repeats that belong together. Optional — leave it blank and one \
                                 is generated when you press record."
                                    .into(),
                            ),
                            kind: SettingKind::Text {
                                default: self.measurement_id.clone(),
                            },
                        },
                        SettingItem {
                            key: "new_id".into(),
                            label: "New id".into(),
                            tooltip: Some(
                                "Put a fresh generated id in the field above, so the next \
                                 recording starts a new measurement folder."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "duration_s".into(),
                            label: "Duration (s)".into(),
                            tooltip: Some(
                                "How many seconds each recording lasts before it stops and saves \
                                 itself. Applies to every button here."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 1,
                                max: 3_600,
                                default: self.duration_s,
                            },
                        },
                        SettingItem {
                            key: "settle_s".into(),
                            label: "Settle time (s)".into(),
                            tooltip: Some(
                                "After changing the depth or the frequency, wait this long with \
                                 the reading holding steady before recording. Longer is safer if \
                                 your signal drifts. Ignored by Record once, which records what \
                                 is already there."
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
                            key: "min_a".into(),
                            label: "Depth axis: min a".into(),
                            tooltip: Some(
                                "Shallowest depth the a sweep records. Must be above 0 — a = 0 \
                                 is the background reference, which has its own button."
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
                            label: "Depth axis: max a".into(),
                            tooltip: Some("Deepest depth the a sweep records.".into()),
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: 10.0,
                                speed: 0.01,
                                default: self.max_a,
                            },
                        },
                        SettingItem {
                            key: "sweep_count".into(),
                            label: "Depth axis: points".into(),
                            tooltip: Some(
                                "How many depths the a sweep records, spread evenly from min a \
                                 to max a."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 2,
                                max: 64,
                                default: self.sweep_count,
                            },
                        },
                        SettingItem {
                            key: "a0_target".into(),
                            label: "Frequency axis: the depth a to hold".into(),
                            tooltip: Some(
                                "Sweep f records every frequency at this one depth, so a change \
                                 in the event count comes from the frequency and not from the \
                                 depth. Not used by Sweep a or Sweep a × f, which command each \
                                 depth themselves.\n\n\
                                 With the photodiode depth source the drive does not hold a depth \
                                 by itself as f changes, so it is re-found by measurement at \
                                 every frequency — see the a₀ section below."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: COMMANDED_A_MIN,
                                max: COMMANDED_A_MAX,
                                speed: 0.01,
                                default: self.a0_target,
                            },
                        },
                        SettingItem {
                            key: "min_f".into(),
                            label: "Frequency axis: min f (Hz)".into(),
                            tooltip: Some(
                                "Lowest frequency the f sweep records. It also decides whether \
                                 the run is measurable at all: the photodiode needs whole \
                                 modulation cycles inside its cache, so a very low value is \
                                 refused up front rather than mid-run."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.01,
                                max: 2_000.0,
                                speed: 0.1,
                                default: self.min_f,
                            },
                        },
                        SettingItem {
                            key: "max_f".into(),
                            label: "Frequency axis: max f (Hz)".into(),
                            tooltip: Some(
                                "Highest frequency to record. Check yourself that the sensor can \
                                 follow it."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.01,
                                max: 2_000.0,
                                speed: 1.0,
                                default: self.max_f,
                            },
                        },
                        SettingItem {
                            key: "freq_count".into(),
                            label: "Frequency axis: points".into(),
                            tooltip: Some(
                                "How many frequencies to record, spaced by decade rather than by \
                                 hertz — a Bode ladder is read per decade."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 1,
                                max: FREQ_SWEEP_MAX_POINTS as i64,
                                default: i64::from(self.freq_count),
                            },
                        },
                        SettingItem {
                            key: "freq_order".into(),
                            label: "Frequency axis: order".into(),
                            tooltip: Some(
                                "The order the frequencies are visited in. Anything but ascending \
                                 separates a real frequency effect from slow drift across the \
                                 block, because neighbouring points are no longer neighbours in \
                                 time."
                                    .into(),
                            ),
                            kind: SettingKind::Enum {
                                variants: vec![
                                    "ascending".into(),
                                    "alternating (low, high, low…)".into(),
                                    "shuffled".into(),
                                ],
                                default: self.freq_order.index() as usize,
                            },
                        },
                        SettingItem {
                            key: "freq_seed".into(),
                            label: "Frequency axis: shuffle seed".into(),
                            tooltip: Some(
                                "Makes the shuffled order repeatable: the same seed always gives \
                                 the same order. Ignored unless the order is shuffled."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 1,
                                max: 1_000_000,
                                default: self.freq_seed as i64,
                            },
                        },
                        SettingItem {
                            key: "freq_reference_every".into(),
                            label: "Frequency axis: repeat the lowest every N".into(),
                            tooltip: Some(
                                "Re-records the lowest frequency after every N points, so drift \
                                 across the block shows up as a disagreement between its \
                                 repeats. 0 = off."
                                    .into(),
                            ),
                            kind: SettingKind::I64Drag {
                                min: 0,
                                max: 10,
                                default: i64::from(self.freq_reference_every),
                            },
                        },
                        SettingItem {
                            key: "start_recording".into(),
                            label: "Record once".into(),
                            tooltip: Some(
                                "One recording with the light exactly as the modulation plugin \
                                 has it armed right now. Nothing is retargeted and nothing \
                                 settles first."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "start_sweep".into(),
                            label: "Sweep a".into(),
                            tooltip: Some(
                                "Records one file at each depth from min a to max a, at the \
                                 armed frequency. Settles on each depth before recording it."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "start_freq_sweep".into(),
                            label: "Sweep f".into(),
                            tooltip: Some(
                                "Records one file at each frequency from min f to max f, all at \
                                 the depth set above (\"the depth a to hold\"). With the \
                                 photodiode depth source that depth is re-found by measurement at \
                                 every frequency, because the drive does not hold it by itself as \
                                 f changes; with the commanded source it is simply commanded."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "start_freq_depth_sweep".into(),
                            label: "Sweep a × f".into(),
                            tooltip: Some(
                                "The whole surface: every depth in the a range, at every \
                                 frequency in the f range. That is (frequency points × depth \
                                 points) recordings — check both counts and the duration before \
                                 starting. Files are named …_f<f>Hz_pNN so the surface sorts by \
                                 frequency and then by depth."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "record_pilot".into(),
                            label: "Record pilot (freezes the ON/OFF windows)".into(),
                            tooltip: Some(
                                "A bright reference recording whose ON/OFF windows are reused by \
                                 every later recording in the same measurement, so the whole row \
                                 is scored consistently."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "record_background".into(),
                            label: "Record background (a ≈ 0 reference)".into(),
                            tooltip: Some(
                                "An unmodulated reference giving the false-response floor the \
                                 later points are measured above."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "stop_recording".into(),
                            label: "Stop".into(),
                            tooltip: Some(
                                "Stops whatever is running — a recording, a sweep, a ladder or a \
                                 protocol — at its next safe point, so the file in flight is \
                                 still finished and saved."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                    ],
                },
                SettingsSection {
                    label: "Protocol (run a survey from a file)".into(),
                    description: Some(
                        "The buttons above sweep one axis with the others left wherever they \
                         happen to be. A protocol names every axis for every recording instead, \
                         in a file that travels with the results.\n\n\
                         **CSV — one row per recording.** Columns: mean_u (the brightness / I_k \
                         axis), frequency_hz, depth_a, plus optional duration_s, settle_s, role \
                         (normal / pilot / background) and label. Columns are found by name so \
                         their order does not matter; blank lines and # comments are skipped, \
                         and a blank cell falls back to the default. Because each row carries \
                         its own duration, a 1 Hz point can record for 40 s and a 200 Hz point \
                         for 10 — and a file can start with its own background and pilot.\n\n\
                         **TOML — blocks and ranges.** [[block]] with a list or \
                         { min, max, points } range on each axis, expanded to their product. \
                         More compact for a dense regular sweep. Points run ū outermost, then f, \
                         then a, which settles the slow axis least often.\n\n\
                         Either way: all three axes are commanded at every point and the point \
                         waits for all three to be acknowledged before recording, so nothing is \
                         ever filed under parameters the file does not state. One modulation \
                         lease covers the whole run and your armed settings are handed back at \
                         the end. A point the drive cannot reach is skipped and named rather \
                         than stopping the survey.\n\n\
                         Commented examples ship with the plugin, at \
                         ~/.augur/plugins/stage-a-a1/protocols/ — example.csv and example.toml."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "protocol_path".into(),
                            label: "Protocol file".into(),
                            tooltip: Some(
                                ".csv (one row per recording) or .toml (blocks and ranges) — the \
                                 reader is chosen by the extension. Read and fully validated \
                                 when you press Run, so a bad value is reported with its line \
                                 number before the drive moves."
                                    .into(),
                            ),
                            kind: SettingKind::Path {
                                dialog: PathDialogKind::OpenFile,
                                default: self.protocol_path.clone(),
                            },
                        },
                        SettingItem {
                            key: "run_protocol".into(),
                            label: "Run the protocol".into(),
                            tooltip: Some(
                                "Reads and validates the file, then records every point in it. \
                                 The status line reports how many recordings and roughly how long \
                                 it will take before the first one starts, and tracks progress \
                                 after that.\n\n\
                                 Use Stop in the Record section to end it early — the recording \
                                 in flight is still finished and saved."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Advanced: hold one depth across frequencies (a₀)".into(),
                    description: Some(
                        "Only needed with the photodiode depth source. The drive does not \
                         deliver the same depth at every frequency by itself, so before \
                         recording a frequency point the plugin adjusts it until the photodiode \
                         measures a₀. That search is what Find a₀ does, and Sweep f runs it \
                         automatically at every frequency.\n\n\
                         With the commanded depth source there is nothing to search for and \
                         none of this is used. The depths found are saved per frequency in the \
                         output folder and survive a restart."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "a0_tolerance".into(),
                            label: "a₀ tolerance".into(),
                            tooltip: Some(
                                "How close the measured depth has to get to a₀ before the search \
                                 calls it done. Tighter takes longer and can fail on a noisy \
                                 reading."
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 0.002,
                                max: 0.5,
                                speed: 0.002,
                                default: self.a0_tolerance,
                            },
                        },
                        SettingItem {
                            key: "find_a0".into(),
                            label: "Find a₀ (at the armed frequency)".into(),
                            tooltip: Some(
                                "Searches for the drive depth that makes the photodiode measure \
                                 a₀ at the frequency currently armed, and remembers it."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "record_a0_point".into(),
                            label: "Record a₀ point".into(),
                            tooltip: Some(
                                "Records one file at the depth Find a₀ found for the armed \
                                 frequency."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: can_record,
                            },
                        },
                        SettingItem {
                            key: "clear_a0_locks".into(),
                            label: "Forget saved depths".into(),
                            tooltip: Some(
                                "Throws away every depth Find a₀ has found. Do this after \
                                 changing the illumination, the calibration, or a₀ itself — the \
                                 old depths no longer apply."
                                    .into(),
                            ),
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
            "depth_source" => Some(json!(self.depth_source.index())),
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
            "a0_target" => Some(json!(self.a0_target)),
            "a0_tolerance" => Some(json!(self.a0_tolerance)),
            "find_a0" => Some(self.press_find_a0.value()),
            "record_a0_point" => Some(self.press_record_a0.value()),
            "clear_a0_locks" => Some(self.press_clear_a0.value()),
            "min_f" => Some(json!(self.min_f)),
            "max_f" => Some(json!(self.max_f)),
            "freq_count" => Some(json!(self.freq_count)),
            "freq_order" => Some(json!(self.freq_order.index())),
            "freq_seed" => Some(json!(self.freq_seed)),
            "freq_reference_every" => Some(json!(self.freq_reference_every)),
            "start_freq_sweep" => Some(self.press_freq_sweep.value()),
            "start_freq_depth_sweep" => Some(self.press_freq_depth_sweep.value()),
            "protocol_path" => Some(json!(self.protocol_path)),
            "run_protocol" => Some(self.press_run_protocol.value()),
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
            "depth_source" => {
                self.depth_source =
                    DepthSource::from_index(value.as_u64().ok_or("depth_source must be an index")?);
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
            "start_freq_sweep" => {
                if self.press_freq_sweep.accept(&value) {
                    self.freq_sweep_pending = Some(FreqSweepMode::A0Point);
                }
            }
            "start_freq_depth_sweep" => {
                if self.press_freq_depth_sweep.accept(&value) {
                    self.freq_sweep_pending = Some(FreqSweepMode::DepthSweep);
                }
            }
            "stop_recording" => {
                if self.press_stop.accept(&value) {
                    self.request_stop();
                }
            }
            "live" => {
                self.live = value.as_bool().ok_or("live must be a boolean")?;
                if !self.live {
                    // Immediately, not on the next frame: with no camera
                    // running there is no next frame, and the operator who just
                    // switched this off is the one waiting for the plots to
                    // stop being slow.
                    self.drop_live_buffers();
                }
            }
            "analysis_window_ms" => {
                self.analysis_window_ms = value
                    .as_i64()
                    .ok_or("analysis_window_ms must be an integer")?
                    .clamp(1, 120_000);
            }
            "clear" => {
                if self.press_clear.accept(&value) {
                    self.drop_live_buffers();
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
            "a0_target" => {
                self.a0_target = value
                    .as_f64()
                    .ok_or("a0_target must be a number")?
                    .clamp(COMMANDED_A_MIN, COMMANDED_A_MAX);
            }
            "a0_tolerance" => {
                self.a0_tolerance = value
                    .as_f64()
                    .ok_or("a0_tolerance must be a number")?
                    .clamp(0.002, 0.5);
            }
            "find_a0" => {
                if self.press_find_a0.accept(&value) {
                    self.a0_lock_pending = true;
                }
            }
            "record_a0_point" => {
                if self.press_record_a0.accept(&value) {
                    self.a0_point_pending = true;
                }
            }
            "min_f" => {
                self.min_f = value.as_f64().ok_or("min_f must be a number")?.max(0.01);
            }
            "max_f" => {
                self.max_f = value.as_f64().ok_or("max_f must be a number")?.max(0.01);
            }
            "freq_count" => {
                self.freq_count = value
                    .as_u64()
                    .ok_or("freq_count must be an integer")?
                    .clamp(1, FREQ_SWEEP_MAX_POINTS as u64)
                    as u32;
            }
            "freq_order" => {
                self.freq_order =
                    FreqOrder::from_index(value.as_u64().ok_or("freq_order must be an index")?);
            }
            "freq_seed" => {
                self.freq_seed = value.as_u64().ok_or("freq_seed must be an integer")?.max(1);
            }
            "freq_reference_every" => {
                self.freq_reference_every = value
                    .as_u64()
                    .ok_or("freq_reference_every must be an integer")?
                    .min(10) as u32;
            }
            "protocol_path" => {
                self.protocol_path = value
                    .as_str()
                    .ok_or("protocol_path must be a string")?
                    .to_string();
            }
            "run_protocol" => {
                if self.press_run_protocol.accept(&value) {
                    self.protocol_pending = true;
                }
            }
            "clear_a0_locks" => {
                if self.press_clear_a0.accept(&value) {
                    self.a0_locks.clear();
                    self.message = match self.save_a0_locks() {
                        Ok(()) => "a₀ lock table cleared".into(),
                        Err(error) => error,
                    };
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
        if let Some(run) = self.protocol.as_ref() {
            entries.push(StatusEntry::Text(format!(
                "Protocol '{}': point {}/{} — {} recorded, {} skipped",
                run.plan.name,
                (run.index + 1).min(run.plan.points.len()),
                run.plan.points.len(),
                run.recorded,
                run.failed.len(),
            )));
            // The per-point message is overwritten within the tick that skips a
            // point, so the most recent reason lives here instead of scrolling
            // past unread.
            if let Some((index, reason)) = run.failed.last() {
                entries.push(StatusEntry::Text(format!(
                    "Last skipped point {}: {reason}",
                    index + 1
                )));
            }
        }
        if self.recording.is_active() {
            if let Some(remaining) = self.recording.remaining_s(now_unix_ms()) {
                entries.push(StatusEntry::Text(format!(
                    "{} — {remaining} s remaining",
                    self.recording.id
                )));
            }
        }
        if let Some(sweep) = &self.freq_sweep {
            let phase = match sweep.phase {
                FreqSweepPhase::AcquiringLease => "taking control of the drive",
                FreqSweepPhase::SettingFrequency => "changing the frequency",
                FreqSweepPhase::ConfirmingFrequency => "checking the frequency really changed",
                FreqSweepPhase::Locking => "finding the depth a₀",
                FreqSweepPhase::Recording => match sweep.mode {
                    // The inner sweep prints its own point-by-point line below,
                    // so this one only has to say which stage of the *ladder*
                    // the run is in.
                    FreqSweepMode::A0Point => "recording",
                    FreqSweepMode::DepthSweep => "recording the depth curve",
                },
            };
            let point = sweep.point();
            entries.push(StatusEntry::Text(format!(
                "Frequency ladder ({}) {}/{} at {}{} — {phase} ({} done, {} skipped)",
                sweep.mode.label(),
                sweep.index + 1,
                sweep.points.len(),
                frequency_label(sweep.frequency_hz()),
                if point.is_some_and(|point| point.is_reference) {
                    " (reference)"
                } else {
                    ""
                },
                sweep.recorded,
                sweep.failed.len(),
            )));
        }
        if let Some(sweep) = &self.sweep {
            let phase = match sweep.phase {
                SweepPhase::AcquiringLease => "taking control of the drive",
                SweepPhase::SettingDepth => "changing the depth",
                SweepPhase::Settling => "waiting for the depth to settle",
                SweepPhase::Recording => "recording",
            };
            let label = match sweep.kind {
                SweepKind::Amplitude => "Depth sweep",
                SweepKind::EventCount => "a₀ point",
            };
            entries.push(StatusEntry::Text(format!(
                "{label}: point {}/{}, asking for a = {:.3} to measure {:.3} ({phase})",
                sweep.index + 1,
                sweep.total(),
                sweep.commanded_a(),
                sweep.target_a()
            )));
        }
        if let Some(lock) = &self.a0_lock {
            let phase = match lock.phase {
                A0LockPhase::AcquiringLease => "taking control of the drive",
                A0LockPhase::SettingDepth => "setting the depth",
                A0LockPhase::Measuring => "measuring",
            };
            entries.push(StatusEntry::Text(format!(
                "Find a₀ at {}: try {}/{A0_LOCK_MAX_TRIALS}, asking for a = {:.3} to measure a₀ = \
                 {:.3} ({phase}, {} reading(s))",
                frequency_label(lock.frequency_hz),
                lock.trial,
                lock.commanded_a,
                lock.target_a,
                lock.samples.len()
            )));
        }
        // One estimate, for the run the operator started: a ladder's own line
        // already covers the inner sweep it is currently running, and a
        // protocol covers both, so three of these would be one fact three times.
        if let Some(line) = self.estimated_time_line(now_unix_ms()) {
            entries.push(StatusEntry::Text(line));
        }
        if !self.message.is_empty() {
            entries.push(StatusEntry::Text(self.message.clone()));
        }
        // Each fact appears once. The panel used to state a missing frequency on
        // three separate lines — the transient message, this line, and the a₀
        // readiness line — which reads as three problems instead of one.
        let no_frequency = self.frequency_hz().is_none();
        match self.period_us() {
            Some(period_us) => {
                let source = match self.frequency_source() {
                    "trigger" => "measured from the trigger",
                    _ => "as set in the modulation plugin",
                };
                entries.push(StatusEntry::Text(format!(
                    "Frequency: {:.3} Hz ({source}) — one cycle is {:.3} ms",
                    1_000_000.0 / period_us,
                    period_us / 1_000.0,
                )));
            }
            None => entries.push(StatusEntry::Text(format!(
                "Frequency: unknown. {}",
                capitalize_first(
                    &self
                        .frequency_blocker()
                        .unwrap_or_else(|| "no drive is armed".into())
                )
            ))),
        }
        // With Live analysis off nothing is ingested at all, so an event count
        // and a pixel count describe the switch rather than the bench. Say only
        // what is true.
        entries.push(StatusEntry::Text(if !self.live {
            "Camera: Live analysis is OFF — no events or triggers are being read".into()
        } else {
            let anchor = if self.is_marker_anchored() {
                format!("{} triggers seen", self.camera_markers_us.len())
            } else {
                "no trigger signal — check the cable from the Teensy to the camera".into()
            };
            format!(
                "Camera: {} events, {} usable pixels; {anchor}",
                self.camera_events.len(),
                self.valid_pixel_count().unwrap_or(0)
            )
        }));
        let depth_label = match self.depth_source {
            DepthSource::Photodiode => "Measured depth a",
            DepthSource::Commanded => "Commanded depth a (open loop, not measured)",
        };
        entries.push(StatusEntry::Text(match self.depth_a() {
            Some(a) => format!("{depth_label} = {a:.3}"),
            // Name the gate, not just its effect: every a₀ and sweep button
            // refuses on this value, so the panel has to say what to fix. A
            // colon, not a dash: the reason carries a dash of its own.
            None => format!(
                "{depth_label}: not available. {}",
                capitalize_first(
                    &self
                        .depth_a_blocker()
                        .unwrap_or_else(|| "no depth source has sent anything yet".into())
                )
            ),
        }));
        // Silent when the host reports nothing (replay, or a camera without a
        // monitoring block) rather than printing three dashes.
        if let Some(sensor) = self.sensor {
            let mut parts = Vec::new();
            if let Some(celsius) = sensor.temperature_c {
                parts.push(format!("{celsius:.1} °C"));
            }
            if let Some(dead_time_us) = sensor.pixel_dead_time_us {
                parts.push(format!("{dead_time_us:.1} µs dead time"));
            }
            if let Some(lux) = sensor.illumination_lux {
                parts.push(format!("{lux:.0} lx"));
            }
            if !parts.is_empty() {
                entries.push(StatusEntry::Text(format!(
                    "Sensor: {} (read {:.1} s ago; recorded with every run)",
                    parts.join(", "),
                    sensor.age_s
                )));
            }
            // The point values above ride the context bus and are always
            // there. The per-run *time series* is a separate host feature the
            // operator switches on, and it is off by default — so a survey
            // could record forty runs, keep the bench conditions of none of
            // them, and say nothing until the analysis. A1 cannot ask the host
            // whether it is on, but it can report that the last run produced
            // no readout, which is the same fact one recording later.
            if self.last_run_had_no_readout {
                entries.push(StatusEntry::Text(
                    "Sensor readout: the last run wrote none — tick \"Record sensor monitoring\" \
                     in the recording panel, or runs keep only the single reading above and not \
                     the series"
                        .into(),
                ));
            }
        }
        if let Some((on, off)) = self.latest_rolling() {
            entries.push(StatusEntry::Text(format!(
                "Events per pixel per half-cycle: {on:.4} bright, {off:.4} dark"
            )));
        }
        // Silent until there is something to report: at rest this line said
        // "0 point(s), no bright/dark windows yet", which is just the absence of
        // the two facts above it.
        let windows = self.current_windows();
        if !self.response_points.is_empty() || windows.is_some() {
            let source = if self.windows_are_frozen() {
                "from the pilot"
            } else {
                "found automatically"
            };
            let windows = windows.map_or_else(
                || "no bright/dark windows yet".into(),
                |(on, off)| {
                    format!(
                        "bright/dark windows {source} (bright {:.2}–{:.2}, dark {:.2}–{:.2} of a \
                         cycle)",
                        on.start, on.end, off.start, off.end
                    )
                },
            );
            entries.push(StatusEntry::Text(format!(
                "Response curve: {} point(s), {windows}",
                self.response_points.len()
            )));
        }
        if let Some((q0_on, q0_off)) = self.background_floor {
            entries.push(StatusEntry::Text(format!(
                "Background floor recorded: {q0_on:.3} bright, {q0_off:.3} dark"
            )));
        }
        // Open loop there is no lock table and nothing was searched for, so the
        // line says what will happen rather than reporting a saved-depth count
        // that is structurally always zero.
        entries.push(StatusEntry::Text(match (self.armed_a0(), no_frequency) {
            (Some(lock), _) if !self.depth_source.needs_a0_lock() => format!(
                "Ready to record at a₀ = {:.3}: the drive is commanded to a = {:.3} at {} — no \
                 search needed, press \"Record a₀ point\" or \"Record all frequencies\".",
                lock.target_a,
                lock.commanded_a,
                frequency_label(lock.frequency_hz),
            ),
            (Some(lock), _) => format!(
                "Ready to record at a₀ = {:.3}: at {} the drive is set to a = {:.3} and the \
                 photodiode measures {:.3}. {} depth(s) saved.",
                lock.target_a,
                frequency_label(lock.frequency_hz),
                lock.commanded_a,
                lock.measured_a,
                self.a0_locks.len()
            ),
            // Without a frequency nothing about a₀ can be judged yet, and the
            // Frequency line above already says what to fix. Point at it rather
            // than repeating it.
            (None, true) => "a₀ points: waiting for a frequency (above).".into(),
            (None, false) => format!(
                "Not ready to record an a₀ point — {}.",
                self.armed_a0_blocker()
                    .unwrap_or_else(|| "no depth is armed".into()),
            ),
        }));
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
                            column("a", "a (depth)"),
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
                HostDatasetDescriptor {
                    id: A0_LOCK_DATASET_ID.into(),
                    title: "A1 a₀ locks — commanded depth per frequency".into(),
                    kind: HostDatasetKind::TableV1(TableSchema {
                        columns: vec![
                            column("frequency", "Frequency"),
                            column("target_a", "a₀ (target)"),
                            column("commanded_a", "Commanded a"),
                            column("measured_a", "Observed a"),
                            column("depth_source", "a from"),
                            column("trials", "Trials"),
                            column("state", "State"),
                            column("locked_at", "Locked at (UTC)"),
                        ],
                        ..TableSchema::default()
                    }),
                    empty_message: "No a₀ lock yet — set a₀ and press Find a₀ per frequency".into(),
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
                HostViewDescriptor {
                    id: A0_LOCK_VIEW_ID.into(),
                    title: "A1 a₀ locks (commanded depth per frequency)".into(),
                    dataset_id: A0_LOCK_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::TableWindow,
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
            A0_LOCK_DATASET_ID => serde_json::to_vec(&self.a0_locks_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        matches!(
            dataset_id,
            STATUS_DATASET_ID | ROLLING_DATASET_ID | RESPONSE_CURVE_DATASET_ID | A0_LOCK_DATASET_ID
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
        PhotodiodeCalibrationV1, PhotodiodeStreamV1, RequestOutcomeV1, ResponseCommonV1, Sha256V1,
        StreamIntegrityV1, SynchronizationV1, UnsyncedReasonV1, CONTRACT_VERSION_V1,
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

    /// Mirrors the ordering of [`StageAA1Plugin::process_control`].
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
        // Same order as `process_control`: outermost supervisor first, so one
        // tick can carry a hand-off from the ladder down into a recording.
        plugin.drive_lease_heartbeat(sink);
        plugin.drive_protocol(sink);
        plugin.drive_freq_sweep(sink);
        plugin.drive_a0_lock(sink);
        plugin.drive_sweep(sink);
        plugin.drive_recording(sink);
    }

    /// Bare `Accepted` reply, as the modulation owner answers a lease or depth
    /// command (only the outcome variant is routed).
    fn accepted(request_id: u64) -> PluginServiceReply {
        PluginServiceReply {
            request_id,
            source_plugin_id: A1_PLUGIN_ID.into(),
            target_plugin_id: MODULATION_PLUGIN_ID.into(),
            service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
            outcome: PluginServiceOutcome::Accepted {
                payload: Value::Null,
            },
        }
    }

    /// A control inbox carrying just these service replies.
    fn inbox_with(service_replies: Vec<PluginServiceReply>) -> PluginControlInbox {
        PluginControlInbox {
            service_replies,
            ..PluginControlInbox::default()
        }
    }

    /// The modulation command inside a routed service request, if it is one.
    fn modulation_command(request: &PluginServiceRequest) -> Option<ModulationCommandV1> {
        serde_json::from_value::<ModulationRequestV1>(request.payload.clone())
            .ok()
            .map(|envelope| envelope.command)
    }

    fn rejected(request_id: u64, message: &str) -> PluginServiceReply {
        PluginServiceReply {
            request_id,
            source_plugin_id: A1_PLUGIN_ID.into(),
            target_plugin_id: MODULATION_PLUGIN_ID.into(),
            service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
            outcome: PluginServiceOutcome::Rejected {
                code: "invalid_command".into(),
                message: message.into(),
            },
        }
    }

    fn connected_modulation() -> ModulationStateV1 {
        ModulationStateV1 {
            contract_version: stage_a_plugin_contract::CONTRACT_VERSION_V1,
            owner_instance: OwnerInstanceId::new("mod-test"),
            service_revision: 1,
            connection: ConnectionStateV1::Connected {
                port_label: "mock".into(),
                firmware_version: Some("0.4.0".into()),
            },
            capabilities: Vec::new(),
            lease: None,
            controller_state: stage_a_plugin_contract::ControllerStateV1::Configured,
            active_run_id: None,
            requested: None,
            acknowledged: None,
            synchronization: stage_a_plugin_contract::SynchronizationV1::Unsynced {
                reason: stage_a_plugin_contract::UnsyncedReasonV1::NoLease,
                detail: None,
            },
            last_response: None,
            freshness: stage_a_plugin_contract::FreshnessV1 {
                observed_at_unix_ms: now_unix_ms(),
                valid_for_ms: 5_000,
            },
            calibration_id: Some("pockels-test".into()),
            optical_drive: None,
        }
    }

    /// A connected modulation owner running a calibrated optical drive at
    /// `depth_a`, published at `revision`.
    fn commanded_modulation(revision: u64, depth_a: f64) -> ModulationStateV1 {
        ModulationStateV1 {
            service_revision: revision,
            optical_drive: Some(stage_a_plugin_contract::OpticalDriveStateV1 {
                target: OpticalTargetV1::LogSine,
                requested_mean_u_milli: 500,
                resolved_mean_u_milli: 500,
                internal_u_milli: 500,
                depth_a_milli: (depth_a * 1_000.0).round() as u32,
                v_null_dac: 100,
                v_peak_dac: 800,
            }),
            ..connected_modulation()
        }
    }

    /// A photodiode snapshot reporting `measured_a`, published at `revision`.
    fn photodiode_measuring(revision: u64, measured_a: f64) -> PhotodiodeSummaryV1 {
        PhotodiodeSummaryV1 {
            contract_version: stage_a_plugin_contract::CONTRACT_VERSION_V1,
            owner_instance: OwnerInstanceId::new("pd-test"),
            service_revision: revision,
            connection: ConnectionStateV1::Connected {
                port_label: "mock".into(),
                firmware_version: None,
            },
            lease: None,
            active_run_id: None,
            requested_revision: None,
            acknowledged_revision: None,
            stream: stage_a_plugin_contract::PhotodiodeStreamV1 {
                stream_epoch: 1,
                sample_range: None,
                sample_rate_hz: Some(20_000),
                latest_adc_code: None,
                integrity: StreamIntegrityV1::default(),
                level: None,
            },
            active_recording: None,
            last_finalized_recording: None,
            data_dir: Some(std::env::temp_dir().display().to_string()),
            optical_summary: Some(stage_a_plugin_contract::PhotodiodeOpticalSummaryV1 {
                run_id: RunId::new("pd-run"),
                calibration: stage_a_plugin_contract::PhotodiodeCalibrationV1 {
                    adc_calibration_id: "adc".into(),
                    dark_id: "dark".into(),
                    anchor_id: "anchor".into(),
                    dark_volts: 0.0,
                    total_power_volts: 1.0,
                },
                measured_log_contrast: measured_a,
                log_contrast_stddev: None,
                excitation_min_volts: 0.1,
                excitation_max_volts: 0.9,
                excitation_headroom_volts: 0.1,
                low_clip_fraction: 0.0,
                high_clip_fraction: 0.0,
                measured_frequency_hz: None,
                fundamental_phase_rad: None,
                total_harmonic_distortion: None,
                // A short window, so the lock's dwell and sample spacing stay
                // in the millisecond range the tests tick at.
                window_seconds: Some(0.001),
                covered_cycles: Some(8.0),
            }),
            optical_unavailable: None,
            synchronization: stage_a_plugin_contract::SynchronizationV1::Unsynced {
                reason: stage_a_plugin_contract::UnsyncedReasonV1::NoLease,
                detail: None,
            },
            last_response: None,
            freshness: stage_a_plugin_contract::FreshnessV1 {
                observed_at_unix_ms: now_unix_ms(),
                valid_for_ms: 5_000,
            },
        }
    }

    /// The depth carried by the newest `SetOpticalDepth` the plugin emitted.
    fn last_commanded_depth(sink: &ControlSink) -> Option<f64> {
        sink.services.iter().rev().find_map(|request| {
            let envelope: ModulationRequestV1 =
                serde_json::from_value(request.payload.clone()).ok()?;
            match envelope.command {
                ModulationCommandV1::SetOpticalDepth { depth_a_milli } => {
                    Some(f64::from(depth_a_milli) / 1_000.0)
                }
                _ => None,
            }
        })
    }

    /// A unique scratch directory for a test's lock table and sidecars.
    fn temp_folder(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("a1-{tag}-{}", now_unix_ms()))
    }

    /// A plugin wired to a connected drive at 1 kHz (marker-anchored) whose
    /// photodiode reports a bench that delivers `gain ×` the commanded depth.
    fn plugin_locking(gain: f64, folder: &Path) -> StageAA1Plugin {
        let mut plugin = plugin_with_markers();
        plugin.modulation = Some(connected_modulation());
        plugin.photodiode = Some(photodiode_measuring(1, gain));
        plugin.output_folder = folder.display().to_string();
        plugin.measurement_id = "A1-ec".into();
        plugin.settle_s = 0.0;
        plugin.a0_tolerance = 0.02;
        plugin
    }

    /// Answers the lock's outstanding lease/depth request and publishes the
    /// photodiode readings the commanded depth produces, until the lock ends.
    fn run_lock_to_completion(
        plugin: &mut StageAA1Plugin,
        sink: &mut ControlSink,
        gain: f64,
        max_ticks: usize,
    ) -> usize {
        let mut revision = 1;
        // The first tick consumes the latched press and starts the lock.
        control_tick(plugin, PluginControlInbox::default(), sink);
        for tick in 0..max_ticks {
            if plugin.a0_lock.is_none() {
                return tick + 1;
            }
            let (lease_req, depth_req, granted, applied) = {
                let lock = plugin.a0_lock.as_ref().expect("lock");
                (
                    lock.lease_req,
                    lock.depth_req,
                    lock.lease_granted,
                    lock.depth_applied,
                )
            };
            let mut replies = Vec::new();
            if !granted {
                replies.push(accepted(lease_req));
            } else if !applied && depth_req != 0 {
                replies.push(accepted(depth_req));
            } else {
                // Measuring: publish what the bench delivers for the commanded
                // depth as a fresh summary.
                let commanded = plugin.a0_lock.as_ref().expect("lock").commanded_a;
                revision += 1;
                plugin.photodiode = Some(photodiode_measuring(revision, commanded * gain));
                // The lock spaces its readings by a fraction of the photodiode's
                // estimator window (1 ms in these fixtures), so a tick loop that
                // never advances the wall clock would collect exactly one.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            control_tick(
                plugin,
                PluginControlInbox {
                    service_replies: replies,
                    ..PluginControlInbox::default()
                },
                sink,
            );
        }
        max_ticks
    }

    /// The bench that motivated the commanded source: the photodiode streams
    /// fine but withholds `a` because no phase-0 markers ever arrive, so no
    /// window can be proven to cover whole modulation cycles.
    fn photodiode_without_triggers() -> PhotodiodeSummaryV1 {
        PhotodiodeSummaryV1 {
            optical_summary: None,
            optical_unavailable: Some(
                "no stretch of samples covers two whole modulation cycles between triggers \
                 (0 trigger(s) in the last 3446784 samples) — lower the frequency, or raise the \
                 photodiode cache length"
                    .into(),
            ),
            ..photodiode_measuring(1, 0.5)
        }
    }

    #[test]
    fn a_withheld_photodiode_depth_names_the_commanded_fallback() {
        // The photodiode's own reason has to survive verbatim — it is the only
        // side that knows which gate refused — but a bench with no trigger
        // markers at all cannot act on it, so the way past must be on the same
        // line as the diagnosis.
        let mut plugin = plugin_with_markers();
        plugin.photodiode = Some(photodiode_without_triggers());

        let blocker = plugin.depth_a_blocker().expect("a withheld a has a reason");
        assert!(
            blocker.contains("two whole modulation cycles"),
            "the owner's own words must survive: {blocker}"
        );
        assert!(
            blocker.contains("Depth a source"),
            "and must name the setting that gets past it: {blocker}"
        );
    }

    #[test]
    fn the_commanded_source_reports_a_depth_with_no_photodiode_at_all() {
        let mut plugin = plugin_with_markers();
        plugin.photodiode = None;
        plugin.modulation = Some(commanded_modulation(1, 0.75));
        plugin.depth_source = DepthSource::Commanded;

        assert_eq!(plugin.depth_a(), Some(0.75));
        assert!(
            plugin.depth_a_blocker().is_none(),
            "the commanded drive is a depth source in its own right"
        );
    }

    #[test]
    fn the_commanded_source_refuses_a_drive_that_is_not_calibrated() {
        // Without a calibrated optical drive the owner publishes no inversion,
        // and a "commanded a" would be a DAC number wearing a physical name.
        let mut plugin = plugin_with_markers();
        plugin.photodiode = None;
        plugin.modulation = Some(connected_modulation());
        plugin.depth_source = DepthSource::Commanded;

        assert!(plugin.depth_a().is_none());
        let blocker = plugin.depth_a_blocker().expect("a reason");
        assert!(
            blocker.contains("calibration") && blocker.contains("OPTICAL_LOG_SINE"),
            "{blocker}"
        );
    }

    /// An acknowledged periodic drive at `hz`, i.e. what the modulation owner
    /// publishes once it has really applied a commanded frequency.
    fn acknowledged_sine(hz: f64) -> stage_a_plugin_contract::ModulationTargetV1 {
        stage_a_plugin_contract::ModulationTargetV1 {
            revision: SemanticRevision(1),
            waveform: Some(WaveformV1::Periodic {
                waveform: stage_a_plugin_contract::PeriodicWaveformV1::Sine,
                min_dac: 100,
                max_dac: 900,
                frequency_millihz: (hz * 1_000.0).round() as u64,
            }),
            a1_configuration: None,
            acquisition_running: true,
            board_dac_code: None,
            firmware_configuration_revision: None,
        }
    }

    /// A plugin ready to record a₀ points open loop: calibrated commanded
    /// drive, no photodiode, and — deliberately — no camera trigger markers.
    fn plugin_commanded_a0(folder: &Path) -> StageAA1Plugin {
        StageAA1Plugin {
            frame_width: 10,
            frame_height: 1,
            depth_source: DepthSource::Commanded,
            photodiode: Some(fresh_photodiode_summary()),
            modulation: Some(commanded_modulation(1, 0.5)),
            output_folder: folder.display().to_string(),
            measurement_id: "A1-cmd".into(),
            settle_s: 0.0,
            a0_target: 0.5,
            a0_tolerance: 0.02,
            ..StageAA1Plugin::default()
        }
    }

    #[test]
    fn find_a0_refuses_to_search_for_a_depth_it_is_commanding() {
        // The search commands a₀, reads back a₀ and stops — one identical row
        // per frequency and nothing learned. Refuse and say so rather than
        // spending a lease and a trial to arrive where the operator already is.
        let dir = temp_folder("commanded-find");
        let mut plugin = plugin_commanded_a0(&dir);
        let mut sink = ControlSink::default();

        plugin.set_setting("find_a0", json!(true)).expect("press");
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert!(plugin.a0_lock.is_none(), "no search may start");
        assert!(
            sink.services.is_empty(),
            "and no lease may be taken for it: {:?}",
            sink.services.len()
        );
        assert!(
            plugin.message.contains("No search needed"),
            "{}",
            plugin.message
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_commanded_a0_point_is_armed_without_any_lock() {
        // `Record a₀ point` and the ladder both ask one question — what depth
        // is armed here — and open loop the answer needs no stored table.
        let dir = temp_folder("commanded-armed");
        let mut plugin = plugin_commanded_a0(&dir);
        // 1 kHz from the modulation owner's acknowledged drive; no markers.
        plugin.modulation = Some(ModulationStateV1 {
            acknowledged: Some(acknowledged_sine(1_000.0)),
            ..commanded_modulation(1, 0.5)
        });

        assert!(
            !plugin.is_marker_anchored(),
            "no camera trigger in this test"
        );
        assert!(
            plugin.armed_a0_blocker().is_none(),
            "{:?}",
            plugin.armed_a0_blocker()
        );
        let armed = plugin.armed_a0().expect("an armed depth");
        assert!((armed.commanded_a - 0.5).abs() < 1e-9);
        assert!((armed.frequency_hz - 1_000.0).abs() < 1e-6);
        assert_eq!(armed.trials, 0, "no search happened, and it must say so");
        assert_eq!(armed.depth_source, DepthSource::Commanded);
        assert!(
            plugin.a0_locks.is_empty(),
            "and nothing was written to the lock table"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_commanded_ladder_records_every_point_without_a_lock_or_a_trigger() {
        // The whole simplification in one test: no photodiode `a`, no camera
        // markers, no Find a₀ — the ladder still leases once, walks the
        // frequencies confirming each against the modulation owner, and records
        // a point at a₀ at every one of them.
        let dir = temp_folder("commanded-ladder");
        let mut plugin = plugin_commanded_a0(&dir);
        plugin.min_f = 10.0;
        plugin.max_f = 1_000.0;
        plugin.freq_count = 3;
        plugin.freq_order = FreqOrder::Ascending;
        plugin.duration_s = 1;

        let mut sink = ControlSink::default();
        plugin
            .set_setting("start_freq_sweep", json!(true))
            .expect("press");
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert!(
            plugin.freq_sweep.is_some(),
            "the ladder must start with no trigger: {}",
            plugin.message
        );

        let mut recorded = Vec::new();
        for _ in 0..400 {
            let Some((phase, target_hz, lease_req, freq_req, granted, applied)) =
                plugin.freq_sweep.as_ref().map(|sweep| {
                    (
                        sweep.phase,
                        sweep.frequency_hz(),
                        sweep.lease_req,
                        sweep.freq_req,
                        sweep.lease_granted,
                        sweep.freq_applied,
                    )
                })
            else {
                break;
            };
            let mut replies = Vec::new();
            match phase {
                FreqSweepPhase::AcquiringLease if !granted => replies.push(accepted(lease_req)),
                FreqSweepPhase::SettingFrequency if !applied && freq_req != 0 => {
                    // The owner acknowledges the new frequency. In commanded
                    // mode that ack — not the camera trigger — is what confirms
                    // the point, so nothing here ever writes a marker.
                    plugin.modulation = Some(ModulationStateV1 {
                        acknowledged: Some(acknowledged_sine(target_hz)),
                        ..commanded_modulation(2, 0.5)
                    });
                    replies.push(accepted(freq_req));
                }
                FreqSweepPhase::Locking => {
                    panic!("the commanded ladder must never enter the search phase")
                }
                FreqSweepPhase::Recording => {
                    if let Some(sweep) = plugin.sweep.as_ref() {
                        if !sweep.depth_applied && sweep.depth_req != 0 {
                            replies.push(accepted(sweep.depth_req));
                        }
                    }
                    // Short-circuit the recording coordinator once the point has
                    // started: this test is about the ladder, and the
                    // coordinator has tests of its own.
                    if plugin.recording.is_active()
                        && plugin
                            .sweep
                            .as_ref()
                            .is_some_and(|sweep| sweep.point_started)
                    {
                        plugin.recording = Recording::idle();
                        plugin.recording_completed_ok = true;
                        recorded.push(target_hz);
                    }
                }
                _ => {}
            }
            control_tick(
                &mut plugin,
                PluginControlInbox {
                    service_replies: replies,
                    ..PluginControlInbox::default()
                },
                &mut sink,
            );
        }

        assert!(
            plugin.freq_sweep.is_none(),
            "the ladder must finish: {}",
            plugin.message
        );
        assert_eq!(
            recorded.len(),
            3,
            "every planned point records: {recorded:?}"
        );
        assert!(
            plugin.message.contains("3/3 points recorded"),
            "{}",
            plugin.message
        );
        assert!(
            plugin.a0_locks.is_empty(),
            "and the lock table stays empty — nothing was searched for"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_nested_sweep_records_every_depth_at_every_frequency_on_one_lease() {
        // The q_p(a, f) surface in one press: the outer ladder walks the
        // frequencies, and each rung runs the *whole* inner depth sweep. No a₀
        // and no search are involved at any point.
        let dir = temp_folder("nested-sweep");
        let mut plugin = plugin_commanded_a0(&dir);
        plugin.min_f = 10.0;
        plugin.max_f = 100.0;
        plugin.freq_count = 2;
        plugin.freq_order = FreqOrder::Ascending;
        plugin.min_a = 0.5;
        plugin.max_a = 1.5;
        plugin.sweep_count = 3;
        plugin.duration_s = 1;

        let mut sink = ControlSink::default();
        plugin
            .set_setting("start_freq_depth_sweep", json!(true))
            .expect("press");
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert!(
            plugin.freq_sweep.is_some(),
            "the nested sweep must start: {}",
            plugin.message
        );

        // (frequency, depth index) of every recording that actually started.
        let mut recorded: Vec<(f64, usize)> = Vec::new();
        for _ in 0..600 {
            let Some((phase, mode, target_hz, lease_req, freq_req, granted, applied)) =
                plugin.freq_sweep.as_ref().map(|sweep| {
                    (
                        sweep.phase,
                        sweep.mode,
                        sweep.frequency_hz(),
                        sweep.lease_req,
                        sweep.freq_req,
                        sweep.lease_granted,
                        sweep.freq_applied,
                    )
                })
            else {
                break;
            };
            assert_eq!(mode, FreqSweepMode::DepthSweep);
            assert_ne!(
                phase,
                FreqSweepPhase::Locking,
                "a depth sweep never needs an a₀ search"
            );
            let mut replies = Vec::new();
            match phase {
                FreqSweepPhase::AcquiringLease if !granted => replies.push(accepted(lease_req)),
                FreqSweepPhase::SettingFrequency if !applied && freq_req != 0 => {
                    plugin.modulation = Some(ModulationStateV1 {
                        acknowledged: Some(acknowledged_sine(target_hz)),
                        ..commanded_modulation(2, 0.5)
                    });
                    replies.push(accepted(freq_req));
                }
                FreqSweepPhase::Recording => {
                    if let Some(sweep) = plugin.sweep.as_ref() {
                        let (depth_req, depth_applied, index, commanded) = (
                            sweep.depth_req,
                            sweep.depth_applied,
                            sweep.index,
                            sweep.commanded_a(),
                        );
                        if !depth_applied && depth_req != 0 {
                            // The owner applies the depth this point asked for,
                            // which is what the settle check then reads back.
                            plugin.modulation = Some(ModulationStateV1 {
                                acknowledged: Some(acknowledged_sine(target_hz)),
                                ..commanded_modulation(3, commanded)
                            });
                            replies.push(accepted(depth_req));
                        }
                        if plugin.recording.is_active() && sweep.point_started {
                            plugin.recording = Recording::idle();
                            plugin.recording_completed_ok = true;
                            recorded.push((target_hz, index));
                        }
                    }
                }
                _ => {}
            }
            control_tick(
                &mut plugin,
                PluginControlInbox {
                    service_replies: replies,
                    ..PluginControlInbox::default()
                },
                &mut sink,
            );
        }

        assert!(
            plugin.freq_sweep.is_none(),
            "the nested sweep must finish: {}",
            plugin.message
        );
        assert_eq!(
            recorded.len(),
            6,
            "2 frequencies × 3 depths: {recorded:?} — {}",
            plugin.message
        );
        // Every frequency saw its whole curve, in depth order.
        assert_eq!(
            recorded.iter().map(|(_, index)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2, 0, 1, 2]
        );
        assert!((recorded[0].0 - 10.0).abs() < 1e-6, "{recorded:?}");
        assert!((recorded[3].0 - 100.0).abs() < 1e-6, "{recorded:?}");
        assert!(
            plugin
                .message
                .contains("2/2 frequencies × 3 depths recorded"),
            "{}",
            plugin.message
        );

        // One lease for the whole block: the inner sweeps inherit it, so the
        // operator's drive cannot move between rungs.
        let leases = sink
            .services
            .iter()
            .filter_map(|request| {
                serde_json::from_value::<ModulationRequestV1>(request.payload.clone()).ok()
            })
            .filter(|envelope| matches!(envelope.command, ModulationCommandV1::AcquireLease { .. }))
            .count();
        assert_eq!(
            leases, 1,
            "exactly one lease acquisition for the whole block"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_nested_sweep_point_is_named_by_frequency_and_depth() {
        // `_p03` alone repeats at every rung, so the surface would collide
        // inside one measurement id.
        let dir = temp_folder("nested-name");
        let mut plugin = plugin_commanded_a0(&dir);
        plugin.freq_sweep = Some(FreqSweep {
            phase: FreqSweepPhase::Recording,
            mode: FreqSweepMode::DepthSweep,
            points: vec![FreqSweepPoint {
                frequency_hz: 50.0,
                is_reference: false,
            }],
            index: 0,
            lease_id: LeaseId::new("nested"),
            lease_granted: true,
            lease_req: 0,
            freq_req: 0,
            freq_applied: true,
            confirm_deadline_ms: 0,
            skip_reason: None,
            failed: Vec::new(),
            recorded: 0,
            order: FreqOrder::Ascending,
            seed: 1,
            last_activity_ms: 0,
            stop_requested: false,
            eta: Eta::new(0),
        });
        plugin.sweep = Some(Sweep {
            phase: SweepPhase::Recording,
            kind: SweepKind::Amplitude,
            points: vec![
                SweepPoint {
                    commanded_a: 1.0,
                    expected_a: 1.0,
                };
                3
            ],
            lock: None,
            index: 2,
            lease_id: LeaseId::new("nested"),
            lease_granted: true,
            lease_req: 0,
            owns_lease: false,
            depth_req: 0,
            depth_applied: true,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: false,
            completed_ok: false,
            last_activity_ms: 0,
            stop_requested: false,
            eta: Eta::new(0),
        });

        let mut sink = ControlSink::default();
        plugin.begin_recording(&mut sink, RecRole::Normal);
        let stem = plugin.recording.stem.clone();
        assert!(stem.ends_with("_f50Hz_p03"), "stem: {stem}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_run_records_the_bench_conditions_the_sensor_measured() {
        let mut plugin = plugin_with_markers();
        plugin.sensor = Some(SensorMonitoringV1 {
            pixel_dead_time_us: Some(102.5),
            illumination_lux: Some(742.0),
            temperature_c: Some(41.25),
            bias_codes: Some(augur_plugin_api::SensorBiasReadbackV1 {
                current: augur_plugin_api::SensorBiasCodesV1 {
                    diff_on: 115,
                    diff_off: 52,
                    fo: 55,
                    hpf: 0,
                    refr: 20,
                },
                factory_default: augur_plugin_api::SensorBiasCodesV1::default(),
            }),
            age_s: 0.25,
        });
        // Frozen at start: the die warms and the room lights move, so what
        // belongs to a run is what held when it began.
        plugin.sensor_at_start = plugin.sensor;
        plugin.sensor = Some(SensorMonitoringV1 {
            temperature_c: Some(99.0),
            ..plugin.sensor.expect("set above")
        });

        let meta = plugin.recording_metadata();
        assert_eq!(
            meta.get("sensor_temperature_c").map(String::as_str),
            Some("41.25"),
            "the start snapshot wins over the drifted live one"
        );
        assert_eq!(
            meta.get("sensor_pixel_dead_time_us").map(String::as_str),
            Some("102.500")
        );
        assert_eq!(
            meta.get("sensor_illumination_lux").map(String::as_str),
            Some("742.000")
        );
        assert_eq!(
            meta.get("sensor_reading_age_s").map(String::as_str),
            Some("0.250")
        );

        plugin.recording.id = "A1-sensor".into();
        plugin.recording.stem = "A1-sensor_20260731-000000".into();
        plugin.recording.folder = std::env::temp_dir().display().to_string();
        plugin.recording.duration_s = 5;
        let doc = plugin.write_sidecar().expect("sidecar path");
        let text = std::fs::read_to_string(&doc).expect("read sidecar");
        assert!(text.contains("[sensor]"), "{text}");
        assert!(text.contains("temperature_c = 41.25"), "{text}");
        assert!(text.contains("pixel_dead_time_us = 102.5"), "{text}");
        assert!(text.contains("illumination_lux = 742.0"), "{text}");
        assert!(text.contains("bias_refr = 20"), "{text}");
        let _ = std::fs::remove_file(&doc);
    }

    #[test]
    fn a_quantity_the_sensor_cannot_report_is_absent_rather_than_zero() {
        // Replay, decoded imports and cameras without a monitoring block have
        // no sensor to ask. A 0 °C die or 0 lx scene would be read downstream
        // as a measurement.
        let mut plugin = plugin_with_markers();
        assert!(plugin.recorded_sensor().is_none());
        let meta = plugin.recording_metadata();
        assert!(!meta.contains_key("sensor_temperature_c"));
        assert!(!meta.contains_key("sensor_illumination_lux"));

        // A sensor that reports only some of the three is equally honest.
        plugin.sensor_at_start = Some(SensorMonitoringV1 {
            temperature_c: Some(38.0),
            age_s: 0.1,
            ..SensorMonitoringV1::default()
        });
        let meta = plugin.recording_metadata();
        assert_eq!(
            meta.get("sensor_temperature_c").map(String::as_str),
            Some("38.00")
        );
        assert!(!meta.contains_key("sensor_illumination_lux"));
        assert!(!meta.contains_key("sensor_pixel_dead_time_us"));
    }

    #[test]
    fn a_run_records_which_source_its_depth_came_from() {
        let mut plugin = plugin_with_markers();
        plugin.photodiode = None;
        plugin.modulation = Some(commanded_modulation(1, 0.75));
        plugin.depth_source = DepthSource::Commanded;

        let meta = plugin.recording_metadata();
        assert_eq!(
            meta.get("depth_a_source").map(String::as_str),
            Some("modulation_commanded")
        );
        assert_eq!(meta.get("depth_a").map(String::as_str), Some("0.750000"));
        assert!(
            !meta.contains_key("measured_a"),
            "`measured_a` names a measurement, and there was none"
        );

        plugin.depth_source = DepthSource::Photodiode;
        plugin.photodiode = Some(photodiode_measuring(1, 0.42));
        let meta = plugin.recording_metadata();
        assert_eq!(
            meta.get("depth_a_source").map(String::as_str),
            Some("photodiode_measured")
        );
        assert_eq!(meta.get("measured_a").map(String::as_str), Some("0.420000"));
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
            optical_unavailable: None,
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

    fn fresh_photodiode_summary() -> PhotodiodeSummaryV1 {
        PhotodiodeSummaryV1 {
            contract_version: CONTRACT_VERSION_V1,
            owner_instance: OwnerInstanceId::new("pd-test"),
            service_revision: 1,
            connection: ConnectionStateV1::Connected {
                port_label: "mock".into(),
                firmware_version: Some("test".into()),
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
            active_recording: None,
            last_finalized_recording: None,
            data_dir: Some(std::env::temp_dir().display().to_string()),
            optical_summary: Some(PhotodiodeOpticalSummaryV1 {
                run_id: RunId::from("test-run"),
                calibration: PhotodiodeCalibrationV1 {
                    adc_calibration_id: "adc-test".into(),
                    dark_id: "dark-test".into(),
                    anchor_id: "itot-test".into(),
                    dark_volts: 0.05,
                    total_power_volts: 3.0,
                },
                measured_log_contrast: 1.0,
                log_contrast_stddev: None,
                excitation_min_volts: 0.8,
                excitation_max_volts: 0.8 * std::f64::consts::E,
                excitation_headroom_volts: 0.8,
                low_clip_fraction: 0.0,
                high_clip_fraction: 0.0,
                measured_frequency_hz: Some(1_000.0),
                fundamental_phase_rad: None,
                total_harmonic_distortion: None,
                window_seconds: Some(0.008),
                covered_cycles: Some(8.0),
            }),
            optical_unavailable: None,
            synchronization: SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::NoLease,
                detail: None,
            },
            last_response: None,
            freshness: FreshnessV1 {
                observed_at_unix_ms: now_unix_ms(),
                valid_for_ms: 60_000,
            },
        }
    }

    /// A plugin whose period comes from marker spacing (no fallback frequency).
    fn plugin_with_markers() -> StageAA1Plugin {
        StageAA1Plugin {
            // 10 x 1 sensor, no host ROI => valid_pixel_count() == 10.
            frame_width: 10,
            frame_height: 1,
            camera_markers_us: vec![0, 1_000, 2_000, 3_000],
            photodiode: Some(fresh_photodiode_summary()),
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
        plugin.photodiode = None;
        assert!(plugin.depth_a().is_none());
        assert!(plugin.record_response_point().is_err());
    }

    #[test]
    fn the_rolling_response_is_normalised_over_the_roi_not_the_sensor() {
        // `q_p` counts ROI-minus-masked pixels; the rolling half-period rate is
        // plotted next to it and must agree. Normalising by the whole sensor
        // under-reported S_p by the ROI/frame ratio *and* counted events from
        // outside the ROI.
        let mut plugin = StageAA1Plugin {
            frame_width: 10,
            frame_height: 10,
            camera_markers_us: vec![0, 1_000, 2_000, 3_000],
            ..StageAA1Plugin::default()
        };
        plugin.host_roi = Some(RoiV1 {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        });
        let event = |x: u16, y: u16, timestamp_us: u64| CameraEvent {
            timestamp_us,
            x,
            y,
            polarity: Polarity::On,
        };
        // Two ON events inside the 2x2 ROI, five well outside it, all inside
        // the trailing half period the status readout samples.
        plugin.camera_events.push(event(0, 0, 2_800));
        plugin.camera_events.push(event(1, 1, 2_850));
        for x in 5..10_u16 {
            plugin.camera_events.push(event(x, 9, 2_900));
        }

        let (on_rate, _) = plugin.latest_rolling().expect("rolling value");
        assert!(
            (on_rate - 0.5).abs() < 1e-9,
            "expected 2 ROI events over 4 valid pixels, got {on_rate}"
        );
    }

    #[test]
    fn the_fold_cache_tracks_its_inputs() {
        let mut plugin = plugin_with_markers();
        for cycle in 0..8 {
            plugin.camera_events.push(on(cycle * 1_000 + 200));
        }
        let first = plugin.current_fold().expect("fold");
        // Repeated calls within a repaint must be identical, not merely equal
        // to a fresh recomputation.
        assert_eq!(plugin.current_fold().as_ref(), Some(&first));
        assert_eq!(plugin.compute_fold().as_ref(), Some(&first));

        // ...and adding an event inside the marker span must invalidate it.
        plugin.camera_events.push(on(2_500));
        plugin.camera_events.sort_by_key(|event| event.timestamp_us);
        let second = plugin.current_fold().expect("fold");
        assert_eq!(second.events.len(), first.events.len() + 1);

        // A changed ROI also invalidates, even at identical event counts.
        plugin.host_roi = Some(RoiV1 {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
        });
        let third = plugin.current_fold().expect("fold");
        assert_eq!(third.events.len(), second.events.len());
        plugin.host_roi = Some(RoiV1 {
            x: 5,
            y: 0,
            width: 1,
            height: 1,
        });
        let fourth = plugin.current_fold().expect("fold");
        assert!(
            fourth.events.is_empty(),
            "ROI moved off the events but the cache served a stale fold"
        );
    }

    #[test]
    fn a_failed_pilot_freeze_clears_stale_windows() {
        // `scan_measurement_folder` may have loaded windows from an earlier
        // pilot for this measurement. If the freeze then fails, the sidecar
        // must not record those as if they had come from this run.
        let mut plugin = plugin_with_markers();
        plugin.pilot_windows = Some((
            PhaseWindow {
                start: 0.0,
                end: 0.2,
            },
            PhaseWindow {
                start: 0.5,
                end: 0.7,
            },
        ));
        // No events => the fold carries no signal => the freeze cannot pick
        // windows and must not leave the loaded ones in place.
        assert!(plugin.camera_events.is_empty());
        plugin.freeze_pilot_windows();
        assert!(
            plugin.pilot_windows.is_none(),
            "stale pilot windows survived a failed freeze"
        );
        assert!(!plugin.windows_are_frozen());
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
            photodiode: Some(fresh_photodiode_summary()),
            duration_s: 1,
            pending_role: Some(RecRole::Normal),
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
            frame_width: 10,
            frame_height: 1,
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
        assert!((points[0].expected_a - 0.5).abs() < 1e-12);
        assert!((points[4].expected_a - 2.5).abs() < 1e-12);
        assert!((points[2].expected_a - 1.5).abs() < 1e-12);
        // The amplitude sweep trusts the calibration: it commands what it expects.
        assert!(points
            .iter()
            .all(|point| point.commanded_a == point.expected_a));
    }

    #[test]
    fn sweep_point_recordings_carry_the_requested_a_in_the_sidecar() {
        let mut plugin = plugin_with_markers();
        plugin.min_a = 0.5;
        plugin.max_a = 1.5;
        plugin.sweep_count = 3;
        plugin.sweep = Some(Sweep {
            phase: SweepPhase::Recording,
            kind: SweepKind::Amplitude,
            points: plugin.sweep_points(),
            lock: None,
            index: 1,
            lease_id: LeaseId::new("a1-sweep-test"),
            lease_granted: true,
            lease_req: 0,
            owns_lease: true,
            depth_req: 0,
            depth_applied: true,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: true,
            completed_ok: false,
            last_activity_ms: 0,
            stop_requested: false,
            eta: Eta::new(0),
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
    fn a0_lock_trims_the_commanded_depth_until_the_photodiode_measures_a0() {
        // A bench that delivers 60 % of the commanded depth (drive roll-off):
        // commanding a₀ directly would record a = 0.30 instead of 0.50.
        let folder = temp_folder("lock");
        let mut plugin = plugin_locking(0.6, &folder);
        plugin.a0_target = 0.5;
        plugin.a0_lock_pending = true;
        let mut sink = ControlSink::default();

        let ticks = run_lock_to_completion(&mut plugin, &mut sink, 0.6, 64);
        assert!(ticks < 64, "lock never finished");

        let lock = plugin
            .a0_locks
            .first()
            .expect("the converged lock is stored");
        assert!(lock.converged, "message: {}", plugin.message);
        assert!(
            (lock.measured_a - 0.5).abs() <= plugin.a0_tolerance,
            "measured {}",
            lock.measured_a
        );
        assert!(
            (lock.commanded_a - 0.5 / 0.6).abs() < 0.01,
            "commanded {}",
            lock.commanded_a
        );
        assert!(lock.trials >= 2, "trials {}", lock.trials);
        assert!((lock.frequency_hz - 1_000.0).abs() < 1.0);
        // The drive is left at the depth the lock found, and the lease is
        // released without a safe-off so it stays there for the recording.
        assert!((last_commanded_depth(&sink).expect("depth") - lock.commanded_a).abs() < 0.002);
        let release: ModulationRequestV1 =
            serde_json::from_value(sink.services.last().expect("release").payload.clone())
                .expect("envelope");
        assert!(matches!(
            release.command,
            ModulationCommandV1::ReleaseLease {
                safe_off: false,
                ..
            }
        ));
        // The lock arms the event-count recording for this frequency.
        assert!(plugin.armed_lock().is_some());
        // …and the table is on disk next to the recordings.
        assert!(folder.join(A0_LOCK_FILE).exists());
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Widens the fixture photodiode's contrast window, so it covers a whole
    /// cycle at every frequency a ladder test visits (the lock refuses below
    /// one cycle, which is the point of a different test).
    fn photodiode_window(plugin: &mut StageAA1Plugin, seconds: f64) {
        if let Some(summary) = plugin.photodiode.as_mut() {
            if let Some(optical) = summary.optical_summary.as_mut() {
                optical.window_seconds = Some(seconds);
            }
        }
    }

    /// Rewrites the plugin's phase-0 markers so the trigger reports `hz`, the
    /// way the camera would once the drive has really moved.
    fn trigger_reports(plugin: &mut StageAA1Plugin, hz: f64) {
        let period_us = (1_000_000.0 / hz).round() as u64;
        plugin.camera_markers_us = (0..8).map(|index| index * period_us).collect();
        plugin.fold_cache.replace(None);
    }

    /// Drives a whole frequency ladder to completion against a bench that
    /// delivers `gain ×` the commanded depth, answering every lease/depth/
    /// frequency request and letting the trigger confirm each commanded
    /// frequency. Returns the frequencies whose points were recorded, in order.
    fn run_freq_sweep_to_completion(
        plugin: &mut StageAA1Plugin,
        sink: &mut ControlSink,
        gain: f64,
        max_ticks: usize,
    ) -> Vec<f64> {
        let mut revision = 1;
        let mut recorded = Vec::new();
        control_tick(plugin, PluginControlInbox::default(), sink);
        for _ in 0..max_ticks {
            let Some((phase, target_hz, lease_req, freq_req, granted, applied)) =
                plugin.freq_sweep.as_ref().map(|sweep| {
                    (
                        sweep.phase,
                        sweep.frequency_hz(),
                        sweep.lease_req,
                        sweep.freq_req,
                        sweep.lease_granted,
                        sweep.freq_applied,
                    )
                })
            else {
                break;
            };
            let mut replies = Vec::new();
            match phase {
                FreqSweepPhase::AcquiringLease if !granted => replies.push(accepted(lease_req)),
                FreqSweepPhase::SettingFrequency if !applied && freq_req != 0 => {
                    replies.push(accepted(freq_req));
                }
                FreqSweepPhase::ConfirmingFrequency => trigger_reports(plugin, target_hz),
                FreqSweepPhase::Locking => {
                    if let Some(lock) = plugin.a0_lock.as_ref() {
                        let (depth_req, applied, commanded) =
                            (lock.depth_req, lock.depth_applied, lock.commanded_a);
                        if !applied && depth_req != 0 {
                            replies.push(accepted(depth_req));
                        } else {
                            revision += 1;
                            plugin.photodiode =
                                Some(photodiode_measuring(revision, commanded * gain));
                            photodiode_window(plugin, 0.02);
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
                FreqSweepPhase::Recording => {
                    if let Some(sweep) = plugin.sweep.as_ref() {
                        let (depth_req, applied, expected) =
                            (sweep.depth_req, sweep.depth_applied, sweep.target_a());
                        if !applied && depth_req != 0 {
                            replies.push(accepted(depth_req));
                        } else {
                            revision += 1;
                            plugin.photodiode = Some(photodiode_measuring(revision, expected));
                            photodiode_window(plugin, 0.02);
                        }
                    }
                    // Short-circuit the recording coordinator once the sweep
                    // has seen the point start: this test is about the ladder,
                    // and the coordinator has tests of its own.
                    if plugin.recording.is_active()
                        && plugin
                            .sweep
                            .as_ref()
                            .is_some_and(|sweep| sweep.point_started)
                    {
                        plugin.recording = Recording::idle();
                        plugin.recording_completed_ok = true;
                        recorded.push(target_hz);
                    }
                }
                _ => {}
            }
            control_tick(
                plugin,
                PluginControlInbox {
                    service_replies: replies,
                    ..PluginControlInbox::default()
                },
                sink,
            );
        }
        recorded
    }

    #[test]
    fn the_frequency_ladder_is_log_spaced_and_ordered_reproducibly() {
        let mut plugin = plugin_with_markers();
        plugin.min_f = 1.0;
        plugin.max_f = 100.0;
        plugin.freq_count = 3;

        plugin.freq_order = FreqOrder::Ascending;
        let ladder = plugin.planned_frequencies();
        // Log-spaced: |H(f)| is read per decade, so a decade per step.
        assert_eq!(ladder.len(), 3);
        assert!((ladder[0] - 1.0).abs() < 1e-9);
        assert!((ladder[1] - 10.0).abs() < 1e-6, "middle {}", ladder[1]);
        assert!((ladder[2] - 100.0).abs() < 1e-6);

        // Alternating decorrelates frequency from time without a seed.
        plugin.freq_order = FreqOrder::Alternating;
        let order: Vec<f64> = plugin
            .freq_sweep_points()
            .iter()
            .map(|point| point.frequency_hz)
            .collect();
        assert!((order[0] - 1.0).abs() < 1e-9 && (order[1] - 100.0).abs() < 1e-6);
        assert!((order[2] - 10.0).abs() < 1e-6);

        // A seeded random order is reproducible — the seed is in the sidecar.
        plugin.freq_order = FreqOrder::Random;
        plugin.freq_count = 8;
        plugin.freq_seed = 42;
        let first: Vec<f64> = plugin
            .freq_sweep_points()
            .iter()
            .map(|point| point.frequency_hz)
            .collect();
        let again: Vec<f64> = plugin
            .freq_sweep_points()
            .iter()
            .map(|point| point.frequency_hz)
            .collect();
        assert_eq!(first, again, "the seeded order must be reproducible");
        plugin.freq_seed = 43;
        let other: Vec<f64> = plugin
            .freq_sweep_points()
            .iter()
            .map(|point| point.frequency_hz)
            .collect();
        assert_ne!(first, other, "a different seed must shuffle differently");
        let mut sorted = first.clone();
        sorted.sort_by(f64::total_cmp);
        let mut planned = plugin.planned_frequencies();
        planned.sort_by(f64::total_cmp);
        assert_eq!(sorted.len(), planned.len(), "the shuffle is a permutation");
    }

    #[test]
    fn the_low_frequency_reference_is_interleaved_into_the_ladder() {
        let mut plugin = plugin_with_markers();
        plugin.min_f = 1.0;
        plugin.max_f = 1_000.0;
        plugin.freq_count = 4;
        plugin.freq_order = FreqOrder::Ascending;
        plugin.freq_reference_every = 2;

        let points = plugin.freq_sweep_points();
        let flags: Vec<bool> = points.iter().map(|point| point.is_reference).collect();
        assert_eq!(flags, [false, false, true, false, false, true]);
        for point in points.iter().filter(|point| point.is_reference) {
            assert!(
                (point.frequency_hz - 1.0).abs() < 1e-9,
                "the reference repeats the lowest planned frequency"
            );
        }
    }

    #[test]
    fn the_frequency_sweep_locks_and_records_every_point_on_one_lease() {
        let folder = temp_folder("fsweep");
        let mut plugin = plugin_locking(0.6, &folder);
        plugin.a0_target = 0.5;
        plugin.min_f = 100.0;
        plugin.max_f = 1_000.0;
        plugin.freq_count = 2;
        plugin.freq_order = FreqOrder::Ascending;
        photodiode_window(&mut plugin, 0.02);
        plugin.freq_sweep_pending = Some(FreqSweepMode::A0Point);
        let mut sink = ControlSink::default();

        let recorded = run_freq_sweep_to_completion(&mut plugin, &mut sink, 0.6, 4_000);

        assert_eq!(recorded.len(), 2, "message: {}", plugin.message);
        assert!((recorded[0] - 100.0).abs() < 1.0 && (recorded[1] - 1_000.0).abs() < 10.0);
        assert!(plugin.freq_sweep.is_none(), "the ladder must finish");
        assert!(
            plugin.message.contains("2/2 points recorded"),
            "message: {}",
            plugin.message
        );

        // One lease for the whole ladder: the operator's drive settings are
        // locked out from the first frequency to the last, so the amplitude
        // provably cannot move between a lock and the point that replays it.
        let commands: Vec<ModulationCommandV1> = sink
            .services
            .iter()
            .filter_map(|request| {
                serde_json::from_value::<ModulationRequestV1>(request.payload.clone())
                    .ok()
                    .map(|envelope| envelope.command)
            })
            .collect();
        let acquired = commands
            .iter()
            .filter(|command| matches!(command, ModulationCommandV1::AcquireLease { .. }))
            .count();
        let released = commands
            .iter()
            .filter(|command| matches!(command, ModulationCommandV1::ReleaseLease { .. }))
            .count();
        assert_eq!(acquired, 1, "one lease for the ladder, not one per child");
        assert_eq!(released, 1, "released exactly once, at the end");
        assert!(commands
            .iter()
            .any(|command| matches!(command, ModulationCommandV1::SetDriveFrequency { .. })));

        // Both frequencies are locked, each at the depth its own roll-off needs.
        assert_eq!(plugin.a0_locks.len(), 2);
        for lock in &plugin.a0_locks {
            assert!(
                lock.converged,
                "lock at {} did not converge",
                lock.frequency_hz
            );
            assert!((lock.measured_a - 0.5).abs() <= plugin.a0_tolerance);
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_frequency_the_trigger_never_confirms_is_skipped_not_fatal() {
        // The firmware ACKs a table it accepted, not light that is modulating.
        // A point whose trigger never reports the commanded period is skipped
        // and named; the rest of the ladder is still worth having.
        let folder = temp_folder("fskip");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.a0_target = 0.5;
        plugin.min_f = 100.0;
        plugin.max_f = 1_000.0;
        plugin.freq_count = 2;
        plugin.freq_order = FreqOrder::Ascending;
        photodiode_window(&mut plugin, 0.02);
        plugin.freq_sweep_pending = Some(FreqSweepMode::A0Point);
        let mut sink = ControlSink::default();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        let mut revision = 1;
        let mut recorded = Vec::new();
        for _ in 0..4_000 {
            let Some((phase, target_hz, lease_req, freq_req, granted, applied)) =
                plugin.freq_sweep.as_ref().map(|sweep| {
                    (
                        sweep.phase,
                        sweep.frequency_hz(),
                        sweep.lease_req,
                        sweep.freq_req,
                        sweep.lease_granted,
                        sweep.freq_applied,
                    )
                })
            else {
                break;
            };
            let mut replies = Vec::new();
            match phase {
                FreqSweepPhase::AcquiringLease if !granted => replies.push(accepted(lease_req)),
                FreqSweepPhase::SettingFrequency if !applied && freq_req != 0 => {
                    replies.push(accepted(freq_req));
                }
                FreqSweepPhase::ConfirmingFrequency => {
                    // The trigger confirms 100 Hz but never moves to 1 kHz.
                    if target_hz < 500.0 {
                        trigger_reports(&mut plugin, target_hz);
                    } else if let Some(sweep) = plugin.freq_sweep.as_mut() {
                        sweep.confirm_deadline_ms = 1;
                    }
                }
                FreqSweepPhase::Locking => {
                    if let Some(lock) = plugin.a0_lock.as_ref() {
                        let (depth_req, applied, commanded) =
                            (lock.depth_req, lock.depth_applied, lock.commanded_a);
                        if !applied && depth_req != 0 {
                            replies.push(accepted(depth_req));
                        } else {
                            revision += 1;
                            plugin.photodiode = Some(photodiode_measuring(revision, commanded));
                            photodiode_window(&mut plugin, 0.02);
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                    }
                }
                FreqSweepPhase::Recording => {
                    if let Some(sweep) = plugin.sweep.as_ref() {
                        let (depth_req, applied, expected) =
                            (sweep.depth_req, sweep.depth_applied, sweep.target_a());
                        if !applied && depth_req != 0 {
                            replies.push(accepted(depth_req));
                        } else {
                            revision += 1;
                            plugin.photodiode = Some(photodiode_measuring(revision, expected));
                            photodiode_window(&mut plugin, 0.02);
                        }
                    }
                    if plugin.recording.is_active()
                        && plugin
                            .sweep
                            .as_ref()
                            .is_some_and(|sweep| sweep.point_started)
                    {
                        plugin.recording = Recording::idle();
                        plugin.recording_completed_ok = true;
                        recorded.push(target_hz);
                    }
                }
                _ => {}
            }
            control_tick(
                &mut plugin,
                PluginControlInbox {
                    service_replies: replies,
                    ..PluginControlInbox::default()
                },
                &mut sink,
            );
        }

        assert_eq!(recorded.len(), 1, "message: {}", plugin.message);
        assert!(plugin.freq_sweep.is_none());
        assert!(
            plugin.message.contains("1/2 points recorded") && plugin.message.contains("1 skipped"),
            "message: {}",
            plugin.message
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_frequency_sweep_refuses_a_ladder_its_photodiode_cannot_measure() {
        // The estimator window is one window for the whole ladder, so the
        // *lowest* point decides measurability. Refuse the plan, not its
        // ninth point two hours in.
        let folder = temp_folder("fladder");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.a0_target = 0.5;
        plugin.min_f = 0.1;
        plugin.max_f = 100.0;
        plugin.freq_count = 4;
        if let Some(summary) = plugin.photodiode.as_mut() {
            if let Some(optical) = summary.optical_summary.as_mut() {
                optical.window_seconds = Some(1.0); // 0.1 cycles at 0.1 Hz
            }
        }
        plugin.freq_sweep_pending = Some(FreqSweepMode::A0Point);
        let mut sink = ControlSink::default();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert!(plugin.freq_sweep.is_none(), "the ladder must not start");
        assert!(sink.services.is_empty(), "no lease may be requested");
        assert!(
            plugin.message.contains("lowest point") && plugin.message.contains("cache length"),
            "message: {}",
            plugin.message
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn changing_frequency_drops_the_previous_period_s_markers_and_windows() {
        // The measured period is the mean marker spacing, so markers from the
        // old drive would confirm the new frequency against a mixture. Pilot
        // windows are frozen at a phase of the old period and do not transfer.
        let folder = temp_folder("fflush");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.pilot_windows = Some((
            PhaseWindow {
                start: 0.0,
                end: 0.2,
            },
            PhaseWindow {
                start: 0.5,
                end: 0.7,
            },
        ));
        plugin.freq_sweep = Some(FreqSweep {
            phase: FreqSweepPhase::AcquiringLease,
            mode: FreqSweepMode::A0Point,
            points: vec![FreqSweepPoint {
                frequency_hz: 50.0,
                is_reference: false,
            }],
            index: 0,
            lease_id: LeaseId::new("a1-fsweep-test"),
            lease_granted: true,
            lease_req: 0,
            freq_req: 0,
            freq_applied: false,
            confirm_deadline_ms: 0,
            skip_reason: None,
            failed: Vec::new(),
            recorded: 0,
            order: FreqOrder::Ascending,
            seed: 1,
            last_activity_ms: now_unix_ms(),
            stop_requested: false,
            eta: Eta::new(0),
        });
        let mut sink = ControlSink::default();
        plugin.send_freq_sweep_frequency(&mut sink);

        assert!(plugin.camera_markers_us.is_empty());
        assert!(plugin.camera_events.is_empty());
        assert!(
            plugin.pilot_windows.is_none(),
            "windows frozen at another period must not carry over"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a0_lock_refuses_a_photodiode_window_shorter_than_one_cycle() {
        // `a` is peak-to-peak. Under one cycle the photodiode under-reports it,
        // and the lock divides by it — so it would inflate the drive until it
        // railed. Refuse before touching the drive, and say what to change.
        let folder = temp_folder("subcycle");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.a0_target = 0.5;
        // 1 kHz markers give the plugin its frequency; make the estimator
        // window 0.4 ms, i.e. 0.4 of a cycle.
        if let Some(summary) = plugin.photodiode.as_mut() {
            if let Some(optical) = summary.optical_summary.as_mut() {
                optical.window_seconds = Some(0.000_4);
            }
        }
        plugin.a0_lock_pending = true;
        let mut sink = ControlSink::default();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert!(plugin.a0_lock.is_none(), "the lock must not start");
        assert!(sink.services.is_empty(), "no lease may be requested");
        assert!(
            plugin.message.contains("0.40 cycles") && plugin.message.contains("cache length"),
            "message: {}",
            plugin.message
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a0_gates_quote_the_owners_reason_for_withholding_a() {
        // The old refusal named the anchor and the cable whatever the real cause
        // was, which sent the operator to re-check a calibration that was
        // already fine. Whatever gate the owner closed has to reach the panel.
        let folder = temp_folder("blocker");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.a0_target = 0.5;
        if let Some(summary) = plugin.photodiode.as_mut() {
            summary.optical_summary = None;
            summary.optical_unavailable = Some("ADC clipping: 307‰ low / 0‰ high".into());
        }

        plugin.a0_lock_pending = true;
        let mut sink = ControlSink::default();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert!(plugin.a0_lock.is_none(), "the lock must not start");
        assert!(
            plugin.message.contains("307‰ low"),
            "the a₀ refusal must quote the owner: {}",
            plugin.message
        );
        // And without pressing anything: the resting panel says the same thing.
        let status = plugin
            .status_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            status.contains("307‰ low"),
            "the status panel must name the gate: {status}"
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn the_status_panel_names_live_analysis_when_it_is_off() {
        // "0 events, free-running" describes the toggle, not the bench, and the
        // frequency sweep refuses on the marker count it produces.
        let folder = temp_folder("liveoff");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.live = false;
        plugin.camera_markers_us.clear();
        // Clear the gates that sit *before* the marker check, so the refusal
        // under test is the marker one and not the optical-window one.
        plugin.min_f = 1.0;
        plugin.max_f = 1.0;
        photodiode_window(&mut plugin, 4.0);

        let status = plugin
            .status_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(status.contains("Live analysis is OFF"), "status: {status}");

        plugin.freq_sweep_pending = Some(FreqSweepMode::A0Point);
        let mut sink = ControlSink::default();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert!(plugin.freq_sweep.is_none(), "the sweep must not start");
        assert!(
            plugin.message.contains("Live analysis"),
            "the sweep refusal must name the toggle: {}",
            plugin.message
        );
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a0_lock_refuses_to_lock_onto_an_unsettled_operating_point() {
        // Readings that walk across the target are not a lock: the next action
        // would record at wherever the drive drifted to, not at a₀.
        let folder = temp_folder("unsettled");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.a0_target = 0.5;
        plugin.a0_lock_pending = true;
        let mut sink = ControlSink::default();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        let mut revision = 1;
        let mut drift = 0.30;
        for _ in 0..64 {
            if plugin.a0_lock.is_none() {
                break;
            }
            let (lease_req, depth_req, granted, applied) = {
                let lock = plugin.a0_lock.as_ref().expect("lock");
                (
                    lock.lease_req,
                    lock.depth_req,
                    lock.lease_granted,
                    lock.depth_applied,
                )
            };
            let mut replies = Vec::new();
            if !granted {
                replies.push(accepted(lease_req));
            } else if !applied && depth_req != 0 {
                replies.push(accepted(depth_req));
            } else {
                revision += 1;
                drift += 0.20; // 0.50, 0.70, 0.90 — straddling a₀ = 0.50
                plugin.photodiode = Some(photodiode_measuring(revision, drift));
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            control_tick(
                &mut plugin,
                PluginControlInbox {
                    service_replies: replies,
                    ..PluginControlInbox::default()
                },
                &mut sink,
            );
        }

        assert!(plugin.a0_lock.is_none(), "the lock must end");
        assert!(
            plugin.message.contains("not settled"),
            "message: {}",
            plugin.message
        );
        // Nothing is stored, so nothing can arm a recording.
        assert!(plugin.a0_locks.is_empty());
        assert!(plugin.armed_lock().is_none());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a0_lock_reports_an_unreachable_depth_instead_of_arming_a_recording() {
        // The bench delivers 5 % of the commanded depth: a₀ = 0.5 would need a
        // commanded depth far beyond what the owner accepts.
        let folder = temp_folder("unreachable");
        let mut plugin = plugin_locking(0.05, &folder);
        plugin.a0_target = 0.5;
        plugin.a0_lock_pending = true;
        let mut sink = ControlSink::default();

        assert!(run_lock_to_completion(&mut plugin, &mut sink, 0.05, 256) < 256);
        let lock = plugin.a0_locks.first().expect("the attempt is recorded");
        assert!(!lock.converged);
        assert!((lock.commanded_a - COMMANDED_A_MAX).abs() < 1e-9);
        assert!(
            plugin.message.contains("drivable limit")
                || plugin.message.contains("did not converge"),
            "message: {}",
            plugin.message
        );
        // A non-converged lock must never arm an event-count recording.
        assert!(plugin.armed_lock().is_none());
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a0_lock_surfaces_a_drive_rejection_verbatim() {
        let folder = temp_folder("reject");
        let mut plugin = plugin_locking(1.0, &folder);
        plugin.a0_lock_pending = true;
        let mut sink = ControlSink::default();
        // Tick 1: begin and lease.
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = plugin.a0_lock.as_ref().expect("lock").lease_req;
        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![accepted(lease_req)],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        let depth_req = plugin.a0_lock.as_ref().expect("lock").depth_req;
        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![rejected(
                    depth_req,
                    "calibrated optical peak u = 1.2 exceeds the lobe ceiling",
                )],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        assert!(plugin.a0_lock.is_none(), "the lock must not keep trying");
        assert!(
            plugin.message.contains("lobe ceiling"),
            "message: {}",
            plugin.message
        );
        assert!(plugin.a0_locks.is_empty(), "a rejected lock stores nothing");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn event_count_point_commands_the_locked_depth_not_a0() {
        let folder = temp_folder("ecpoint");
        let mut plugin = plugin_locking(0.6, &folder);
        plugin.a0_target = 0.5;
        plugin.a0_locks.push(A0LockPoint {
            frequency_hz: 1_000.0,
            target_a: 0.5,
            commanded_a: 0.8333,
            measured_a: 0.5,
            trials: 2,
            converged: true,
            locked_at_unix_ms: now_unix_ms(),
            low_clip_fraction: Some(0.0),
            high_clip_fraction: Some(0.0),
            depth_source: DepthSource::Photodiode,
        });
        let mut sink = ControlSink::default();

        plugin
            .set_setting("record_a0_point", json!(true))
            .expect("press");
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let sweep = plugin.sweep.as_ref().expect("event-count sweep");
        assert_eq!(sweep.kind, SweepKind::EventCount);
        assert_eq!(sweep.total(), 1);
        let lease_req = sweep.lease_req;

        control_tick(
            &mut plugin,
            PluginControlInbox {
                service_replies: vec![accepted(lease_req)],
                ..PluginControlInbox::default()
            },
            &mut sink,
        );
        // The drive is commanded to the locked depth, *not* to a₀ itself.
        let commanded = last_commanded_depth(&sink).expect("commanded depth");
        assert!((commanded - 0.833).abs() < 0.002, "commanded {commanded}");
        assert!((plugin.sweep.as_ref().expect("sweep").target_a() - 0.5).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn event_count_stems_and_sidecars_carry_the_frequency_and_the_lock() {
        let folder = temp_folder("ecstem");
        let mut plugin = plugin_locking(0.6, &folder);
        plugin.measurement_id = "A1-ecrow".into();
        plugin.frame_width = 4;
        plugin.frame_height = 1;
        let lock = A0LockPoint {
            frequency_hz: 1_000.0,
            target_a: 0.5,
            commanded_a: 0.8333,
            measured_a: 0.5,
            trials: 2,
            converged: true,
            locked_at_unix_ms: 1_784_764_800_000,
            low_clip_fraction: Some(0.0),
            high_clip_fraction: Some(0.0),
            depth_source: DepthSource::Photodiode,
        };
        plugin.sweep = Some(Sweep {
            phase: SweepPhase::Recording,
            kind: SweepKind::EventCount,
            points: vec![SweepPoint {
                commanded_a: 0.8333,
                expected_a: 0.5,
            }],
            lock: Some(lock),
            index: 0,
            lease_id: LeaseId::new("a1-sweep-test"),
            lease_granted: true,
            lease_req: 0,
            owns_lease: true,
            depth_req: 0,
            depth_applied: true,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: true,
            completed_ok: false,
            last_activity_ms: 0,
            stop_requested: false,
            eta: Eta::new(0),
        });

        // The stem carries the frequency instead of a sweep-point index.
        let mut sink = ControlSink::default();
        plugin.begin_recording(&mut sink, RecRole::EventCount);
        let stem = plugin.recording.stem.clone();
        assert!(stem.ends_with("_ec_f1000Hz"), "stem: {stem}");

        plugin.recording.duration_s = 5;
        plugin.recording.start_unix_ms = 1_784_764_800_000;
        let path = plugin.write_sidecar().expect("sidecar path");
        let text = std::fs::read_to_string(&path).expect("read sidecar");
        assert!(text.contains("role = \"event-count point\""), "{text}");
        assert!(text.contains("[a0_lock]"), "{text}");
        assert!(text.contains("target_a = 0.5"), "{text}");
        assert!(text.contains("commanded_a = 0.8333"), "{text}");
        assert!(text.contains("converged = true"), "{text}");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn frequency_tags_are_file_safe() {
        assert_eq!(frequency_tag(50.0), "f50Hz");
        assert_eq!(frequency_tag(0.5), "f0p5Hz");
        assert_eq!(frequency_tag(1_200.0), "f1200Hz");
        assert_eq!(frequency_tag(12.345), "f12p345Hz");
        assert_eq!(sanitize_stem(&frequency_tag(0.5)), frequency_tag(0.5));
    }

    #[test]
    fn locks_are_one_per_frequency_and_round_trip_through_the_folder() {
        let dir = std::env::temp_dir().join(format!("a1-a0-{}", now_unix_ms()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let folder = dir.display().to_string();

        let mut plugin = StageAA1Plugin {
            output_folder: folder.clone(),
            ..StageAA1Plugin::default()
        };
        let point = |hz: f64, commanded_a: f64| A0LockPoint {
            frequency_hz: hz,
            target_a: 0.5,
            commanded_a,
            measured_a: 0.5,
            trials: 2,
            converged: true,
            locked_at_unix_ms: now_unix_ms(),
            low_clip_fraction: None,
            high_clip_fraction: None,
            depth_source: DepthSource::Photodiode,
        };
        plugin.store_lock(point(1_000.0, 0.83)).expect("saved");
        plugin.store_lock(point(50.0, 0.52)).expect("saved");
        // Re-locking the same frequency replaces the row rather than appending.
        plugin.store_lock(point(1_000.5, 0.86)).expect("saved");
        assert_eq!(plugin.a0_locks.len(), 2);
        assert!(
            (plugin.a0_locks[0].frequency_hz - 50.0).abs() < 1e-9,
            "sorted by frequency"
        );

        let mut other = StageAA1Plugin {
            output_folder: folder.clone(),
            ..StageAA1Plugin::default()
        };
        other.load_a0_locks();
        assert_eq!(other.a0_locks.len(), 2);
        let reloaded = other.lock_for_frequency(1_000.0).expect("reloaded lock");
        assert!((reloaded.commanded_a - 0.86).abs() < 1e-9);
        assert_eq!(other.a0_locks_dataset().columns.len(), 8);
        // A table written before the depth-source setting existed loads as the
        // photodiode-measured rows it was: the field defaults, it is not lost.
        assert_eq!(reloaded.depth_source, DepthSource::Photodiode);

        let _ = std::fs::remove_dir_all(&dir);
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

    /// Turning Live analysis off used to leave the last analysis window in
    /// place — up to millions of events — and the control tick keeps folding
    /// whatever is in the buffer. So the plots stayed slow after the switch was
    /// off again, which is exactly what the operator reported.
    #[test]
    fn turning_live_analysis_off_releases_the_event_buffer() {
        let mut plugin = plugin_with_markers();
        plugin.live = true;
        plugin.camera_events = (0..50_000)
            .map(|index| CameraEvent {
                x: 0,
                y: 0,
                timestamp_us: index,
                polarity: Polarity::On,
            })
            .collect();
        plugin.camera_markers_us = (0..100).map(|index| index * 1_000).collect();
        // Prime the memoised fold so the stale one cannot survive either.
        let _ = plugin.current_fold();

        plugin.set_setting("live", json!(false)).expect("live off");

        assert!(plugin.camera_events.is_empty());
        assert!(plugin.camera_markers_us.is_empty());
        assert!(
            plugin.camera_events.capacity() == 0,
            "the buffer kept {} events' worth of capacity reserved",
            plugin.camera_events.capacity()
        );
        assert!(
            plugin.fold_cache.borrow().is_none(),
            "a stale fold survived"
        );
    }

    /// The Clear button and the off switch must leave the plugin in the same
    /// state — they are the same operation.
    #[test]
    fn clearing_captured_events_releases_the_same_buffers() {
        let mut plugin = plugin_with_markers();
        plugin.live = true;
        plugin.camera_events = vec![CameraEvent {
            x: 0,
            y: 0,
            timestamp_us: 1,
            polarity: Polarity::On,
        }];
        plugin.camera_markers_us = vec![0, 1_000];

        plugin.set_setting("clear", json!(true)).expect("clear");

        assert!(plugin.camera_events.is_empty());
        assert!(plugin.camera_markers_us.is_empty());
        assert!(plugin.fold_cache.borrow().is_none());
    }

    /// `settings_schema` is rendered by the UI mirror, which never runs
    /// `process_control` — so every run lives on an instance the panel cannot
    /// see. Gating a button on "is something running" therefore disables
    /// nothing and lies to the next reader; the interlocks belong worker-side.
    #[test]
    fn buttons_are_not_gated_on_state_the_ui_mirror_cannot_see() {
        let mut mirror = StageAA1Plugin {
            output_folder: "/tmp/a1-mirror".into(),
            ..StageAA1Plugin::default()
        };
        mirror.set_runtime_role(PluginRuntimeRole::UiMirror);

        let enabled_of = |plugin: &StageAA1Plugin, key: &str| {
            plugin
                .settings_schema()
                .sections
                .iter()
                .flat_map(|section| section.items.iter())
                .find(|item| item.key == key)
                .and_then(|item| match item.kind {
                    SettingKind::Button { enabled } => Some(enabled),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing button {key}"))
        };

        let buttons = [
            "start_recording",
            "start_sweep",
            "start_freq_sweep",
            "start_freq_depth_sweep",
            "run_protocol",
        ];
        let before: Vec<bool> = buttons.iter().map(|key| enabled_of(&mirror, key)).collect();
        assert!(before.iter().all(|enabled| *enabled), "{before:?}");

        // Now make the *worker-side* state look busy. The mirror renders the
        // same either way, because it never sees any of this.
        mirror.recording.phase = RecPhase::StartingCamera;
        let after: Vec<bool> = buttons.iter().map(|key| enabled_of(&mirror, key)).collect();
        assert_eq!(before, after, "a button was gated on worker-only state");

        // Without an output folder they *are* disabled — that is mirrored
        // state, so it is a legitimate gate.
        mirror.output_folder = String::new();
        for key in buttons {
            assert!(!enabled_of(&mirror, key), "{key} ignored the output folder");
        }
    }

    /// The Sweep f button and the depth it holds have to be in the same place:
    /// the button used to be in Record while `a₀` sat in a collapsed section.
    #[test]
    fn the_depth_sweep_f_holds_sits_beside_the_frequency_axis() {
        let schema = StageAA1Plugin::default().settings_schema();
        let record = schema
            .sections
            .iter()
            .find(|section| section.label == "Record")
            .expect("a single Record section");
        for key in ["a0_target", "min_f", "max_f", "start_freq_sweep"] {
            assert!(
                record.items.iter().any(|item| item.key == key),
                "{key} is not in the Record section"
            );
        }
    }

    /// The protocol's own duration must not be written into the panel setting:
    /// the host re-applies the mirror's snapshot every pass, so it would be
    /// reverted within the frame and the operator's value would flicker.
    #[test]
    fn a_protocol_duration_overrides_without_touching_the_panel_setting() {
        let folder = temp_folder("protocol-duration-override");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        plugin.duration_s = 999;
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );
        let retargets: Vec<u64> = sink
            .services
            .iter()
            .filter(|request| {
                matches!(
                    modulation_command(request),
                    Some(
                        ModulationCommandV1::SetOperatingPoint { .. }
                            | ModulationCommandV1::SetDriveFrequency { .. }
                            | ModulationCommandV1::SetOpticalDepth { .. }
                    )
                )
            })
            .map(|request| request.request_id)
            .collect();
        control_tick(
            &mut plugin,
            inbox_with(retargets.into_iter().map(accepted).collect()),
            &mut sink,
        );
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert_eq!(
            plugin.recording.duration_s, 3,
            "the protocol's duration lost"
        );
        assert_eq!(
            plugin.duration_s, 999,
            "the protocol overwrote the operator's own duration setting"
        );
        // And it is consumed, so the next hand-driven recording is the panel's.
        assert!(plugin.pending_duration_s.is_none());

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Drives a pending protocol up to the tick that starts its first row's
    /// recording: take the lease, apply the three retargets, let the dwell pass.
    fn run_protocol_to_first_recording(plugin: &mut StageAA1Plugin, sink: &mut ControlSink) {
        control_tick(plugin, PluginControlInbox::default(), sink);
        let lease_req = sink
            .services
            .iter()
            .find_map(|request| match modulation_command(request) {
                Some(ModulationCommandV1::AcquireLease { .. }) => Some(request.request_id),
                _ => None,
            })
            .expect("the protocol takes a modulation lease");
        sink.services.clear();
        control_tick(plugin, inbox_with(vec![accepted(lease_req)]), sink);
        let retargets: Vec<u64> = sink
            .services
            .iter()
            .filter(|request| {
                matches!(
                    modulation_command(request),
                    Some(
                        ModulationCommandV1::SetOperatingPoint { .. }
                            | ModulationCommandV1::SetDriveFrequency { .. }
                            | ModulationCommandV1::SetOpticalDepth { .. }
                    )
                )
            })
            .map(|request| request.request_id)
            .collect();
        sink.services.clear();
        control_tick(
            plugin,
            inbox_with(retargets.into_iter().map(accepted).collect()),
            sink,
        );
        // Settle (`settle_s = 0`) then hand off to the recording coordinator.
        control_tick(plugin, PluginControlInbox::default(), sink);
    }

    /// Every row of a protocol is recorded with the same panel state, so a row
    /// that does not name itself is indistinguishable on disk from the row
    /// before it but for the second it started. The operating point a `.pdq`
    /// was taken at then cannot be recovered from its name at all — which is
    /// exactly what a survey's files are for.
    #[test]
    fn a_protocol_row_names_its_own_point_in_the_file_stem() {
        let folder = temp_folder("protocol-file-stem");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        run_protocol_to_first_recording(&mut plugin, &mut sink);

        let stem = plugin.recording.stem.clone();
        assert!(
            stem.ends_with("_p01_u400m_f25Hz_a700m"),
            "the stem does not name the protocol point: {stem}"
        );
        // Two rows of the same protocol differ by more than their timestamp.
        let second = TWO_POINT_PROTOCOL.replace("mean_u = [0.4, 0.6]", "mean_u = [0.6]");
        let other_folder = temp_folder("protocol-file-stem-2");
        let (mut other, _) = protocol_plugin(&other_folder, &second);
        let mut other_sink = ControlSink::default();
        run_protocol_to_first_recording(&mut other, &mut other_sink);
        assert!(
            other.recording.stem.ends_with("_p01_u600m_f25Hz_a700m"),
            "the second row reuses the first row's tag: {}",
            other.recording.stem
        );

        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&other_folder);
    }

    /// The PDQ travels on its own — it is the photodiode owner's file, and an
    /// analysis that opens it must be able to say which row of which protocol
    /// it is without joining through the A1 sidecar next door.
    #[test]
    fn a_protocol_row_names_itself_in_the_recording_metadata() {
        let folder = temp_folder("protocol-metadata");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        run_protocol_to_first_recording(&mut plugin, &mut sink);
        let meta = plugin.recording_metadata();

        assert_eq!(
            meta.get("protocol_name").map(String::as_str),
            Some("two-point")
        );
        assert_eq!(meta.get("protocol_block").map(String::as_str), Some("pair"));
        assert_eq!(
            meta.get("protocol_point_index").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            meta.get("protocol_point_total").map(String::as_str),
            Some("2")
        );
        assert_eq!(
            meta.get("protocol_point_tag").map(String::as_str),
            Some("u400m_f25Hz_a700m")
        );
        assert_eq!(
            meta.get("protocol_mean_u").map(String::as_str),
            Some("0.400000")
        );
        assert_eq!(
            meta.get("protocol_frequency_hz").map(String::as_str),
            Some("25.000000")
        );
        assert_eq!(
            meta.get("protocol_depth_a").map(String::as_str),
            Some("0.700000")
        );

        // The metadata map has to stay inside the photodiode owner's bounds, or
        // BeginRecording is refused and the row records nothing at all.
        assert!(meta.len() <= 64, "{} metadata keys", meta.len());
        assert!(meta
            .iter()
            .all(|(key, value)| key.len() <= 128 && value.len() <= 1_024));

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The A1 sidecar had a section for every automatic path *except* the one
    /// that names all three axes: a protocol row's `[sweep]` carries only the
    /// panel's `min_a`/`max_a`, so nothing on disk said which line of the file
    /// had asked for the recording.
    #[test]
    fn the_a1_sidecar_names_the_protocol_row() {
        let folder = temp_folder("protocol-sidecar");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        run_protocol_to_first_recording(&mut plugin, &mut sink);
        // The quantitative sidecar needs a measured optical summary; the plan
        // itself is admitted against the photodiode the fixture starts with.
        plugin.photodiode = Some(fresh_photodiode_summary());
        let path = plugin.write_sidecar().expect("sidecar path");
        let text = std::fs::read_to_string(&path).expect("read sidecar");

        assert!(text.contains("[protocol]"), "{text}");
        assert!(text.contains("name = \"two-point\""), "{text}");
        assert!(text.contains("block = \"pair\""), "{text}");
        assert!(text.contains("point_index = 1"), "{text}");
        assert!(text.contains("point_total = 2"), "{text}");
        assert!(text.contains("point_tag = \"u400m_f25Hz_a700m\""), "{text}");
        assert!(text.contains("requested_mean_u = 0.4"), "{text}");
        assert!(text.contains("requested_frequency_hz = 25.0"), "{text}");
        assert!(text.contains("requested_depth_a = 0.7"), "{text}");

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A run that is not driving anything must not claim a protocol row.
    #[test]
    fn a_hand_driven_recording_carries_no_protocol_keys() {
        let mut plugin = plugin_with_markers();
        plugin.modulation = Some(connected_modulation());
        let meta = plugin.recording_metadata();
        assert!(!meta.contains_key("protocol_name"));
        assert!(!meta.contains_key("protocol_point_index"));
    }

    /// What the drive was actually doing, in the photodiode's own sidecar. A
    /// frequency and two DAC codes do not say which lobe, or which operating
    /// point `ū`, produced a trace — and `ū` is a whole axis of the survey.
    #[test]
    fn the_recording_metadata_describes_how_the_drive_was_shaped() {
        let mut plugin = plugin_with_markers();
        let mut state = commanded_modulation(1, 0.7);
        state.acknowledged = Some(stage_a_plugin_contract::ModulationTargetV1 {
            revision: SemanticRevision(1),
            waveform: Some(WaveformV1::Periodic {
                waveform: stage_a_plugin_contract::PeriodicWaveformV1::Sine,
                min_dac: 100,
                max_dac: 800,
                frequency_millihz: 25_000,
            }),
            a1_configuration: None,
            acquisition_running: true,
            board_dac_code: None,
            firmware_configuration_revision: None,
        });
        plugin.modulation = Some(state);
        let meta = plugin.recording_metadata();

        for key in [
            "modulation_waveform",
            "pockels_calibration_id",
            "optical_target",
            "requested_mean_u",
            "resolved_mean_u",
            "commanded_a",
            "v_null_dac",
            "v_peak_dac",
        ] {
            assert!(meta.contains_key(key), "{key} missing from {meta:?}");
        }
        assert_eq!(
            meta.get("optical_target").map(String::as_str),
            Some("log_sine")
        );
        assert_eq!(
            meta.get("requested_mean_u").map(String::as_str),
            Some("0.500000")
        );
        assert_eq!(
            meta.get("commanded_a").map(String::as_str),
            Some("0.700000")
        );
    }

    /// A protocol file on disk, and a plugin ready to run it.
    fn protocol_plugin(folder: &Path, body: &str) -> (StageAA1Plugin, PathBuf) {
        std::fs::create_dir_all(folder).expect("protocol folder");
        let path = folder.join("protocol.toml");
        std::fs::write(&path, body).expect("write protocol");
        let mut plugin = plugin_with_markers();
        plugin.modulation = Some(connected_modulation());
        plugin.photodiode = Some(ready_photodiode());
        plugin.output_folder = folder.display().to_string();
        plugin.measurement_id = "A1-proto".into();
        plugin.protocol_path = path.display().to_string();
        plugin.protocol_pending = true;
        (plugin, path)
    }

    const TWO_POINT_PROTOCOL: &str = r#"
name = "two-point"

[defaults]
duration_s = 3
settle_s = 0.0

[[block]]
name = "pair"
mean_u = [0.4, 0.6]
frequency_hz = 25.0
depth_a = 0.7
"#;

    /// The reason a protocol exists rather than three nested button presses:
    /// every point states its whole operating condition, so all three axes are
    /// commanded at every point instead of being left wherever the last one
    /// happened to leave them. `I_k` (ū) is the axis the buttons could not
    /// sweep at all.
    #[test]
    fn a_protocol_commands_all_three_axes_at_every_point() {
        let folder = temp_folder("protocol-axes");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink
            .services
            .iter()
            .find_map(|request| match modulation_command(request) {
                Some(ModulationCommandV1::AcquireLease { .. }) => Some(request.request_id),
                _ => None,
            })
            .expect("the protocol takes a modulation lease");

        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );

        let commands: Vec<ModulationCommandV1> = sink
            .services
            .iter()
            .filter_map(modulation_command)
            .collect();
        assert!(
            commands.iter().any(|command| matches!(
                command,
                ModulationCommandV1::SetOperatingPoint { mean_u_milli: 400 }
            )),
            "the I_k axis was not commanded: {commands:?}"
        );
        assert!(
            commands.iter().any(|command| matches!(
                command,
                ModulationCommandV1::SetDriveFrequency {
                    frequency_millihz: 25_000
                }
            )),
            "the frequency axis was not commanded: {commands:?}"
        );
        assert!(
            commands.iter().any(|command| matches!(
                command,
                ModulationCommandV1::SetOpticalDepth { depth_a_milli: 700 }
            )),
            "the depth axis was not commanded: {commands:?}"
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Recording before every axis has been acknowledged would file the run
    /// under parameters the bench was not actually at.
    #[test]
    fn a_protocol_point_waits_for_all_three_retargets_before_recording() {
        let folder = temp_folder("protocol-wait");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );

        let retargets: Vec<u64> = sink
            .services
            .iter()
            .filter(|request| {
                matches!(
                    modulation_command(request),
                    Some(
                        ModulationCommandV1::SetOperatingPoint { .. }
                            | ModulationCommandV1::SetDriveFrequency { .. }
                            | ModulationCommandV1::SetOpticalDepth { .. }
                    )
                )
            })
            .map(|request| request.request_id)
            .collect();
        assert_eq!(retargets.len(), 3);

        // Two of three applied: still not recording.
        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(retargets[0]), accepted(retargets[1])]),
            &mut sink,
        );
        assert!(
            !plugin.recording.is_active(),
            "recording started with a retarget still outstanding"
        );

        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(retargets[2])]),
            &mut sink,
        );
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        assert!(
            plugin.recording.is_active(),
            "the point never started recording; message: {}",
            plugin.message
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A refused point is the common case in a long survey — a `ū`/`a` pair
    /// that runs off the top of the lobe. It must cost that point and carry the
    /// owner's own wording, not abandon the rest of the night's work.
    #[test]
    fn a_refused_point_is_skipped_with_the_owners_reason_and_the_run_continues() {
        let folder = temp_folder("protocol-skip");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );
        let first_retarget = sink
            .services
            .iter()
            .find(|request| {
                matches!(
                    modulation_command(request),
                    Some(ModulationCommandV1::SetOperatingPoint { .. })
                )
            })
            .expect("operating point retarget")
            .request_id;

        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![rejected(
                first_retarget,
                "operating point ū=0.400 rejected: peak exceeds the lobe ceiling",
            )]),
            &mut sink,
        );

        let run = plugin
            .protocol
            .as_ref()
            .expect("the run abandoned the survey");
        assert_eq!(run.index, 1, "the run did not move on to the second point");
        let (index, reason) = run.failed.last().expect("the skip was recorded");
        assert_eq!(*index, 0);
        assert!(
            reason.contains("lobe ceiling"),
            "the owner's reason was replaced: {reason}"
        );
        // And it is on the status pane, because the per-point message has
        // already been overwritten by the next point.
        let status = plugin
            .status_entries()
            .iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(status.contains("lobe ceiling"), "{status}");

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A protocol whose file is wrong must say so on the button press, before
    /// the drive has moved — the whole point is that it runs unattended.
    #[test]
    fn an_invalid_protocol_is_refused_before_the_drive_moves() {
        let folder = temp_folder("protocol-invalid");
        let (mut plugin, _) = protocol_plugin(
            &folder,
            r#"
[[block]]
mean_u = 0.5
frequency_hz = 10.0
depth_a = 99.0
"#,
        );
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert!(plugin.protocol.is_none(), "a bad protocol started anyway");
        assert!(
            sink.services.is_empty(),
            "the drive was touched before the file was validated: {:?}",
            sink.services
        );
        assert!(
            plugin.message.contains("depth_a"),
            "the message does not name the offending axis: {}",
            plugin.message
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The owners cap the lease TTL they grant far below the length of a
    /// survey, so a run that renewed only when it stepped to its next point
    /// lost the drive in the middle of any point longer than that cap — the
    /// owner STOPs and switches the output off, which took the phase-0 trigger
    /// and the photodiode's optical summary with it.
    #[test]
    fn a_long_point_renews_the_modulation_lease_before_the_owner_drops_it() {
        let folder = temp_folder("protocol-lease-heartbeat");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        let lease_id = plugin
            .protocol
            .as_ref()
            .expect("the protocol is running")
            .lease_id
            .clone();
        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );

        // The owner granted far less than the whole-survey TTL that was asked
        // for, and the point is still running.
        let now_ms = now_unix_ms();
        if let Some(state) = plugin.modulation.as_mut() {
            state.lease = Some(LeaseSnapshotV1 {
                lease_id: lease_id.clone(),
                holder: ClientId::new(A1_PLUGIN_ID),
                expires_at_unix_ms: now_ms + LEASE_RENEW_MARGIN_MS / 2,
                run_id: None,
            });
        }
        sink.services.clear();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        let renewed = sink
            .services
            .iter()
            .filter_map(modulation_command)
            .any(|command| matches!(command, ModulationCommandV1::RenewLease { .. }));
        assert!(
            renewed,
            "the lease was left to expire underneath the point: {:?}",
            sink.services
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// A lease with plenty of time left must not be renewed on every tick: the
    /// control plane runs at 20 Hz and each renewal is a device round trip.
    #[test]
    fn a_lease_with_time_left_is_not_renewed_every_tick() {
        let folder = temp_folder("protocol-lease-quiet");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        let lease_id = plugin
            .protocol
            .as_ref()
            .expect("the protocol is running")
            .lease_id
            .clone();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );
        if let Some(state) = plugin.modulation.as_mut() {
            state.lease = Some(LeaseSnapshotV1 {
                lease_id,
                holder: ClientId::new(A1_PLUGIN_ID),
                expires_at_unix_ms: now_unix_ms() + LEASE_RENEW_MARGIN_MS * 4,
                run_id: None,
            });
        }

        sink.services.clear();
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert!(
            !sink
                .services
                .iter()
                .filter_map(modulation_command)
                .any(|command| matches!(command, ModulationCommandV1::RenewLease { .. })),
            "a lease that is nowhere near expiry was renewed anyway: {:?}",
            sink.services
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Starting and stopping the host recorder is reported as `SourceChanged`,
    /// twice per recording. Between two points a protocol is not "recording",
    /// so asking only about the recording treated its own self-inflicted
    /// boundary as an idle-time reset and wiped the survey's pilot windows,
    /// background floor and response curve mid-run.
    #[test]
    fn a_source_change_between_two_protocol_points_keeps_the_survey_state() {
        let folder = temp_folder("protocol-discontinuity");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );
        plugin.background_floor = Some((0.25, 0.25));
        plugin.pilot_windows = Some((
            PhaseWindow {
                start: 0.1,
                end: 0.2,
            },
            PhaseWindow {
                start: 0.6,
                end: 0.7,
            },
        ));
        assert!(!plugin.recording.is_active(), "the point is between stages");

        plugin.on_discontinuity(PluginDiscontinuity::SourceChanged);

        assert_eq!(
            plugin.background_floor,
            Some((0.25, 0.25)),
            "the survey's background reference was wiped between two points"
        );
        assert!(
            plugin.pilot_windows.is_some(),
            "the survey's pilot windows were wiped between two points"
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Each point's own duration governs the recording, not the panel's — a
    /// survey whose lengths silently came from the UI would not be
    /// reproducible from the protocol alone.
    #[test]
    fn a_point_records_for_the_duration_the_file_asks_for() {
        let folder = temp_folder("protocol-duration");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        plugin.duration_s = 999;
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);
        let lease_req = sink.services[0].request_id;
        sink.services.clear();
        control_tick(
            &mut plugin,
            inbox_with(vec![accepted(lease_req)]),
            &mut sink,
        );
        let retargets: Vec<u64> = sink
            .services
            .iter()
            .filter(|request| {
                matches!(
                    modulation_command(request),
                    Some(
                        ModulationCommandV1::SetOperatingPoint { .. }
                            | ModulationCommandV1::SetDriveFrequency { .. }
                            | ModulationCommandV1::SetOpticalDepth { .. }
                    )
                )
            })
            .map(|request| request.request_id)
            .collect();
        control_tick(
            &mut plugin,
            inbox_with(retargets.into_iter().map(accepted).collect()),
            &mut sink,
        );
        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert_eq!(
            plugin.recording.duration_s, 3,
            "the panel's duration overrode the protocol's"
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Every text line of the status pane, joined.
    fn status_text(plugin: &StageAA1Plugin) -> String {
        plugin
            .status_entries()
            .iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// A survey started at 23:00 has to say whether it is a coffee-length or a
    /// night-length one — on the button press, before it is left alone.
    #[test]
    fn a_protocol_says_how_long_it_will_take_before_it_starts() {
        let folder = temp_folder("protocol-estimate");
        let (mut plugin, _) = protocol_plugin(&folder, TWO_POINT_PROTOCOL);
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        // Two rows of 3 s with no settle, plus the handshake around each.
        let expected = format_duration(2.0 * (3.0 + crate::eta::POINT_OVERHEAD_S));
        assert!(
            plugin.message.contains(&format!("≈ {expected}")),
            "the button press does not say how long the survey takes: {}",
            plugin.message
        );
        // And the panel keeps saying it while the run walks the file.
        let status = status_text(&plugin);
        assert!(
            status.contains("Estimated time:"),
            "no estimate on the status pane: {status}"
        );
        assert!(
            status.contains("from the plan"),
            "an estimate with nothing measured yet claims to be a measurement: {status}"
        );

        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The plan is what the operator asked for; the bench charges more for it
    /// (handshakes, settling, a photodiode window). Once a point has actually
    /// finished, the estimate has to follow the bench rather than the file —
    /// otherwise a run that is running at half speed keeps promising the
    /// original finish time right up to the end.
    #[test]
    fn the_estimate_follows_the_measured_pace_not_the_plan() {
        let mut plugin = StageAA1Plugin {
            duration_s: 10,
            settle_s: 0.0,
            sweep_count: 5,
            ..StageAA1Plugin::default()
        };
        let planned_point_s = plugin.planned_point_s();
        let start_ms = 1_000_000;
        plugin.sweep = Some(Sweep {
            phase: SweepPhase::Recording,
            kind: SweepKind::Amplitude,
            points: plugin.sweep_points(),
            lock: None,
            index: 0,
            lease_id: LeaseId::new("test"),
            lease_granted: true,
            lease_req: 0,
            owns_lease: true,
            depth_req: 0,
            depth_applied: true,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: true,
            completed_ok: false,
            last_activity_ms: start_ms,
            stop_requested: false,
            eta: Eta::new(start_ms),
        });

        // Nothing measured yet: five points at their planned cost.
        let planned_total = 5.0 * planned_point_s;
        assert!(
            plugin
                .estimated_time_line(start_ms)
                .expect("a running sweep has an estimate")
                .contains(&format_duration(planned_total)),
            "the first estimate is not the plan's own time"
        );

        // The first point took twice as long as planned. Four points are left,
        // so the estimate has to double them too.
        let after_ms = start_ms + (2.0 * planned_point_s * 1_000.0) as u64;
        if let Some(sweep) = plugin.sweep.as_mut() {
            sweep.eta.point_done(after_ms, planned_point_s);
            sweep.index = 1;
        }
        let line = plugin
            .estimated_time_line(after_ms)
            .expect("a running sweep has an estimate");
        assert!(
            line.contains(&format_duration(8.0 * planned_point_s)),
            "the estimate ignored how long the first point actually took: {line}"
        );
        assert!(
            !line.contains("from the plan"),
            "a measured estimate still calls itself a plan: {line}"
        );
    }

    /// The ladder, its inner depth sweep and a protocol are three nested runs,
    /// and each knows its own remaining time. Printing all three would state
    /// one fact three times, with the two inner ones — which end long before
    /// the run does — reading as contradictions of the one that matters.
    #[test]
    fn only_the_outermost_run_states_an_estimate() {
        let mut plugin = StageAA1Plugin {
            duration_s: 5,
            sweep_count: 3,
            ..StageAA1Plugin::default()
        };
        // The status pane reads the wall clock, so this run has to start on it.
        let now_ms = now_unix_ms();
        plugin.sweep = Some(Sweep {
            phase: SweepPhase::Recording,
            kind: SweepKind::Amplitude,
            points: plugin.sweep_points(),
            lock: None,
            index: 0,
            lease_id: LeaseId::new("test"),
            lease_granted: true,
            lease_req: 0,
            owns_lease: false,
            depth_req: 0,
            depth_applied: true,
            settled_since_ms: None,
            settle_deadline_ms: 0,
            point_started: true,
            completed_ok: false,
            last_activity_ms: now_ms,
            stop_requested: false,
            eta: Eta::new(now_ms),
        });
        plugin.freq_sweep = Some(FreqSweep {
            phase: FreqSweepPhase::Recording,
            mode: FreqSweepMode::DepthSweep,
            points: vec![
                FreqSweepPoint {
                    frequency_hz: 10.0,
                    is_reference: false,
                },
                FreqSweepPoint {
                    frequency_hz: 20.0,
                    is_reference: false,
                },
            ],
            index: 0,
            lease_id: LeaseId::new("test-ladder"),
            lease_granted: true,
            lease_req: 0,
            freq_req: 0,
            freq_applied: true,
            confirm_deadline_ms: 0,
            skip_reason: None,
            failed: Vec::new(),
            recorded: 0,
            order: FreqOrder::default(),
            seed: 1,
            last_activity_ms: now_ms,
            stop_requested: false,
            eta: Eta::new(now_ms),
        });

        let lines: Vec<String> = plugin
            .status_entries()
            .iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) if text.starts_with("Estimated time:") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(lines.len(), 1, "more than one estimate: {lines:?}");
        // And it is the ladder's: both rungs, not just the three depths of the
        // rung currently running.
        let both_rungs = plugin.planned_rung_s(FreqSweepMode::DepthSweep, 10.0)
            + plugin.planned_rung_s(FreqSweepMode::DepthSweep, 20.0);
        assert!(
            lines[0].contains(&format_duration(both_rungs)),
            "the inner sweep's estimate won over the ladder's: {}",
            lines[0]
        );
    }

    /// The host writes its sensor telemetry beside the RAW and A1 moves the
    /// recording somewhere else, so the conditions a run was taken under used
    /// to be separated from the run itself at the first gather. It has to
    /// arrive in the measurement folder under the measurement's own name.
    #[test]
    fn the_sensor_readout_lands_in_the_measurement_folder_under_the_run_name() {
        let capture = temp_folder("sensor-capture");
        std::fs::create_dir_all(&capture).expect("capture dir");
        let raw = capture.join("host-capture.raw");
        std::fs::write(&raw, b"raw").expect("raw");
        std::fs::write(
            capture.join("host-capture.sensor-monitoring.csv"),
            "schema_version,sample_id,poll_kind,host_elapsed_start_us,host_elapsed_end_us,\
raw_data_offset_before_bytes,raw_data_offset_after_bytes,illumination_lux,temperature_c,\
pixel_dead_time_us,bias_diff_on_code,bias_diff_off_code,bias_fo_code,bias_hpf_code,\
bias_refr_code,status,error\n\
1,1,full,1000,1200,0,0,140.0,41.5,12.7,10,20,30,40,50,ok,\n\
1,2,fast,2000,2200,0,0,,,12.8,,,,,,ok,\n",
        )
        .expect("telemetry");

        let output = temp_folder("sensor-output");
        let mut plugin = StageAA1Plugin {
            output_folder: output.display().to_string(),
            ..StageAA1Plugin::default()
        };
        plugin.recording.folder = output.display().to_string();
        plugin.recording.id = "A1-sensor".into();
        plugin.recording.stem = "A1-sensor_20260731-120000".into();
        plugin.recording.cam_finalized_path = Some(raw.display().to_string());

        plugin.gather_into_measurement_folder();

        let written = plugin
            .recording
            .sensor_readout_path
            .as_deref()
            .expect("a sensor readout was written");
        assert!(
            written.ends_with("A1-sensor_20260731-120000.sensor.json"),
            "{written}"
        );
        assert!(
            Path::new(written).starts_with(output.join("A1-sensor")),
            "{written} is outside the measurement folder"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(written).expect("read")).expect("JSON");
        assert_eq!(parsed["measurement_id"], "A1-sensor");
        assert_eq!(parsed["channels"]["pixel_dead_time_us"]["value"][1], 12.8);
        assert_eq!(parsed["channels"]["temperature_c"]["value"][0], 41.5);
        // The wide original does not stay behind in the capture folder.
        assert!(!capture.join("host-capture.sensor-monitoring.csv").exists());
        assert!(
            !plugin.last_run_had_no_readout,
            "a run that did write a readout must not warn about one"
        );

        let _ = std::fs::remove_dir_all(&capture);
        let _ = std::fs::remove_dir_all(&output);
    }

    /// The host's telemetry companion is governed by its own **Record sensor
    /// monitoring** switch, which is off by default and which A1 cannot ask
    /// about. A survey that recorded forty runs and kept the bench conditions
    /// of none of them used to say nothing at all — the absence surfaced in
    /// the analysis, months later.
    #[test]
    fn a_run_with_no_host_telemetry_says_so_in_the_panel() {
        let capture = temp_folder("sensor-off-capture");
        std::fs::create_dir_all(&capture).expect("capture dir");
        let raw = capture.join("host-capture.raw");
        std::fs::write(&raw, b"raw").expect("raw");
        // No `.sensor-monitoring.csv` beside it: the host switch was off.

        let output = temp_folder("sensor-off-output");
        let mut plugin = plugin_with_markers();
        plugin.sensor = Some(SensorMonitoringV1 {
            temperature_c: Some(21.5),
            pixel_dead_time_us: Some(18.1),
            illumination_lux: Some(0.07),
            ..SensorMonitoringV1::default()
        });
        plugin.output_folder = output.display().to_string();
        plugin.recording.folder = output.display().to_string();
        plugin.recording.id = "A1-sensor-off".into();
        plugin.recording.stem = "A1-sensor-off_20260803-120000".into();
        plugin.recording.cam_finalized_path = Some(raw.display().to_string());

        plugin.gather_into_measurement_folder();

        assert!(
            plugin.recording.sensor_readout_path.is_none(),
            "there was no telemetry to compact"
        );
        let panel = plugin
            .status_entries()
            .into_iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            panel.contains("Record sensor monitoring"),
            "the panel does not name the switch that governs the readout:\n{panel}"
        );

        let _ = std::fs::remove_dir_all(&capture);
        let _ = std::fs::remove_dir_all(&output);
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
        // Provenance of `a` is unconditional: offline analysis must never have
        // to guess whether a run's depth was measured or merely commanded.
        assert!(text.contains("depth_a_source = \"photodiode_measured\""));
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

    /// A connected modulation plugin with no drive applied yet must never be
    /// reported as disconnected.
    ///
    /// The frequency comes from the phase-0 trigger markers or from the
    /// *acknowledged* drive; neither is the connection state, which is a
    /// separate check. Conflating them told operators to plug in a bench that
    /// was already plugged in.
    #[test]
    fn a_missing_frequency_is_not_reported_as_a_disconnected_plugin() {
        let mut plugin = StageAA1Plugin {
            // Connected, but nothing applied yet, and no markers (Live off).
            modulation: Some(connected_modulation()),
            photodiode: Some(fresh_photodiode_summary()),
            frame_width: 10,
            frame_height: 1,
            ..StageAA1Plugin::default()
        };
        assert!(plugin.modulation_connected());
        assert!(plugin.frequency_hz().is_none());

        let blocker = plugin.frequency_blocker().expect("a reason");
        assert!(
            blocker.contains("has not applied a drive yet"),
            "an armed-nothing bench must be named as such: {blocker}"
        );
        assert!(
            !blocker.contains("connect"),
            "a connected plugin must not be reported as needing connecting: {blocker}"
        );

        // A genuinely absent owner still says so.
        plugin.modulation = None;
        let absent = plugin.frequency_blocker().expect("a reason");
        assert!(absent.contains("not reporting status"), "{absent}");
    }

    /// One fact, one line. A missing frequency used to be stated three times.
    #[test]
    fn the_status_panel_states_a_missing_frequency_exactly_once() {
        let plugin = StageAA1Plugin {
            modulation: Some(connected_modulation()),
            photodiode: Some(fresh_photodiode_summary()),
            frame_width: 10,
            frame_height: 1,
            ..StageAA1Plugin::default()
        };
        let lines: Vec<String> = plugin
            .status_entries()
            .into_iter()
            .map(|entry| match entry {
                StatusEntry::Text(text) => text,
                StatusEntry::LabeledValue { label, value, .. } => format!("{label}: {value}"),
                _ => String::new(),
            })
            .collect();

        let explaining = lines
            .iter()
            .filter(|line| line.contains("has not applied a drive yet"))
            .count();
        assert_eq!(
            explaining, 1,
            "the cause belongs on one line, not three: {lines:#?}"
        );
        // The a₀ line points at that line instead of restating it.
        assert!(
            lines
                .iter()
                .any(|line| line.contains("waiting for a frequency")),
            "{lines:#?}"
        );
        // Nothing to say about the response curve at rest.
        assert!(
            !lines.iter().any(|line| line.starts_with("Response curve")),
            "an empty response curve must not take a line: {lines:#?}"
        );
    }

    /// An output folder is the only thing an operator must type before they can
    /// record. The measurement id names a folder and the flux point id is
    /// provenance; neither has ever been a reason to refuse the run, and every
    /// fixture in this file used to pre-fill both, which is how the refusal
    /// survived. Nothing here sets them.
    #[test]
    fn recording_needs_only_an_output_folder_not_the_optional_ids() {
        let mut plugin = StageAA1Plugin {
            output_folder: "/tmp/a1-no-ids".into(),
            measurement_id: String::new(),
            duration_s: 10,
            pending_role: Some(RecRole::Normal),
            photodiode: Some(ready_photodiode()),
            ..StageAA1Plugin::default()
        };
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert_eq!(
            plugin.recording.phase,
            RecPhase::StartingCamera,
            "blank ids must not refuse the recording; message={}",
            plugin.message
        );
        // The generated id is written back, so the panel shows what was used
        // rather than filing the run under a name the operator cannot see.
        assert!(
            !plugin.measurement_id.trim().is_empty(),
            "a generated id must land in the field the operator reads"
        );
        assert_eq!(plugin.recording.id, sanitize_stem(&plugin.measurement_id));
        assert!(plugin.recording.stem.starts_with(&plugin.recording.id));
    }

    /// An operator id that is present is kept exactly as it was.
    #[test]
    fn a_typed_measurement_id_is_never_replaced_by_a_generated_one() {
        let mut plugin = StageAA1Plugin {
            output_folder: "/tmp/a1-typed-id".into(),
            measurement_id: "row-7".into(),
            pending_role: Some(RecRole::Normal),
            photodiode: Some(ready_photodiode()),
            ..StageAA1Plugin::default()
        };
        let mut sink = ControlSink::default();

        control_tick(&mut plugin, PluginControlInbox::default(), &mut sink);

        assert_eq!(plugin.measurement_id, "row-7");
        assert_eq!(plugin.recording.id, "row-7");
    }

    /// The whole unattended ladder, with nothing typed in but the folder.
    ///
    /// Each point ends in a recording, and `begin_recording` used to refuse a
    /// blank id — three stages after the ladder had already taken the lease and
    /// moved the drive. The panel showed the ladder running and the recording
    /// idle, and every point was skipped.
    #[test]
    fn the_frequency_sweep_runs_with_no_ids_typed_in() {
        let folder = temp_folder("fsweep-no-ids");
        let mut plugin = plugin_locking(0.6, &folder);
        plugin.measurement_id = String::new();
        plugin.a0_target = 0.5;
        plugin.min_f = 100.0;
        plugin.max_f = 1_000.0;
        plugin.freq_count = 2;
        plugin.freq_order = FreqOrder::Ascending;
        photodiode_window(&mut plugin, 0.02);
        plugin.freq_sweep_pending = Some(FreqSweepMode::A0Point);
        let mut sink = ControlSink::default();

        let recorded = run_freq_sweep_to_completion(&mut plugin, &mut sink, 0.6, 4_000);

        assert_eq!(
            recorded.len(),
            2,
            "every ladder point must record; message: {}",
            plugin.message
        );
        assert!(plugin.message.contains("2/2 points recorded"));
        assert!(!plugin.measurement_id.trim().is_empty());
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// The a₀ field is a drag control with a 0.01 step. Comparing it to the
    /// lock's target on exact equality meant one stray pixel of drag disarmed a
    /// lock that had just converged, and the panel then asked for the very thing
    /// the operator had done. The operator's own tolerance is the right band.
    #[test]
    fn nudging_a0_inside_its_tolerance_keeps_the_lock_armed() {
        let mut plugin = plugin_with_markers();
        plugin.a0_target = 0.5;
        plugin.a0_tolerance = 0.02;
        plugin.a0_locks.push(A0LockPoint {
            frequency_hz: plugin.frequency_hz().expect("frequency"),
            target_a: 0.5,
            commanded_a: 0.83,
            measured_a: 0.5,
            trials: 2,
            converged: true,
            locked_at_unix_ms: now_unix_ms(),
            low_clip_fraction: None,
            high_clip_fraction: None,
            depth_source: DepthSource::Photodiode,
        });
        assert!(plugin.armed_lock().is_some(), "the fresh lock must arm");

        plugin.a0_target = 0.51;
        assert!(
            plugin.armed_lock().is_some(),
            "a nudge inside the tolerance must not disarm the lock"
        );

        // Beyond the tolerance it really is a different target, and the refusal
        // says so instead of asking for a Find a₀ that was already done.
        plugin.a0_target = 0.70;
        assert!(plugin.armed_lock().is_none());
        let blocker = plugin.armed_lock_blocker().expect("a reason");
        assert!(
            blocker.contains("0.500") && blocker.contains("0.700"),
            "the refusal must name both targets: {blocker}"
        );
    }

    /// Three different causes used to share one sentence telling the operator to
    /// press Find a₀ — which only fixes the first of them.
    #[test]
    fn a_lock_that_cannot_arm_names_which_of_the_three_causes_it_is() {
        let mut plugin = plugin_with_markers();
        plugin.a0_target = 0.5;
        plugin.a0_tolerance = 0.02;
        let hz = plugin.frequency_hz().expect("frequency");

        let no_lock = plugin.armed_lock_blocker().expect("a reason");
        assert!(
            no_lock.contains("press Find a₀"),
            "with no lock at all, pressing Find a₀ is the fix: {no_lock}"
        );

        plugin.a0_locks.push(A0LockPoint {
            frequency_hz: hz,
            target_a: 0.5,
            commanded_a: 6.0,
            measured_a: 0.31,
            trials: 8,
            converged: false,
            locked_at_unix_ms: now_unix_ms(),
            low_clip_fraction: None,
            high_clip_fraction: None,
            depth_source: DepthSource::Photodiode,
        });
        let not_converged = plugin.armed_lock_blocker().expect("a reason");
        assert!(
            not_converged.contains("0.310") && not_converged.contains("tolerance"),
            "a lock that stopped short must report where it stopped: {not_converged}"
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
            photodiode: Some(fresh_photodiode_summary()),
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
