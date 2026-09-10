//! Live A4 threshold-survey runner.
//!
//! A4 has one job. At a **fixed optical condition** it walks a protocol of
//! `(diff_on, diff_off)` bias pairs, and for each one it:
//!
//! 1. sets the two biases by cloning the camera configuration the host
//!    confirmed when the run opened its session, and changing only `diff_on`
//!    and `diff_off` — the host owns no A4-specific verb, so `fo`, `hpf`,
//!    `refr`, the ROI and the pixel mask stay frozen because A4 copies them
//!    forward unchanged (augur-rs ADR 037);
//! 2. **confirms against the sensor's own readback** that the absolute codes on
//!    the die are `factory_default + offset`, and retries the same point if they are
//!    not, or if the reading is missing or older than the change;
//! 3. settles, and refuses to record until a monitoring sample newer than the
//!    settle has arrived — a settle that produced no fresh telemetry is not a
//!    settle;
//! 4. records a RAW file for the row's duration, counting ON/OFF events as it
//!    goes;
//! 5. checks the receipt (size, hash, duration, clean finalization) and writes
//!    an A4 sidecar carrying the protocol row, the bias codes, the bench
//!    conditions and the QC verdict.
//!
//! Afterwards — on completion, on Stop, and on any abort — the biases the bench
//! was on before the survey are put back.
//!
//! The built-in bright reference leases the existing modulation and PD owners
//! for constant illumination and paired recordings. Legacy threshold surveys
//! retain externally controlled optics and explicit optical pauses.
//!
//! **What is a hard refusal and what is only a flag** is a deliberate split.
//! The sensor state that makes a threshold number mean something at all — the
//! event filters being off, the bias codes being confirmed, the file being
//! whole — is a gate. The bench *stability* limits (temperature drift,
//! illumination drift, event rate) are flags: the point is recorded, kept, and
//! marked, because whether a 2 °C drift invalidated it is a judgement to make
//! later with the file in hand, and a runner that discarded the point would
//! have thrown away the evidence for making it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use augur_plugin_api::{
    export_plugin, CameraConfigurationSnapshotV1, CameraConfigurationSourceV1, EventFiltersV1,
    EventStoreHandle, GlobalSettings, HostCommand, HostCommandOutcome, HostCommandReply,
    HostCommandRequest, HostContext, HostDatasetDescriptor, HostDatasetKind, HostOutput,
    HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry, PathDialogKind, Plugin,
    PluginCapabilities, PluginControlContext, PluginControlInbox, PluginDiscontinuity, PluginFrame,
    PluginInput, PluginRuntimeRole, PluginServiceRequest, RoiV1, SensorBiasReadbackV1,
    SensorMonitoringV1, SettingItem, SettingKind, SettingsSchema, SettingsSection, StatusEntry,
    TableColumn, TableColumnData, TableColumnValues, TableDatasetV1, TableSchema, TableValueType,
    CTX_GLOBAL_SETTINGS, CTX_SENSOR_MONITORING,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use stage_a_plugin_contract::telemetry;

use crate::protocol::{self, A4Point, Protocol};
use crate::qc::{self, Drift, Endpoints, QcStatus, RateSummary};
use crate::sidecar;

const STATUS_DATASET_ID: &str = "stage-a-a4.status";
const STATUS_VIEW_ID: &str = "stage-a-a4.status.view";
const POINTS_DATASET_ID: &str = "stage-a-a4.points";
const POINTS_VIEW_ID: &str = "stage-a-a4.points.view";

const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long to wait for any single host command to answer. The bias command
/// waits on a sensor read the host caps at five seconds, so this has to be
/// comfortably longer or a slow-but-working readback would look like a hang.
const REPLY_TIMEOUT_MS: u64 = 20_000;

/// A confirming readback older than this is not evidence about the point being
/// recorded. The host already refuses to confirm with a reading taken before
/// the change; this is the plugin's own independent bound on how stale the
/// reading it *records as provenance* may be.
const MAX_READBACK_AGE_S: f64 = 2.0;

/// A recording shorter than this fraction of what was asked for is a truncated
/// file, not a short one. Below it the point is not counted as recorded.
const MIN_DURATION_FRACTION: f64 = 0.9;

/// Buttons cross the UI-mirror/live-worker boundary as a monotonic counter, not
/// as a bool.
///
/// The host runs two instances of every plugin: a UI mirror that renders the
/// panel, and the live worker that actually runs the survey. A click arrives as
/// `true` on the clicked instance, while the other only ever sees the snapshot
/// value from `get_setting` — so a bool would either be missed or replayed
/// forever. A counter advance is one press edge, and the first counter a fresh
/// instance sees is adopted silently so a reloaded worker does not replay old
/// presses.
#[derive(Debug, Default, Clone, Copy)]
struct PressLatch {
    counter: u64,
    seen: Option<u64>,
}

impl PressLatch {
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

/// The two bias offsets A4 varies, around the sensor's per-unit factory trim.
///
/// Plugin-local bookkeeping: the host contract carries all five biases, and A4
/// deliberately reads and writes only these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct BiasOffsets {
    diff_on: i32,
    diff_off: i32,
}

/// Where the run is in the current point's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunPhase {
    /// `ApplyCameraConfiguration { Current }` sent; waiting for the host to
    /// confirm the configuration the survey will clone for every point.
    OpeningSession,
    /// Stopped before a point that needs a filter change or a dark cap.
    PausedForOperator,
    /// A point's configuration sent; waiting for the host's readback
    /// confirmation.
    ApplyingBiases,
    /// Biases confirmed; waiting out `settle_s` and for fresh telemetry.
    Settling,
    /// `StartRecording` sent; waiting for the host to acknowledge.
    StartingRecording,
    /// RAW is being written and events are being counted.
    Recording,
    /// `StopRecording` sent; waiting for the finalize receipt.
    StoppingRecording,
    /// Every point is done; putting the operator's biases back.
    RestoringBiases,
    RecoveryPaused,
    PreparingDevices,
    StartingPd,
    FinalizingPd,
    CleaningDevices,
}

/// How one point ended.
#[derive(Debug, Clone, PartialEq)]
enum PointOutcome {
    Recorded,
    /// Skipped or failed, with the reason in the operator's own terms.
    Failed(String),
}

/// One executed point, kept for the status table and the run receipt.
#[derive(Debug, Clone)]
struct PointRecord {
    row: usize,
    label: String,
    diff_on: i64,
    diff_off: i64,
    repeat: (u32, u32),
    outcome: PointOutcome,
    codes: Option<(u8, u8)>,
    raw: Option<String>,
    rates: RateSummary,
    qc: QcStatus,
}

impl PointRecord {
    fn status_text(&self) -> String {
        match &self.outcome {
            PointOutcome::Recorded => "recorded".into(),
            PointOutcome::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// State for the point currently in flight.
#[derive(Debug, Default)]
struct PointState {
    stem: String,
    /// Set once the host acknowledges the start.
    raw_path: Option<String>,
    finalized_path: Option<String>,
    size: Option<u64>,
    sha256: Option<String>,
    recorded_duration_s: Option<f64>,
    complete: bool,
    incomplete_reason: Option<String>,
    /// The confirmed readback for this point, and how stale it was.
    readback: Option<SensorBiasReadbackV1>,
    readback_age_s: f64,
    applied: BiasOffsets,
    /// Bench conditions at the two ends of the recording.
    temperature: Endpoints,
    illumination: Endpoints,
    pixel_dead_time_us: Option<f32>,
    sensor_age_s: Option<f64>,
    rates: RateSummary,
    /// Where event counting has consumed the stream up to.
    counted_to_us: Option<u64>,
    started_unix_ms: u64,
    settle_until_ms: u64,
    /// Latest sensor reading the plugin saw when the settle began; the settle
    /// is not over until a *newer* one has arrived.
    settle_started_ms: u64,
    saw_fresh_sensor: bool,
}

/// An in-flight survey.
#[derive(Debug)]
struct Run {
    plan: Protocol,
    protocol_path: String,
    protocol_sha256: String,
    measurement_id: String,
    index: usize,
    phase: RunPhase,
    /// Request id currently awaited, and when it was sent.
    pending_request: Option<u64>,
    last_activity_ms: u64,
    stop_requested: bool,
    started_at_unix_ms: u64,
    /// Set by a reply handler that has decided this point cannot be recorded.
    /// Consumed by `drive`, which owns advancing the run — a reply arriving
    /// mid-tick must not start the next point before this one is filed.
    pending_skip: Option<String>,
    point: PointState,
    records: Vec<PointRecord>,
    /// The offsets the bench was on before the survey started.
    original: Option<BiasOffsets>,
    /// The configuration the host confirmed when this run opened its session.
    /// Every point is this snapshot with two fields changed, which is what
    /// keeps `fo`, `hpf`, `refr`, the ROI and the mask frozen across the sweep.
    camera: Option<CameraConfigurationSnapshotV1>,
    biases_restored: bool,
    retry_count: u32,
    camera_may_record: bool,
    device_failure: Option<String>,
}

impl Run {
    fn point(&self) -> Option<&A4Point> {
        self.plan.points.get(self.index)
    }
}

pub struct StageAA4Plugin {
    devices: crate::devices::Devices,
    enabled: bool,
    runtime_role: PluginRuntimeRole,
    generation: u64,

    output_folder: String,
    measurement_id: String,
    protocol_path: String,

    press_start: PressLatch,
    press_stop: PressLatch,
    press_continue: PressLatch,
    press_restore: PressLatch,
    start_pending: bool,
    stop_pending: bool,
    continue_pending: bool,
    restore_pending: bool,

    host_roi: Option<RoiV1>,
    sensor_size: (u16, u16),
    masked_pixels: usize,
    event_filters: Option<EventFiltersV1>,
    sensor: Option<SensorMonitoringV1>,
    /// Bumped every time a fresh monitoring sample lands, so the settle gate
    /// can tell "a new reading arrived" from "the same one is still there".
    sensor_seq: u64,

    run: Option<Run>,
    request_seq: u64,
    message: String,
    /// The offsets the most recent survey found the bench on, kept after the
    /// run ends so Restore still has something to put back. A run that died
    /// with the host — a crash, a reload — leaves the sensor on whatever
    /// threshold it was last set to, and this is the only record of where it
    /// started.
    last_original: Option<BiasOffsets>,
    /// A standalone restore, outside a run, and the request it is waiting on.
    restore_request: Option<u64>,
}

impl Default for StageAA4Plugin {
    fn default() -> Self {
        Self {
            devices: crate::devices::Devices::default(),
            enabled: true,
            runtime_role: PluginRuntimeRole::LiveWorker,
            generation: 1,
            output_folder: String::new(),
            measurement_id: String::new(),
            protocol_path: String::new(),
            press_start: PressLatch::default(),
            press_stop: PressLatch::default(),
            press_continue: PressLatch::default(),
            press_restore: PressLatch::default(),
            start_pending: false,
            stop_pending: false,
            continue_pending: false,
            restore_pending: false,
            host_roi: None,
            sensor_size: (1280, 720),
            masked_pixels: 0,
            event_filters: None,
            sensor: None,
            sensor_seq: 0,
            run: None,
            request_seq: 0,
            message:
                "Press Run for the bright reference; the PD output folder is reused when available"
                    .into(),
            last_original: None,
            restore_request: None,
        }
    }
}

/// The control surface the runner drives. Abstracted so the state machine can
/// be tested without a host.
trait HostControl {
    fn request_host(&mut self, request: &HostCommandRequest);
    fn request_service(&mut self, _request: &PluginServiceRequest) {}
}

impl HostControl for PluginControlContext<'_> {
    fn request_service(&mut self, request: &PluginServiceRequest) {
        let _ = PluginControlContext::request_service(self, request);
    }
    fn request_host(&mut self, request: &HostCommandRequest) {
        // Fully qualified: the trait method and the inherent one share a name,
        // so `self.request_host(..)` would resolve back to this one.
        let _ = PluginControlContext::request_host(self, request);
    }
}

impl StageAA4Plugin {
    fn bump(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn note(&mut self, message: impl Into<String>) {
        self.message = message.into();
        self.bump();
    }

    fn next_request_id(&mut self) -> u64 {
        self.request_seq += 1;
        self.request_seq
    }

    /// The offsets the bench is on right now, derived from the sensor's own
    /// readback: the configured offset is `current - factory_default`.
    ///
    /// This is the only way to learn them. The panel value a plugin could read
    /// belongs to the host settings UI, not to this plugin, and asking the
    /// sensor is the same source the survey confirms every point against.
    fn live_offsets(&self) -> Option<BiasOffsets> {
        let codes = self.sensor?.bias_codes?;
        Some(BiasOffsets {
            diff_on: codes.current.diff_on as i32 - codes.factory_default.diff_on as i32,
            diff_off: codes.current.diff_off as i32 - codes.factory_default.diff_off as i32,
        })
    }

    /// Why a survey must not start right now, phrased as the action that fixes
    /// it. `None` means every gate is satisfied.
    ///
    /// Each of these is checked *before* the first bias moves, because the
    /// whole point of a protocol is that it runs unattended: a file that cannot
    /// work should say so on the button press.
    fn start_blocker(&self) -> Option<String> {
        if self.run.is_some() {
            return Some("A protocol is already running — press Stop to end it".into());
        }
        if self.output_folder.trim().is_empty() {
            return Some("Pick an output folder first — that is where the files go".into());
        }
        if !Path::new(self.output_folder.trim()).is_absolute() {
            return Some("Choose an absolute output folder".into());
        }
        if self.event_filters.is_none() {
            return Some(
                "Camera filter state is unavailable; wait for confirmed camera settings".into(),
            );
        }
        if let Some(filters) = self.event_filters {
            let mut on = Vec::new();
            if filters.stc_enabled {
                on.push("STC");
            }
            if filters.trail_enabled {
                on.push("Trail");
            }
            if filters.erc_enabled {
                on.push("ERC");
            }
            if !on.is_empty() {
                // These discard events before they are streamed, which is
                // exactly the quantity a threshold survey counts.
                return Some(format!(
                    "Turn {} off in the camera settings — a threshold survey counts events, and \
                     {} drops some before they are streamed",
                    on.join(" and "),
                    if on.len() == 1 { "it" } else { "they" }
                ));
            }
        }
        // Without a readback the method is unverifiable: every point would
        // record biases nobody can show were live. Refuse rather than run a
        // survey whose central claim cannot be checked.
        if self.sensor.and_then(|sensor| sensor.bias_codes).is_none() {
            return Some(
                "The sensor is not reporting its bias codes — A4 confirms every point against \
                 that readback, so it will not run without one. Start Preview on a camera with \
                 a monitoring block."
                    .into(),
            );
        }
        None
    }

    /// Load, validate and start the protocol named in the settings.
    fn begin_run(&mut self, context: &mut impl HostControl) {
        if let Some(blocker) = self.start_blocker() {
            self.note(blocker);
            return;
        }
        let path = if self.protocol_path.trim().is_empty() {
            "a4_bright_reference.toml".to_owned()
        } else {
            self.protocol_path.trim().to_owned()
        };
        let text = match if self.protocol_path.trim().is_empty() {
            Ok(include_str!("../protocols/a4_bright_reference.toml").to_owned())
        } else {
            std::fs::read_to_string(&path)
        } {
            Ok(text) => text,
            Err(error) => {
                self.note(format!("Cannot read {path}: {error}"));
                return;
            }
        };
        let plan = match protocol::parse_file(&path, &text) {
            Ok(plan) => plan,
            Err(error) => {
                self.note(format!("Protocol rejected — {error}"));
                return;
            }
        };

        if plan.preserve_current {
            if let Some(reason) = self.devices.blocker(now_unix_ms()) {
                self.note(reason);
                return;
            }
        }
        let mut measurement_id = self.ensure_measurement_id();
        let mut dir = Path::new(self.output_folder.trim()).join(&measurement_id);
        if dir.join("requested-protocol.txt").exists() {
            let base = generate_measurement_id();
            let mut attempt = 1_u64;
            loop {
                measurement_id = format!("{base}-{attempt}");
                dir = Path::new(self.output_folder.trim()).join(&measurement_id);
                if !dir.exists() {
                    break;
                }
                attempt += 1;
            }
            self.measurement_id = measurement_id.clone();
        }
        if let Err(error) = std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join("requested-protocol.txt"), &text))
        {
            self.note(format!("Cannot create measurement files: {error}"));
            return;
        }
        let now_ms = now_unix_ms();
        let (on_axis, off_axis) = plan.axis_counts();
        let total = plan.points.len();
        let minutes = plan.total_seconds() / 60.0;
        let pauses = if plan.has_pauses() {
            " — it has operator pauses, so it cannot be left alone"
        } else {
            ""
        };
        self.message = format!(
            "Protocol '{}': {total} recordings ({on_axis} × diff_on, {off_axis} × diff_off), \
             about {minutes:.0} min of bench time{pauses}",
            plan.name
        );

        // Captured from the sensor before anything moves, and kept after the
        // run ends so Restore can still put the bench back.
        let original = self.live_offsets();
        self.last_original = original.or(self.last_original);

        self.run = Some(Run {
            plan,
            protocol_sha256: sha256_hex(text.as_bytes()),
            protocol_path: path,
            measurement_id,
            index: 0,
            phase: RunPhase::OpeningSession,
            pending_request: None,
            last_activity_ms: now_ms,
            stop_requested: false,
            started_at_unix_ms: now_ms,
            pending_skip: None,
            point: PointState::default(),
            records: Vec::new(),
            original,
            camera: None,
            biases_restored: false,
            retry_count: 0,
            camera_may_record: false,
            device_failure: None,
        });
        self.open_camera_session(context);
        self.bump();
    }

    /// Ask the host to preserve and confirm the configuration the bench is on.
    ///
    /// This is the survey's baseline: the host keeps the pre-run state for the
    /// closing restore, and the confirmed snapshot it answers with is what
    /// every point clones. Nothing is recorded until it arrives, so a survey
    /// can never sweep biases on top of a configuration nobody confirmed.
    fn open_camera_session(&mut self, context: &mut impl HostControl) {
        let request_id = self.next_request_id();
        context.request_host(&HostCommandRequest {
            request_id,
            command: HostCommand::ApplyCameraConfiguration {
                configuration: CameraConfigurationSourceV1::Current,
            },
        });
        if let Some(run) = self.run.as_mut() {
            run.phase = RunPhase::OpeningSession;
            run.pending_request = Some(request_id);
            run.last_activity_ms = now_unix_ms();
        }
    }

    /// Begin the point at `index`: pause for the operator if the row asks, else
    /// send its biases.
    fn enter_point(&mut self, context: &mut impl HostControl) {
        let Some(run) = self.run.as_ref() else {
            return;
        };
        let Some(point) = run.point().cloned() else {
            self.finish_run(context);
            return;
        };
        let (index, total) = (run.index, run.plan.points.len());
        if let Some(run) = self.run.as_mut() {
            run.point = PointState::default();
            run.last_activity_ms = now_unix_ms();
        }
        if point.pause_before {
            if let Some(run) = self.run.as_mut() {
                run.phase = RunPhase::PausedForOperator;
            }
            self.note(format!(
                "Paused before {}/{total} [{}]: set up '{}', then press Continue",
                index + 1,
                point.label,
                if point.optical_state.is_empty() {
                    "the next optical condition"
                } else {
                    point.optical_state.as_str()
                }
            ));
            return;
        }
        self.send_biases(context);
    }

    /// Ask the host to program this point's two biases.
    ///
    /// The request carries a complete configuration because that is the only
    /// contract the host offers, but A4 builds it by cloning the snapshot the
    /// session confirmed and changing exactly two fields. Everything the
    /// threshold measurement depends on staying still is therefore carried
    /// forward byte for byte from the baseline.
    fn send_biases(&mut self, context: &mut impl HostControl) {
        let Some(point) = self.run.as_ref().and_then(|run| run.point().cloned()) else {
            return;
        };
        let (index, total) = self
            .run
            .as_ref()
            .map(|run| (run.index, run.plan.points.len()))
            .unwrap_or((0, 0));
        let Some(mut snapshot) = self.run.as_ref().and_then(|run| run.camera.clone()) else {
            self.skip_current(
                "the host never confirmed a camera configuration for this survey, so there is \
                 no baseline to change two biases against",
            );
            return;
        };
        snapshot.biases.diff_on = point.diff_on as i32;
        snapshot.biases.diff_off = point.diff_off as i32;
        let request_id = self.next_request_id();
        context.request_host(&HostCommandRequest {
            request_id,
            command: HostCommand::ApplyCameraConfiguration {
                configuration: CameraConfigurationSourceV1::Snapshot { snapshot },
            },
        });
        if let Some(run) = self.run.as_mut() {
            run.phase = RunPhase::ApplyingBiases;
            run.pending_request = Some(request_id);
            run.last_activity_ms = now_unix_ms();
        }
        self.note(format!(
            "Point {}/{total} [{}]: setting diff_on={}, diff_off={}…",
            index + 1,
            point.label,
            point.diff_on,
            point.diff_off
        ));
    }

    /// Start the RAW recording for the settled point.
    fn start_recording(&mut self, context: &mut impl HostControl) {
        let Some(run) = self.run.as_ref() else {
            return;
        };
        let Some(point) = run.point().cloned() else {
            return;
        };
        let (index, total) = (run.index, run.plan.points.len());
        let id = run.measurement_id.clone();
        let stem = format!(
            "{id}_p{:04}_a{:02}_{}_{}",
            index + 1,
            run.retry_count + 1,
            format_compact_utc(now_unix_ms() / 1_000),
            point.tag()
        );
        let metadata = self.recording_metadata(&point, index, total);
        let request_id = self.next_request_id();
        context.request_host(&HostCommandRequest {
            request_id,
            command: HostCommand::StartRecording {
                run_id: stem.clone(),
                base_path: format!("{id}/{stem}.raw"),
                root_dir: Some(self.output_folder.trim().to_owned()),
                metadata,
            },
        });
        if let Some(run) = self.run.as_mut() {
            run.camera_may_record = true;
            run.phase = RunPhase::StartingRecording;
            run.pending_request = Some(request_id);
            run.point.stem = stem;
            run.last_activity_ms = now_unix_ms();
        }
        self.note(format!(
            "Point {}/{total} [{}]: recording for {} s…",
            index + 1,
            point.label,
            point.duration_s
        ));
    }

    fn stop_recording(&mut self, context: &mut impl HostControl) {
        let request_id = self.next_request_id();
        context.request_host(&HostCommandRequest {
            request_id,
            command: HostCommand::StopRecording,
        });
        if let Some(run) = self.run.as_mut() {
            run.phase = RunPhase::StoppingRecording;
            run.pending_request = Some(request_id);
            run.last_activity_ms = now_unix_ms();
        }
    }

    /// Metadata the host writes into the recording's own description, so a RAW
    /// found on its own still says which point it is.
    fn recording_metadata(
        &self,
        point: &A4Point,
        index: usize,
        total: usize,
    ) -> BTreeMap<String, String> {
        let mut meta = BTreeMap::new();
        let run = self.run.as_ref();
        meta.insert(
            "a4_measurement_id".into(),
            run.map(|run| run.measurement_id.clone())
                .unwrap_or_default(),
        );
        meta.insert("a4_label".into(), point.label.clone());
        meta.insert("brightness_axis".into(), "camera_reported_lux".into());
        if self.devices.active {
            meta.insert("constant_mean_u".into(), "0.30".into());
            meta.insert(
                "external_attenuation".into(),
                "unchanged from preceding A1-A3; verify actual camera lux".into(),
            );
        }
        meta.insert("a4_diff_on".into(), point.diff_on.to_string());
        meta.insert("a4_diff_off".into(), point.diff_off.to_string());
        meta.insert("a4_duration_s".into(), point.duration_s.to_string());
        meta.insert(
            "a4_repeat".into(),
            format!("{}/{}", point.repeat.0, point.repeat.1),
        );
        if !point.optical_state.is_empty() {
            meta.insert("a4_optical_state".into(), point.optical_state.clone());
        }
        if !point.filter_id.is_empty() {
            meta.insert("a4_filter_id".into(), point.filter_id.clone());
        }
        if !point.flux_id.is_empty() {
            meta.insert("a4_flux_id".into(), point.flux_id.clone());
        }
        // Read by the host's crash breadcrumb, so a death during an unattended
        // survey is pinned to the point it was on.
        meta.insert("protocol_point_index".into(), (index + 1).to_string());
        meta.insert("protocol_point_total".into(), total.to_string());
        // The codes actually confirmed on the die for this point.
        if let Some(readback) = run.and_then(|run| run.point.readback) {
            meta.insert(
                "a4_code_diff_on".into(),
                readback.current.diff_on.to_string(),
            );
            meta.insert(
                "a4_code_diff_off".into(),
                readback.current.diff_off.to_string(),
            );
        }
        // Bench conditions, each only when the sensor actually reported it — an
        // absent reading must not arrive downstream as 0 °C or 0 lux.
        if let Some(sensor) = self.sensor {
            if let Some(celsius) = sensor.temperature_c {
                meta.insert("sensor_temperature_c".into(), format!("{celsius:.2}"));
            }
            if let Some(lux) = sensor.illumination_lux {
                meta.insert("sensor_illumination_lux".into(), format!("{lux:.3}"));
            }
            if let Some(dead_time) = sensor.pixel_dead_time_us {
                meta.insert(
                    "sensor_pixel_dead_time_us".into(),
                    format!("{dead_time:.3}"),
                );
            }
        }
        meta
    }

    fn ensure_measurement_id(&mut self) -> String {
        if self.measurement_id.trim().is_empty() {
            self.measurement_id = generate_measurement_id();
        }
        sanitize_stem(self.measurement_id.trim())
    }

    /// Retain the failed attempt and retry only this point when the camera is
    /// confirmed idle. Exhausted retries or uncertain capture state require recovery.
    fn fail_point(&mut self, context: &mut impl HostControl, reason: impl Into<String>) {
        let reason = reason.into();
        let Some(point) = self.run.as_ref().and_then(|run| run.point().cloned()) else {
            return;
        };
        let index = self.run.as_ref().map(|run| run.index).unwrap_or(0);
        self.record_point(&point, index, PointOutcome::Failed(reason.clone()));
        self.note(format!(
            "Point {} [{}] skipped: {reason}",
            index + 1,
            point.label
        ));
        let run = self.run.as_mut().expect("active run");
        run.pending_request = None;
        if run.camera_may_record {
            run.phase = RunPhase::RecoveryPaused;
            self.note(format!("Point {} failed: {reason}. Camera stop needs confirmation; press Continue to retry stop.", index + 1));
        } else if run.stop_requested {
            self.finish_run(context);
        } else if run.retry_count < 2 {
            run.retry_count += 1;
            run.point = PointState::default();
            self.send_biases(context);
        } else {
            run.phase = RunPhase::RecoveryPaused;
            self.note(format!("Point {} failed after three attempts: {reason}. Press Continue to retry this point, or Stop.", index + 1));
        }
    }

    /// File the point's outcome into the run's own record, and write its
    /// sidecar. Both happen for failures too — the record of a failed point is
    /// the reason the survey has a hole in it.
    fn record_point(&mut self, point: &A4Point, index: usize, mut outcome: PointOutcome) {
        if let Some(run) = self.run.as_mut() {
            if run.point.stem.is_empty() {
                run.point.stem = format!(
                    "{}_p{:04}_a{:02}_no_capture",
                    run.measurement_id,
                    index + 1,
                    run.retry_count + 1
                );
            }
        }
        let (rates, drift, status) = self.evaluate_point(point);
        let codes = self
            .run
            .as_ref()
            .and_then(|run| run.point.readback)
            .map(|readback| (readback.current.diff_on, readback.current.diff_off));
        // Gather before writing, so all evidence names the final paths.
        self.gather_point();
        let raw = self
            .run
            .as_ref()
            .and_then(|run| run.point.finalized_path.clone());
        if self.devices.active {
            if let (Some(dir), Some(run)) = (self.measurement_dir(), self.run.as_ref()) {
                let value = json!({"modulation": self.devices.modulation, "photodiode": self.devices.photodiode,
                    "receipts": self.devices.receipts, "error": run.device_failure,
                    "camera_snapshot": run.camera, "brightness_axis": "camera_reported_lux", "absolute_accuracy": "unmeasured"});
                if let Err(error) = std::fs::write(
                    dir.join(format!("{}.devices.json", run.point.stem)),
                    serde_json::to_vec_pretty(&value).unwrap(),
                ) {
                    outcome =
                        PointOutcome::Failed(format!("Device provenance write failed: {error}"));
                    if let Some(run) = self.run.as_mut() {
                        run.stop_requested = true;
                    }
                }
            }
        }
        if let Err(error) = self.write_sidecar(point, index, &outcome, &rates, &drift, &status) {
            outcome = PointOutcome::Failed(format!("Sidecar not saved: {error}"));
            self.message = format!("{}; sidecar not saved: {error}", self.message);
            if let Some(run) = self.run.as_mut() {
                run.stop_requested = true;
            }
        }

        if let (Some(dir), Some(run)) = (self.measurement_dir(), self.run.as_ref()) {
            let row = json!({"point":index+1,"attempt":run.retry_count+1,"label":point.label,
                "raw":raw,"outcome":match &outcome { PointOutcome::Recorded => "recorded", PointOutcome::Failed(_) => "failed" },
                "reason":match &outcome { PointOutcome::Recorded => None, PointOutcome::Failed(reason) => Some(reason) },
                "bias_snapshot":run.camera,"unix_ms":now_unix_ms()});
            let result = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("attempts.jsonl"))
                .and_then(|mut file| {
                    writeln!(file, "{row}")?;
                    file.sync_data()
                });
            if let Err(error) = result {
                outcome = PointOutcome::Failed(format!("Progress journal write failed: {error}"));
                if let Some(run) = self.run.as_mut() {
                    run.stop_requested = true;
                }
            }
        }
        if let Some(run) = self.run.as_mut() {
            run.records.push(PointRecord {
                row: index + 1,
                label: point.label.clone(),
                diff_on: point.diff_on,
                diff_off: point.diff_off,
                repeat: point.repeat,
                outcome,
                codes,
                raw,
                rates,
                qc: status,
            });
        }
    }

    fn evaluate_point(&self, point: &A4Point) -> (RateSummary, Drift, QcStatus) {
        let Some(run) = self.run.as_ref() else {
            return (
                RateSummary::default(),
                Drift::default(),
                QcStatus::NotEvaluated,
            );
        };
        let rates = run.point.rates;
        let drift = qc::drift(run.point.temperature, run.point.illumination);
        let status = qc::evaluate(&point.limits, &rates, &drift);
        (rates, drift, status)
    }

    /// Step to the next point, or finish the run.
    fn advance(&mut self, context: &mut impl HostControl) {
        let Some(run) = self.run.as_mut() else {
            return;
        };
        run.index += 1;
        run.retry_count = 0;
        run.last_activity_ms = now_unix_ms();
        let done = run.index >= run.plan.points.len() || run.stop_requested;
        if done {
            self.finish_run(context);
        } else {
            self.enter_point(context);
        }
    }

    /// Put the operator's biases back and end the run.
    ///
    /// The restore is a command like any other, so the run does not disappear
    /// until it is answered — a survey that vanished while the sensor was still
    /// on its last threshold would leave the bench silently misconfigured.
    fn finish_run(&mut self, context: &mut impl HostControl) {
        if self.devices.active {
            self.devices.release();
            if let Some(run) = self.run.as_mut() {
                run.phase = RunPhase::CleaningDevices;
            }
            return;
        }
        let Some(run) = self.run.as_ref() else {
            return;
        };
        // The host preserved the pre-run configuration when the session opened,
        // so the restore is its own verb rather than a bias change back — which
        // also puts back anything a point's snapshot carried along with the two
        // biases. Nothing to restore if the session never opened.
        if run.camera.is_some() && !run.biases_restored {
            let request_id = self.next_request_id();
            context.request_host(&HostCommandRequest {
                request_id,
                command: HostCommand::RestoreCameraConfiguration,
            });
            if let Some(run) = self.run.as_mut() {
                run.phase = RunPhase::RestoringBiases;
                run.pending_request = Some(request_id);
                run.last_activity_ms = now_unix_ms();
            }
        } else {
            self.close_run();
        }
    }

    /// Write the run receipt, report, and drop the run.
    fn close_run(&mut self) {
        let Some(run) = self.run.take() else {
            return;
        };
        let recorded = run
            .records
            .iter()
            .filter(|record| record.outcome == PointOutcome::Recorded)
            .count();
        let failed = run.records.len() - recorded;
        let flagged = run
            .records
            .iter()
            .filter(|record| record.qc.is_flagged())
            .count();
        let total = run.plan.points.len();
        let name = run.plan.name.clone();
        let stopped = run.stop_requested;

        let receipt = self.write_receipt(&run, recorded, failed, flagged);

        let mut message = format!(
            "Protocol '{name}' {}: {recorded}/{total} recorded",
            if stopped { "stopped" } else { "finished" }
        );
        if failed > 0 {
            // Name the reasons, not just the count: an unattended run's whole
            // report is this one line.
            let mut reasons: Vec<String> = run
                .records
                .iter()
                .filter_map(|record| match &record.outcome {
                    PointOutcome::Failed(reason) => Some(reason.clone()),
                    PointOutcome::Recorded => None,
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            reasons.truncate(3);
            message.push_str(&format!(" — {failed} skipped ({})", reasons.join("; ")));
        }
        if flagged > 0 {
            message.push_str(&format!(", {flagged} QC-flagged"));
        }
        message.push_str(if run.biases_restored {
            ". Biases restored."
        } else {
            ". Biases NOT restored — check the camera settings."
        });
        if let Err(error) = receipt {
            message.push_str(&format!(" Protocol receipt not saved: {error}"));
        }
        self.note(message);
    }

    // ---- artefacts ---------------------------------------------------------

    fn measurement_dir(&self) -> Option<PathBuf> {
        let run = self.run.as_ref()?;
        let folder = self.output_folder.trim();
        if folder.is_empty() {
            return None;
        }
        Some(Path::new(folder).join(&run.measurement_id))
    }

    /// Collect the finalized artefacts into `<output folder>/<measurement id>/`.
    ///
    /// The host resolves plugin recording paths below *its* output directory
    /// and rejects absolute ones, so without this a measurement is split across
    /// two unrelated folders. The RAW is closed and hashed by the time its
    /// receipt arrives, so moving it here is safe.
    fn gather_point(&mut self) {
        let Some(dir) = self.measurement_dir() else {
            return;
        };
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let raw = self
            .run
            .as_ref()
            .and_then(|run| run.point.finalized_path.clone());
        let Some(raw) = raw else {
            return;
        };
        if let Some(moved) = move_into(&dir, &raw) {
            if let Some(run) = self.run.as_mut() {
                if run.point.finalized_path.is_some() {
                    run.point.finalized_path = Some(moved.clone());
                }
                run.point.raw_path = Some(moved);
            }
        }
        // The host writes the camera's own bias/config sidecar as a sibling of
        // the RAW; it travels with it so the recording stays self-describing.
        if let Some(bias) = sibling_toml(&raw) {
            move_into(&dir, &bias);
        }
        self.gather_sensor_readout(&dir, &raw);
    }

    /// Export an additional compact telemetry view while retaining the original CSV.
    ///
    /// Best-effort throughout: a missing telemetry file is normal (a camera
    /// with no monitoring block, a host that did not poll) and must not cost
    /// the operator the point that just finished.
    fn gather_sensor_readout(&mut self, dir: &Path, raw: &str) {
        let Some(run) = self.run.as_ref() else {
            return;
        };
        let (id, stem) = (run.measurement_id.clone(), run.point.stem.clone());
        let source = Path::new(raw)
            .file_stem()
            .map(|file_stem| {
                Path::new(raw)
                    .parent()
                    .unwrap_or(Path::new("."))
                    .join(format!(
                        "{}.sensor-monitoring.csv",
                        file_stem.to_string_lossy()
                    ))
            })
            .filter(|path| path.exists());
        let Some(source) = source else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&source) else {
            return;
        };
        let readout = telemetry::parse_csv(&text);
        if readout.is_empty() {
            // An unsupported format is still original measurement evidence.
            return;
        }
        let destination = dir.join(format!("{stem}.sensor.json"));
        let json = readout.to_json(telemetry::SCHEMA_A4, &id, &stem);
        let _ = std::fs::write(&destination, json);
    }

    fn write_sidecar(
        &self,
        point: &A4Point,
        index: usize,
        outcome: &PointOutcome,
        rates: &RateSummary,
        drift: &Drift,
        status: &QcStatus,
    ) -> Result<String, String> {
        let run = self.run.as_ref().ok_or("no run")?;
        let dir = self.measurement_dir().ok_or("no output folder")?;
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
        let state = &run.point;
        let readback = state.readback.unwrap_or_default();
        let roi = self.host_roi.unwrap_or_default();
        let filters = self.event_filters.unwrap_or_default();
        // A stem is only assigned once a recording starts; a point that failed
        // before that still gets a sidecar, named after its protocol row.
        let stem = if state.stem.is_empty() {
            format!("{}_{}_unrecorded", run.measurement_id, point.tag())
        } else {
            state.stem.clone()
        };

        let doc = sidecar::SidecarDoc {
            schema: sidecar::SIDECAR_SCHEMA,
            measurement_id: run.measurement_id.clone(),
            recording: stem.clone(),
            recorded_at_utc: format_iso_utc(now_unix_ms() / 1_000),
            plugin_version: PLUGIN_VERSION,
            protocol: sidecar::ProtocolSection {
                name: run.plan.name.clone(),
                file: run.protocol_path.clone(),
                sha256: run.protocol_sha256.clone(),
                row: index + 1,
                rows_total: run.plan.points.len(),
                label: point.label.clone(),
                repeat: point.repeat.0,
                repeats: point.repeat.1,
                requested_duration_s: point.duration_s,
                requested_settle_s: point.settle_s,
            },
            bias: sidecar::BiasSection {
                requested_diff_on: point.diff_on,
                requested_diff_off: point.diff_off,
                applied_diff_on: state.applied.diff_on,
                applied_diff_off: state.applied.diff_off,
                code_diff_on: readback.current.diff_on,
                code_diff_off: readback.current.diff_off,
                factory_diff_on: readback.factory_default.diff_on,
                factory_diff_off: readback.factory_default.diff_off,
                code_fo: readback.current.fo,
                code_hpf: readback.current.hpf,
                code_refr: readback.current.refr,
                readback_age_s: state.readback_age_s,
                confirmed: state.readback.is_some(),
            },
            optics: sidecar::OpticsSection {
                optical_state: point.optical_state.clone(),
                filter_id: point.filter_id.clone(),
                flux_id: point.flux_id.clone(),
                paused_for_operator: point.pause_before,
            },
            sensor: sidecar::SensorSection {
                temperature_c_start: state.temperature.start,
                temperature_c_end: state.temperature.end,
                illumination_lux_start: state.illumination.start,
                illumination_lux_end: state.illumination.end,
                pixel_dead_time_us: state.pixel_dead_time_us,
                reading_age_s: state.sensor_age_s,
                illumination_note:
                    "Sensor lux is the die's own integrated reading, used here as a stability \
                     indicator only. It is not a calibrated optical power.",
            },
            filters: sidecar::FiltersSection {
                stc_enabled: filters.stc_enabled,
                trail_enabled: filters.trail_enabled,
                erc_enabled: filters.erc_enabled,
                erc_note: "This host has no event-rate controller, so ERC is off by construction \
                           rather than by configuration.",
            },
            camera: sidecar::CameraSection {
                roi_x: roi.x,
                roi_y: roi.y,
                roi_width: roi.width,
                roi_height: roi.height,
                masked_pixels: self.masked_pixels,
                sensor_width: self.sensor_size.0,
                sensor_height: self.sensor_size.1,
            },
            files: sidecar::FilesSection {
                raw: state.finalized_path.clone().or(state.raw_path.clone()),
                raw_size_bytes: state.size,
                raw_sha256: state.sha256.clone(),
                recorded_duration_s: state.recorded_duration_s,
                sensor_readout: (!state.stem.is_empty())
                    .then(|| dir.join(format!("{stem}.sensor.json")))
                    .filter(|path| path.exists())
                    .map(|path| path.display().to_string()),
                complete: *outcome == PointOutcome::Recorded,
                incomplete_reason: match outcome {
                    PointOutcome::Recorded => None,
                    PointOutcome::Failed(reason) => Some(reason.clone()),
                },
            },
            qc: sidecar::QcSection {
                status: status.as_str().to_owned(),
                flags: status.flags().to_vec(),
                on_events: rates.on_events,
                off_events: rates.off_events,
                total_events: rates.total_events(),
                counted_seconds: rates.seconds,
                on_rate_hz: rates.on_rate_hz(),
                off_rate_hz: rates.off_rate_hz(),
                total_rate_hz: rates.total_rate_hz(),
                on_fraction: rates.on_fraction(),
                temperature_drift_c: drift.temperature_c,
                illumination_drift_percent: drift.illumination_percent,
                limit_temperature_drift_c: point.limits.max_temperature_drift_c,
                limit_illumination_drift_percent: point.limits.max_illumination_drift_percent,
                limit_event_rate: point.limits.max_event_rate,
                rate_note: "Rates are counted from the frames this plugin observed, over \
                     counted_seconds. Compare that against recorded_duration_s for the coverage.",
            },
        };

        let path = dir.join(format!("{stem}.a4.toml"));
        let text = toml::to_string_pretty(&doc).map_err(|error| error.to_string())?;
        std::fs::write(&path, text).map_err(|error| error.to_string())?;
        Ok(path.display().to_string())
    }

    /// Copy the protocol into the measurement folder and write the receipt
    /// beside it, so the folder says which rows ran without anyone having to
    /// diff filenames against the source file.
    fn write_receipt(
        &self,
        run: &Run,
        recorded: usize,
        failed: usize,
        flagged: usize,
    ) -> Result<(), String> {
        let folder = self.output_folder.trim();
        if folder.is_empty() {
            return Err("no output folder".into());
        }
        let dir = Path::new(folder).join(&run.measurement_id);
        std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;

        // The copy travels with the data; the original stays where the
        // operator keeps it.
        let source = Path::new(&run.protocol_path);
        if let Some(name) = source.file_name() {
            let _ = std::fs::copy(dir.join("requested-protocol.txt"), dir.join(name));
        }

        let receipt = sidecar::ProtocolReceipt {
            schema: sidecar::RECEIPT_SCHEMA,
            measurement_id: run.measurement_id.clone(),
            protocol_name: run.plan.name.clone(),
            protocol_file: run.protocol_path.clone(),
            protocol_sha256: run.protocol_sha256.clone(),
            started_at_utc: format_iso_utc(run.started_at_unix_ms / 1_000),
            finished_at_utc: format_iso_utc(now_unix_ms() / 1_000),
            outcome: if run.stop_requested {
                "stopped".into()
            } else {
                "finished".into()
            },
            rows_total: run.plan.points.len(),
            rows_recorded: recorded,
            rows_failed: failed,
            rows_flagged: flagged,
            restored_diff_on: run.original.map(|original| original.diff_on),
            restored_diff_off: run.original.map(|original| original.diff_off),
            biases_restored: run.biases_restored,
            row: run
                .records
                .iter()
                .map(|record| sidecar::ReceiptRow {
                    row: record.row,
                    label: record.label.clone(),
                    diff_on: record.diff_on,
                    diff_off: record.diff_off,
                    repeat: record.repeat.0,
                    status: match &record.outcome {
                        PointOutcome::Recorded => "recorded".into(),
                        PointOutcome::Failed(_) => "failed".into(),
                    },
                    reason: match &record.outcome {
                        PointOutcome::Recorded => None,
                        PointOutcome::Failed(reason) => Some(reason.clone()),
                    },
                    raw: record.raw.clone(),
                    qc: record.qc.as_str().to_owned(),
                })
                .collect(),
        };
        let name = format!("{}.protocol-status.toml", run.measurement_id);
        let text = toml::to_string_pretty(&receipt).map_err(|error| error.to_string())?;
        std::fs::write(dir.join(name), text).map_err(|error| error.to_string())
    }

    // ---- replies -----------------------------------------------------------

    fn on_host_reply(&mut self, reply: &HostCommandReply) {
        // A standalone restore, pressed outside a run.
        if self.restore_request == Some(reply.request_id) {
            self.restore_request = None;
            self.message = match &reply.outcome {
                HostCommandOutcome::CameraConfigurationRestored { readback, .. } => format!(
                    "Biases restored — the sensor reports diff_on={}, diff_off={}",
                    readback.current.diff_on, readback.current.diff_off
                ),
                HostCommandOutcome::Rejected { code, message } => {
                    format!("Restore refused ({code}): {message}")
                }
                _ => "Restore answered with an unexpected receipt".into(),
            };
            self.bump();
            return;
        }

        let Some(run) = self.run.as_ref() else {
            return;
        };
        if run.pending_request != Some(reply.request_id) {
            return;
        }
        let phase = run.phase;
        if let Some(run) = self.run.as_mut() {
            run.pending_request = None;
            run.last_activity_ms = now_unix_ms();
        }
        match phase {
            RunPhase::OpeningSession => self.on_session_reply(&reply.outcome),
            RunPhase::ApplyingBiases => self.on_biases_reply(&reply.outcome),
            RunPhase::StartingRecording => self.on_start_reply(&reply.outcome),
            RunPhase::StoppingRecording => self.on_stop_reply(&reply.outcome),
            RunPhase::RestoringBiases => {
                if let Some(run) = self.run.as_mut() {
                    run.biases_restored = matches!(
                        reply.outcome,
                        HostCommandOutcome::CameraConfigurationRestored { .. }
                    );
                }
                if self.run.as_ref().is_some_and(|r| r.biases_restored) {
                    self.close_run();
                } else {
                    self.note("Camera restoration was not confirmed. Press Continue to retry restoration.");
                }
            }
            _ => {}
        }
    }

    /// The host confirmed the baseline configuration. Keep it as the snapshot
    /// every point clones, then begin the first point.
    ///
    /// A survey that cannot get this cannot run at all — unlike a single point,
    /// there is nothing to skip forward to — so a refusal ends the run instead
    /// of failing a point.
    fn on_session_reply(&mut self, outcome: &HostCommandOutcome) {
        match outcome {
            // Kept here and consumed by `drive`, which owns advancing the run.
            HostCommandOutcome::CameraConfigurationApplied {
                snapshot,
                readback_age_s,
                ..
            } => {
                let filters = &snapshot.digital_filter;
                let valid = readback_age_s.is_finite()
                    && *readback_age_s >= 0.0
                    && *readback_age_s <= MAX_READBACK_AGE_S
                    && !filters.stc_enabled
                    && !filters.trail_enabled
                    && filters.erc_enabled == Some(false)
                    && snapshot.global.record_sensor_telemetry;
                if let Some(run) = self.run.as_mut() {
                    if run.plan.preserve_current {
                        for point in &mut run.plan.points {
                            point.diff_on = i64::from(snapshot.biases.diff_on);
                            point.diff_off = i64::from(snapshot.biases.diff_off);
                        }
                    }
                    run.original = Some(BiasOffsets {
                        diff_on: snapshot.biases.diff_on,
                        diff_off: snapshot.biases.diff_off,
                    });
                    run.camera = Some(snapshot.clone());
                    if !valid {
                        run.stop_requested = true;
                        run.pending_skip = Some("Baseline needs fresh readback, confirmed filters off and sensor telemetry enabled".into());
                    }
                }
            }
            HostCommandOutcome::Rejected { code, message } => {
                self.note(format!(
                    "The host refused to confirm the camera configuration ({code}): {message}"
                ));
                self.close_run();
            }
            _ => {
                self.note("The host answered the configuration request with an unexpected receipt");
                self.close_run();
            }
        }
    }

    fn on_biases_reply(&mut self, outcome: &HostCommandOutcome) {
        match outcome {
            HostCommandOutcome::CameraConfigurationApplied {
                snapshot,
                readback,
                readback_age_s,
                ..
            } => {
                if let Some(run) = self.run.as_ref() {
                    if let (Some(mut expected), Some(point)) = (run.camera.clone(), run.point()) {
                        expected.biases.diff_on = point.diff_on as i32;
                        expected.biases.diff_off = point.diff_off as i32;
                        if expected != *snapshot {
                            self.skip_current(
                                "Camera snapshot differs from the requested complete configuration",
                            );
                            return;
                        }
                    }
                }
                let applied = BiasOffsets {
                    diff_on: snapshot.biases.diff_on,
                    diff_off: snapshot.biases.diff_off,
                };
                let point = self.run.as_ref().and_then(|run| run.point().cloned());
                let Some(point) = point else { return };
                // The host already confirmed the codes; A4 checks them again
                // against what *it* asked for. The two are the same check from
                // two sides, and a threshold point is worth the second look.
                let expected_on = expected_code(readback.factory_default.diff_on, point.diff_on);
                let expected_off = expected_code(readback.factory_default.diff_off, point.diff_off);
                if readback.current.diff_on != expected_on
                    || readback.current.diff_off != expected_off
                {
                    let reason = format!(
                        "the sensor reports diff_on={}/diff_off={} but the row asks for \
                         {expected_on}/{expected_off}",
                        readback.current.diff_on, readback.current.diff_off
                    );
                    self.skip_current(reason);
                    return;
                }
                if !readback_age_s.is_finite()
                    || *readback_age_s < 0.0
                    || *readback_age_s > MAX_READBACK_AGE_S
                {
                    self.skip_current(format!(
                        "the confirming bias reading was {readback_age_s:.1} s old, past the \
                         {MAX_READBACK_AGE_S:.0} s this point will accept"
                    ));
                    return;
                }
                let now_ms = now_unix_ms();
                let settle_ms = (point.settle_s * 1_000.0).round().max(0.0) as u64;
                let seq = self.sensor_seq;
                if let Some(run) = self.run.as_mut() {
                    run.point.readback = Some(*readback);
                    run.point.readback_age_s = *readback_age_s;
                    run.point.applied = applied;
                    run.phase = RunPhase::Settling;
                    run.point.settle_until_ms = now_ms.saturating_add(settle_ms);
                    run.point.settle_started_ms = seq;
                    run.point.saw_fresh_sensor = false;
                    run.last_activity_ms = now_ms;
                }
                self.note(format!(
                    "Point [{}]: codes {}/{} confirmed, settling {:.1} s…",
                    point.label,
                    readback.current.diff_on,
                    readback.current.diff_off,
                    point.settle_s
                ));
            }
            HostCommandOutcome::Rejected { code, message } => {
                // Carry the host's own wording through: "turn the STC filter
                // off" tells the operator what to do, "bias change failed"
                // does not.
                self.skip_current(format!(
                    "the host refused the bias change ({code}): {message}"
                ));
            }
            _ => self.skip_current("the host answered the bias change with a recording receipt"),
        }
    }

    fn on_start_reply(&mut self, outcome: &HostCommandOutcome) {
        match outcome {
            HostCommandOutcome::RecordingStarted {
                actual_raw_path, ..
            } => {
                let now_ms = now_unix_ms();
                let sensor = self.sensor;
                if self.devices.active {
                    let run = self.run.as_ref().unwrap();
                    let expected = Path::new(self.output_folder.trim())
                        .join(&run.measurement_id)
                        .join(format!("{}.raw", run.point.stem));
                    let actual = Path::new(actual_raw_path);
                    if !actual.is_file()
                        || actual.canonicalize().ok() != expected.canonicalize().ok()
                    {
                        let run = self.run.as_mut().unwrap();
                        run.point.raw_path = Some(actual_raw_path.clone());
                        run.stop_requested = true;
                        self.skip_current("Host did not open RAW at the requested common root; PD was not started");
                        return;
                    }
                }
                if self.devices.active {
                    let run = self.run.as_ref().unwrap();
                    self.devices.begin_pd(
                        self.output_folder.trim().to_owned(),
                        &run.measurement_id,
                        &run.point.stem,
                        run.point().unwrap().duration_s as f64,
                    );
                }
                if let Some(run) = self.run.as_mut() {
                    run.point.raw_path = Some(actual_raw_path.clone());
                    run.phase = if self.devices.active {
                        RunPhase::StartingPd
                    } else {
                        RunPhase::Recording
                    };
                    run.point.started_unix_ms = now_ms;
                    // Freeze the bench conditions this point begins under,
                    // before the recording has had time to move them.
                    if let Some(sensor) = sensor {
                        run.point.temperature.start = sensor.temperature_c;
                        run.point.illumination.start = sensor.illumination_lux;
                        run.point.pixel_dead_time_us = sensor.pixel_dead_time_us;
                        run.point.sensor_age_s = Some(sensor.age_s);
                    }
                    run.last_activity_ms = now_ms;
                }
            }
            HostCommandOutcome::Rejected { code, message } => {
                if let Some(run) = self.run.as_mut() {
                    run.camera_may_record = false;
                }
                self.skip_current(format!(
                    "the host refused the recording ({code}): {message}"
                ));
            }
            _ => self.skip_current("the host answered the start with an unexpected receipt"),
        }
    }

    fn on_stop_reply(&mut self, outcome: &HostCommandOutcome) {
        let requested_s = self
            .run
            .as_ref()
            .and_then(|run| run.point().map(|point| point.duration_s))
            .unwrap_or(0) as f64;
        let sensor = self.sensor;
        let Some(run) = self.run.as_mut() else {
            return;
        };
        if let Some(sensor) = sensor {
            run.point.temperature.end = sensor.temperature_c;
            run.point.illumination.end = sensor.illumination_lux;
        }
        let verdict = match outcome {
            HostCommandOutcome::RecordingFinalized {
                actual_raw_path,
                size,
                sha256,
                duration_us,
            } => {
                let seconds = *duration_us as f64 / 1_000_000.0;
                run.camera_may_record = false;
                run.point.finalized_path = Some(actual_raw_path.clone());
                run.point.size = Some(*size);
                run.point.sha256 = Some(sha256.clone());
                run.point.recorded_duration_s = Some(seconds);
                // Every part of the receipt is checked, not just the word the
                // host used: an empty file, a missing hash, or a recording cut
                // short is not a threshold point.
                if *size == 0 {
                    Err("the recording is empty (0 bytes)".to_owned())
                } else if sha256.trim().is_empty() {
                    Err("the recording finished without a hash".to_owned())
                } else if requested_s > 0.0 && seconds < requested_s * MIN_DURATION_FRACTION {
                    Err(format!(
                        "the recording is {seconds:.1} s of the {requested_s:.0} s asked for"
                    ))
                } else {
                    Ok(())
                }
            }
            HostCommandOutcome::RecordingPartial {
                actual_raw_path,
                size,
                sha256,
                duration_us,
                reason,
            } => {
                // Kept on disk and fully described, but never counted as a
                // success: a partial file is not a threshold point.
                run.camera_may_record = false;
                run.point.finalized_path = Some(actual_raw_path.clone());
                run.point.size = *size;
                run.point.sha256 = sha256.clone();
                run.point.recorded_duration_s = Some(*duration_us as f64 / 1_000_000.0);
                Err(format!("the recording did not finalize cleanly: {reason}"))
            }
            HostCommandOutcome::Rejected { code, message } => {
                Err(format!("the stop was refused ({code}): {message}"))
            }
            HostCommandOutcome::RecordingStarted { .. }
            | HostCommandOutcome::CameraConfigurationApplied { .. }
            | HostCommandOutcome::CameraConfigurationRestored { .. } => {
                Err("the stop answered with an unexpected receipt".to_owned())
            }
        };
        // Filed on the next tick by `drive`, which owns advancing the run.
        run.point.complete = verdict.is_ok();
        run.point.incomplete_reason = verdict.err();
    }

    /// Mark the current point as unrecordable. `drive` files it on the next
    /// tick — reply handlers must not advance the run themselves, or a reply
    /// arriving mid-tick would start the next point before this one is filed.
    fn skip_current(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        if let Some(run) = self.run.as_mut() {
            run.pending_skip = Some(reason.clone());
            run.point.complete = false;
        }
        self.message = reason;
        self.bump();
    }

    // ---- the tick ----------------------------------------------------------

    /// Advance the run one control tick.
    fn drive(&mut self, context: &mut impl HostControl) {
        if self.restore_pending {
            self.restore_pending = false;
            self.restore_biases_now(context);
        }
        if self.run.is_none() {
            if self.start_pending {
                self.start_pending = false;
                self.begin_run(context);
            }
            self.stop_pending = false;
            self.continue_pending = false;
            return;
        }
        if self.start_pending {
            // Say so rather than swallowing the press: Stop is a different
            // button, and a silently ignored one reads as a dead control.
            self.start_pending = false;
            self.note("A protocol is already running — press Stop to end it");
        }
        if self.stop_pending {
            self.stop_pending = false;
            if let Some(run) = self.run.as_mut() {
                run.stop_requested = true;
            }
            self.note("Stopping after the point in flight…");
        }

        let now_ms = now_unix_ms();
        if let Some(reason) = self.devices.error.clone() {
            if let Some(run) = self.run.as_mut() {
                if run.phase != RunPhase::CleaningDevices && run.device_failure.is_none() {
                    run.device_failure = Some(reason.clone());
                    run.stop_requested = true;
                    if run.camera_may_record {
                        self.stop_recording(context);
                    } else {
                        self.finish_run(context);
                    }
                    self.note(format!("A4 device failure: {reason}. Files are retained; no later point will start."));
                    return;
                }
                if run.phase == RunPhase::CleaningDevices {
                    self.note(format!(
                        "Device cleanup needs recovery: {reason}. Press Continue to retry cleanup."
                    ));
                }
            }
        }
        // A reply handler decided this point cannot be recorded. File it here,
        // before anything else looks at the phase.
        if let Some(reason) = self.run.as_mut().and_then(|run| run.pending_skip.take()) {
            self.fail_point(context, reason);
            return;
        }

        let (phase, stop_requested, pending, last_activity) = {
            let run = self.run.as_ref().expect("checked above");
            (
                run.phase,
                run.stop_requested,
                run.pending_request,
                run.last_activity_ms,
            )
        };

        // A command that never came back must not strand an unattended survey.
        if pending.is_some() && now_ms.saturating_sub(last_activity) > REPLY_TIMEOUT_MS {
            if let Some(run) = self.run.as_mut() {
                run.pending_request = None;
            }
            match phase {
                RunPhase::RestoringBiases => {
                    self.note(
                        "The host did not answer the bias restore — check the camera settings",
                    );
                    self.note("Camera restore timed out. Press Continue to retry restoration.");
                }
                // Nothing has been changed or recorded yet, and there is no
                // baseline to record against, so end the run rather than fail
                // every point in it one timeout at a time.
                RunPhase::OpeningSession => {
                    self.note(
                        "The host did not confirm the camera configuration in time — the survey \
                         did not start",
                    );
                    self.close_run();
                }
                RunPhase::StoppingRecording => {
                    self.fail_point(context, "the host did not answer the stop in time");
                }
                _ => self.fail_point(context, "the host did not answer in time"),
            }
            return;
        }
        // Stop ends the run as soon as it can do so safely, which is not the
        // same as immediately. A recording in flight has to wind down — an
        // abandoned one leaves a truncated RAW behind — and a start already
        // sent has to be answered before it can be stopped at all, or the host
        // is left recording with nobody to end it. Everywhere else there is no
        // file at risk, so waiting out a bias reply would only make Stop feel
        // dead for twenty seconds.
        if stop_requested
            && matches!(
                phase,
                RunPhase::OpeningSession
                    | RunPhase::PausedForOperator
                    | RunPhase::ApplyingBiases
                    | RunPhase::Settling
            )
        {
            self.finish_run(context);
            return;
        }
        if pending.is_some() {
            return;
        }

        match phase {
            // The baseline reply has landed (pending is clear); begin the
            // first point against it.
            RunPhase::OpeningSession => {
                if self.run.as_ref().is_some_and(|run| run.camera.is_some()) {
                    if self.run.as_ref().is_some_and(|r| r.plan.preserve_current) {
                        let id = self.run.as_ref().unwrap().measurement_id.clone();
                        match self.devices.prepare(&id, now_ms) {
                            Ok(()) => self.run.as_mut().unwrap().phase = RunPhase::PreparingDevices,
                            Err(reason) => {
                                self.note(reason);
                                self.finish_run(context);
                            }
                        }
                    } else {
                        self.enter_point(context);
                    }
                }
            }
            RunPhase::PreparingDevices => {
                if self.devices.ready() {
                    if stop_requested {
                        self.finish_run(context);
                    } else {
                        self.enter_point(context);
                    }
                }
            }
            RunPhase::StartingPd => {
                if self.devices.ready() {
                    let run = self.run.as_mut().unwrap();
                    run.phase = RunPhase::Recording;
                    run.point.started_unix_ms = now_ms;
                }
            }
            RunPhase::FinalizingPd => {
                if self.devices.ready() {
                    self.stop_recording(context);
                }
            }
            RunPhase::CleaningDevices => {
                if self.devices.released() {
                    self.devices.active = false;
                    self.finish_run(context);
                } else if self.continue_pending {
                    self.continue_pending = false;
                    self.devices.release();
                }
            }
            RunPhase::RecoveryPaused => {
                if self.continue_pending || stop_requested {
                    self.continue_pending = false;
                    if self.run.as_ref().is_some_and(|r| r.camera_may_record) {
                        self.stop_recording(context);
                    } else if stop_requested {
                        self.finish_run(context);
                    } else {
                        let run = self.run.as_mut().unwrap();
                        run.retry_count += 1;
                        run.point = PointState::default();
                        self.send_biases(context);
                    }
                }
            }
            RunPhase::RestoringBiases => {
                if self.continue_pending {
                    self.continue_pending = false;
                    self.finish_run(context);
                }
            }
            RunPhase::PausedForOperator => {
                if self.continue_pending {
                    self.continue_pending = false;
                    self.send_biases(context);
                }
            }
            // Waiting on a reply that has not arrived and has not timed out.
            RunPhase::ApplyingBiases | RunPhase::StartingRecording => {}
            RunPhase::Settling => {
                if now_ms < self.run.as_ref().map_or(0, |run| run.point.settle_until_ms) {
                    return;
                }
                // A settle that produced no fresh telemetry is not a settle:
                // without a new reading there is no evidence the bench has
                // stopped moving, and the point's start conditions would be
                // copied from before the bias change.
                if !self
                    .run
                    .as_ref()
                    .is_some_and(|run| run.point.saw_fresh_sensor)
                {
                    if now_ms.saturating_sub(last_activity) > REPLY_TIMEOUT_MS {
                        self.fail_point(
                            context,
                            "no fresh sensor reading arrived during the settle, so the bench \
                             could not be confirmed stable",
                        );
                    }
                    return;
                }
                self.start_recording(context);
            }
            RunPhase::Recording => {
                let (started, duration_s) = {
                    let run = self.run.as_ref().expect("checked above");
                    (
                        run.point.started_unix_ms,
                        run.point().map(|point| point.duration_s).unwrap_or(0),
                    )
                };
                let elapsed_ms = now_ms.saturating_sub(started);
                let over = elapsed_ms >= (duration_s.max(0) as u64).saturating_mul(1_000);
                if over || stop_requested {
                    if self.devices.active {
                        self.devices.finish_pd(stop_requested);
                        self.run.as_mut().unwrap().phase = RunPhase::FinalizingPd;
                    } else {
                        self.stop_recording(context);
                    }
                }
            }
            RunPhase::StoppingRecording => {
                // The reply has landed (pending is clear); file the point.
                let Some(point) = self.run.as_ref().and_then(|run| run.point().cloned()) else {
                    return;
                };
                let index = self.run.as_ref().map(|run| run.index).unwrap_or(0);
                let complete = self
                    .run
                    .as_ref()
                    .is_some_and(|run| run.point.complete && run.device_failure.is_none());
                let reason = self
                    .run
                    .as_ref()
                    .and_then(|run| run.point.incomplete_reason.clone());
                if complete {
                    self.record_point(&point, index, PointOutcome::Recorded);
                    self.note(format!("Point {} [{}] recorded", index + 1, point.label));
                    self.advance(context);
                } else {
                    self.fail_point(
                        context,
                        reason.unwrap_or_else(|| "the recording did not finalize".into()),
                    );
                }
            }
        }
    }

    /// Put the biases back where the last survey found them, outside a run.
    ///
    /// This is the recovery path for a run that did not get to restore them
    /// itself — a point left the sensor on a threshold nobody wants it on.
    /// During a run it is refused: the run restores them when it ends, and a
    /// restore in the middle would silently retarget the point being recorded.
    ///
    /// The pre-run state belongs to the host's session, not to this plugin, so
    /// this asks the host to put it back rather than re-sending remembered
    /// offsets. A host that was reloaded mid-survey has no session left and
    /// refuses; its wording is carried through to the operator.
    fn restore_biases_now(&mut self, context: &mut impl HostControl) {
        if self.run.is_some() {
            self.note("A protocol is running — it puts the biases back when it ends");
            return;
        }
        if self.restore_request.is_some() {
            return;
        }
        if self.last_original.is_none() {
            self.note(
                "Nothing to restore — no survey has changed the biases since this plugin loaded",
            );
            return;
        }
        let request_id = self.next_request_id();
        context.request_host(&HostCommandRequest {
            request_id,
            command: HostCommand::RestoreCameraConfiguration,
        });
        self.restore_request = Some(request_id);
        self.note("Restoring the configuration the survey started from…");
    }

    // ---- datasets ----------------------------------------------------------

    fn status_dataset(&self) -> TableDatasetV1 {
        let column = |id: &str, value: String| TableColumnData {
            column_id: id.into(),
            values: TableColumnValues::String(vec![value]),
        };
        let run = self.run.as_ref();
        let state = match run.map(|run| run.phase) {
            None => "idle".to_owned(),
            Some(RunPhase::OpeningSession) => "confirming the camera configuration".to_owned(),
            Some(RunPhase::PausedForOperator) => "paused — press Continue".to_owned(),
            Some(RunPhase::ApplyingBiases) => "setting biases".to_owned(),
            Some(RunPhase::Settling) => "settling".to_owned(),
            Some(RunPhase::StartingRecording) => "starting".to_owned(),
            Some(RunPhase::Recording) => "recording".to_owned(),
            Some(RunPhase::StoppingRecording) => "saving".to_owned(),
            Some(RunPhase::PreparingDevices) => "preparing constant light and PD".to_owned(),
            Some(RunPhase::StartingPd) => "starting PD capture".to_owned(),
            Some(RunPhase::FinalizingPd) => "finalizing PD capture".to_owned(),
            Some(RunPhase::CleaningDevices) => "releasing devices".to_owned(),
            Some(RunPhase::RecoveryPaused) => "recovery paused".to_owned(),
            Some(RunPhase::RestoringBiases) => "restoring biases".to_owned(),
        };
        let progress = run
            .map(|run| {
                format!(
                    "{}/{}",
                    (run.index + 1).min(run.plan.points.len()),
                    run.plan.points.len()
                )
            })
            .unwrap_or_else(|| "—".into());
        let biases = run
            .and_then(|run| run.point())
            .map(|point| format!("{} / {}", point.diff_on, point.diff_off))
            .unwrap_or_else(|| "—".into());
        let codes = self
            .sensor
            .and_then(|sensor| sensor.bias_codes)
            .map(|codes| format!("{} / {}", codes.current.diff_on, codes.current.diff_off))
            .unwrap_or_else(|| "—".into());
        let rate = run
            .and_then(|run| run.point.rates.total_rate_hz())
            .map(|hz| format!("{hz:.0} ev/s"))
            .unwrap_or_else(|| "—".into());
        let temperature = self
            .sensor
            .and_then(|sensor| sensor.temperature_c)
            .map(|celsius| format!("{celsius:.1} °C"))
            .unwrap_or_else(|| "—".into());

        TableDatasetV1 {
            columns: vec![
                column("state", state),
                column("progress", progress),
                column("biases", biases),
                column("codes", codes),
                column("rate", rate),
                column("temperature", temperature),
                column("message", self.message.clone()),
            ],
        }
    }

    fn points_dataset(&self) -> TableDatasetV1 {
        let records: &[PointRecord] = self
            .run
            .as_ref()
            .map(|run| run.records.as_slice())
            .unwrap_or(&[]);
        let column = |id: &str, values: Vec<String>| TableColumnData {
            column_id: id.into(),
            values: TableColumnValues::String(values),
        };
        let map =
            |select: fn(&PointRecord) -> String| records.iter().map(select).collect::<Vec<_>>();
        TableDatasetV1 {
            columns: vec![
                column("row", map(|record| record.row.to_string())),
                column("label", map(|record| record.label.clone())),
                column(
                    "offsets",
                    map(|record| format!("{} / {}", record.diff_on, record.diff_off)),
                ),
                column(
                    "codes",
                    map(|record| {
                        record
                            .codes
                            .map(|(on, off)| format!("{on} / {off}"))
                            .unwrap_or_else(|| "—".into())
                    }),
                ),
                column(
                    "repeat",
                    map(|record| format!("{}/{}", record.repeat.0, record.repeat.1)),
                ),
                column(
                    "on_rate",
                    map(|record| {
                        record
                            .rates
                            .on_rate_hz()
                            .map(|hz| format!("{hz:.0}"))
                            .unwrap_or_else(|| "—".into())
                    }),
                ),
                column(
                    "off_rate",
                    map(|record| {
                        record
                            .rates
                            .off_rate_hz()
                            .map(|hz| format!("{hz:.0}"))
                            .unwrap_or_else(|| "—".into())
                    }),
                ),
                column(
                    "total_rate",
                    map(|record| {
                        record
                            .rates
                            .total_rate_hz()
                            .map(|hz| format!("{hz:.0}"))
                            .unwrap_or_else(|| "—".into())
                    }),
                ),
                column("qc", map(|record| record.qc.as_str().to_owned())),
                column("status", map(|record| record.status_text())),
            ],
        }
    }
}

/// The absolute code a sensor programs for an offset: the factory trim plus the
/// offset, saturated into the 8-bit register. Mirrors the host's own rule so
/// A4 can state what it expects before the readback arrives.
fn monitoring_is_new(previous: Option<SensorMonitoringV1>, current: SensorMonitoringV1) -> bool {
    if !current.age_s.is_finite() || current.age_s < 0.0 {
        return false;
    }
    previous.is_none_or(|old| {
        current.age_s < old.age_s
            || current.bias_codes != old.bias_codes
            || current.temperature_c != old.temperature_c
            || current.illumination_lux != old.illumination_lux
    })
}

fn expected_code(factory_default: u8, offset: i64) -> u8 {
    (factory_default as i64 + offset).clamp(0, 255) as u8
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

/// Replace anything that is not `[A-Za-z0-9._-]` with `_` so ids are file-safe.
fn sanitize_stem(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for character in input.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            out.push(character);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        "A4".into()
    } else {
        trimmed
    }
}

fn generate_measurement_id() -> String {
    let ms = now_unix_ms();
    format!("A4-{}-{:04x}", format_compact_date(ms / 1_000), ms & 0xffff)
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
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
    let (year, month, day) = civil_from_days(days);
    (year, month, day, sod / 3_600, (sod % 3_600) / 60, sod % 60)
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

impl Plugin for StageAA4Plugin {
    fn name(&self) -> &'static str {
        "Stage-A A4 Background"
    }

    fn description(&self) -> &'static str {
        "Matched constant-light background reference with automatic PD capture and preserved camera settings; also supports legacy threshold surveys."
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
        self.bump();
    }

    fn on_discontinuity(&mut self, reason: PluginDiscontinuity) {
        // Starting and stopping the host recorder restarts the capture
        // pipeline, and the host reports that as SourceChanged — twice per
        // point. Those boundaries are self-inflicted, so none of them may
        // disturb a survey in flight. Nothing here caches across a point
        // anyway: the event counters are reset when each point starts.
        let _ = reason;
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::RawEvents
    }

    fn capabilities(&self) -> PluginCapabilities {
        // The QC rates are counted from preview frames, which is enough for a
        // stability indicator; the authoritative counts come from the RAW file
        // offline. Retaining event history would cost memory a 60 s point does
        // not need.
        PluginCapabilities::default()
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        if let Some(settings) = context
            .get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)
            .ok()
            .flatten()
        {
            self.host_roi = Some(settings.roi);
            self.masked_pixels = settings.masked_pixels.len();
            self.sensor_size = (settings.sensor_width, settings.sensor_height);
            self.event_filters = Some(settings.event_filters);
        }
        if let Some(monitoring) = context
            .get::<SensorMonitoringV1>(CTX_SENSOR_MONITORING)
            .ok()
            .flatten()
        {
            // Only a genuinely new reading counts as one: the host republishes
            // the same snapshot on every frame between polls, and the settle
            // gate is asking whether the sensor has been read *again*.
            if monitoring_is_new(self.sensor, monitoring) {
                self.sensor_seq = self.sensor_seq.wrapping_add(1);
                if let Some(run) = self.run.as_mut() {
                    if run.phase == RunPhase::Settling
                        && self.sensor_seq != run.point.settle_started_ms
                    {
                        run.point.saw_fresh_sensor = true;
                    }
                }
            }
            self.sensor = Some(monitoring);
        }

        // Count events only while a point's RAW is being written, and only over
        // the slice of the stream not yet counted — preview windows overlap, so
        // taking every frame whole would double-count the overlap.
        let recording = self
            .run
            .as_ref()
            .is_some_and(|run| run.phase == RunPhase::Recording);
        if recording {
            let window_end = frame.window_end_us();
            let (mut on, mut off, mut seconds) = (0_u64, 0_u64, 0.0_f64);
            if let Some(run) = self.run.as_ref() {
                let from = run.point.counted_to_us.unwrap_or(window_end);
                if window_end > from {
                    for event in frame.events() {
                        let timestamp = event.t_us.max(0) as u64;
                        if timestamp >= from && timestamp < window_end {
                            if event.polarity != 0 {
                                on += 1;
                            } else {
                                off += 1;
                            }
                        }
                    }
                    seconds = (window_end - from) as f64 / 1_000_000.0;
                }
            }
            if let Some(run) = self.run.as_mut() {
                run.point.rates.on_events += on;
                run.point.rates.off_events += off;
                run.point.rates.seconds += seconds;
                run.point.counted_to_us = Some(window_end);
            }
        }
        self.bump();
    }

    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        let inbox: PluginControlInbox = context.inbox().clone();
        self.devices.update(&inbox);
        if self.run.is_none() && self.output_folder.trim().is_empty() {
            if let Some(folder) = self
                .devices
                .photodiode
                .as_ref()
                .and_then(|p| p.data_dir.clone())
            {
                self.output_folder = folder;
            }
        }
        if let Some(request) = self.devices.tick(now_unix_ms()) {
            HostControl::request_service(context, &request);
        }
        for reply in &inbox.host_replies {
            self.on_host_reply(reply);
        }
        self.drive(context);
        self.bump();
    }

    fn settings_schema(&self) -> SettingsSchema {
        // Deliberately *not* gated on "is something running": `settings_schema`
        // is rendered by the UI mirror, and the run lives on the live worker,
        // which is the only instance the host calls `process_control` on. A
        // mirror reading its own always-idle state would disable nothing and
        // mislead the next reader into thinking it did. The authoritative
        // interlocks stay worker-side, where `start_blocker` refuses with a
        // message that names what is wrong.
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Measurement".into(),
                    description: Some(
                        "Where the survey's files go. The output folder is the only thing A4 \
                         needs from you before it can run — the measurement id is filled in if \
                         you leave it blank."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "output_folder".into(),
                            label: "Output folder".into(),
                            tooltip: Some(
                                "Every recording, sidecar and the protocol copy land in \
                                 <folder>/<measurement id>/."
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
                                "Names the folder and every file stem under it. Left blank, a \
                                 dated one is generated and written back here."
                                    .into(),
                            ),
                            kind: SettingKind::Text {
                                default: self.measurement_id.clone(),
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Protocol".into(),
                    description: Some(
                        "Bright reference is built in: three 120-second captures at constant mean_u=0.30, \
                         with automatic PD capture. Keep the A1-A3 AOD and laser settings. All five camera \
                         biases, ROI and mask are preserved. Leave the custom protocol blank. \
                         Legacy CSV/TOML threshold surveys retain their externally controlled light mode."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "protocol_path".into(),
                            label: "Optional custom protocol (blank = bright reference)".into(),
                            tooltip: Some(
                                "A .csv or .toml protocol. It is validated in full on Run, so a \
                                 bad file is refused before the first bias moves."
                                    .into(),
                            ),
                            kind: SettingKind::Path {
                                dialog: PathDialogKind::OpenFile,
                                default: self.protocol_path.clone(),
                            },
                        },
                        SettingItem {
                            key: "run_protocol".into(),
                            label: "Run protocol".into(),
                            tooltip: Some(
                                "Validate the file, capture the biases the bench is on now, and \
                                 record every point. The originals are put back at the end, on \
                                 Stop, and on any abort."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "continue_run".into(),
                            label: "Continue".into(),
                            tooltip: Some(
                                "Retry the current point or an unconfirmed cleanup; also resumes optical pauses."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "stop_protocol".into(),
                            label: "Stop".into(),
                            tooltip: Some(
                                "End the run after the recording in flight winds down — \
                                 abandoning it mid-write would leave a truncated RAW behind."
                                    .into(),
                            ),
                            kind: SettingKind::Button { enabled: true },
                        },
                        SettingItem {
                            key: "restore_biases".into(),
                            label: "Restore biases".into(),
                            tooltip: Some(
                                "Put diff_on and diff_off back where the last survey found them. \
                                 A run does this itself when it ends; this is the recovery path \
                                 for one that could not — a reload mid-survey, say."
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
            "protocol_path" => Some(json!(self.protocol_path)),
            "run_protocol" => Some(self.press_start.value()),
            "continue_run" => Some(self.press_continue.value()),
            "stop_protocol" => Some(self.press_stop.value()),
            "restore_biases" => Some(self.press_restore.value()),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        if self.run.is_some() && matches!(key, "output_folder" | "measurement_id" | "protocol_path")
        {
            return Err(
                "Recording configuration is locked until the run and restoration finish".into(),
            );
        }
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
            "protocol_path" => {
                self.protocol_path = value
                    .as_str()
                    .ok_or("protocol_path must be a string")?
                    .to_string();
            }
            // Every button arm is effectful, so each one is edge-guarded: the
            // host syncs settings to both plugin instances, and an unguarded
            // arm would fire twice per click.
            "run_protocol" => {
                if self.press_start.accept(&value) {
                    self.start_pending = true;
                }
            }
            "continue_run" => {
                if self.press_continue.accept(&value) {
                    self.continue_pending = true;
                }
            }
            "stop_protocol" => {
                if self.press_stop.accept(&value) {
                    self.stop_pending = true;
                }
            }
            "restore_biases" => {
                if self.press_restore.accept(&value) {
                    self.restore_pending = true;
                }
            }
            _ => return Err(format!("unknown setting '{key}'")),
        }
        self.bump();
        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        match self.run.as_ref() {
            Some(run) => {
                entries.push(StatusEntry::LabeledValue {
                    label: "Protocol".into(),
                    value: format!(
                        "{} — point {}/{}",
                        run.plan.name,
                        (run.index + 1).min(run.plan.points.len()),
                        run.plan.points.len()
                    ),
                    color: None,
                });
                let recorded = run
                    .records
                    .iter()
                    .filter(|record| record.outcome == PointOutcome::Recorded)
                    .count();
                let flagged = run
                    .records
                    .iter()
                    .filter(|record| record.qc.is_flagged())
                    .count();
                entries.push(StatusEntry::Text(format!(
                    "{recorded} recorded, {} failed attempts, {flagged} QC-flagged",
                    run.records.len() - recorded
                )));
            }
            None => {
                entries.push(StatusEntry::LabeledValue {
                    label: "Protocol".into(),
                    value: "idle".into(),
                    color: None,
                });
                if let Some(blocker) = self.start_blocker() {
                    entries.push(StatusEntry::Text(format!("Not ready — {blocker}")));
                }
            }
        }
        // The bias codes the sensor is actually running, always, so the panel
        // never has to be trusted about them.
        entries.push(StatusEntry::Text(
            match self.sensor.and_then(|sensor| sensor.bias_codes) {
                Some(codes) => format!(
                    "Sensor reports diff_on={} (offset {}), diff_off={} (offset {})",
                    codes.current.diff_on,
                    codes.current.diff_on as i32 - codes.factory_default.diff_on as i32,
                    codes.current.diff_off,
                    codes.current.diff_off as i32 - codes.factory_default.diff_off as i32,
                ),
                None => {
                    "The sensor is not reporting bias codes — A4 will not run without them".into()
                }
            },
        ));
        entries.push(StatusEntry::Text(self.message.clone()));
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
                    title: "A4 status".into(),
                    kind: HostDatasetKind::TableV1(TableSchema {
                        columns: vec![
                            column("state", "State"),
                            column("progress", "Point"),
                            column("biases", "Asked (on/off)"),
                            column("codes", "On die (on/off)"),
                            column("rate", "Rate"),
                            column("temperature", "Die temp"),
                            column("message", "Message"),
                        ],
                        ..TableSchema::default()
                    }),
                    empty_message: "A4 idle".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: POINTS_DATASET_ID.into(),
                    title: "A4 threshold points".into(),
                    kind: HostDatasetKind::TableV1(TableSchema {
                        columns: vec![
                            column("row", "Row"),
                            column("label", "Label"),
                            column("offsets", "Offsets"),
                            column("codes", "Codes"),
                            column("repeat", "Repeat"),
                            column("on_rate", "ON (ev/s)"),
                            column("off_rate", "OFF (ev/s)"),
                            column("total_rate", "Total (ev/s)"),
                            column("qc", "QC"),
                            column("status", "Status"),
                        ],
                        ..TableSchema::default()
                    }),
                    empty_message: "No points recorded yet — press Run protocol".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: STATUS_VIEW_ID.into(),
                    title: "A4 status".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
                HostViewDescriptor {
                    id: POINTS_VIEW_ID.into(),
                    title: "A4 threshold points".into(),
                    dataset_id: POINTS_DATASET_ID.into(),
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
            POINTS_DATASET_ID => serde_json::to_vec(&self.points_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            STATUS_DATASET_ID | POINTS_DATASET_ID => self.generation,
            _ => 0,
        }
    }
}

export_plugin!(StageAA4Plugin);

#[cfg(test)]
mod tests {
    use super::*;
    use augur_plugin_api::{
        CameraBiasOffsetsV1, CameraConfigurationProvenanceV1, CameraDigitalFilterV1,
        CameraExternalTriggerV1, CameraGlobalSettingsV1, SensorBiasCodesV1, SensorBiasReadbackV1,
    };

    /// Factory trim of the unit these tests pretend to run on.
    const FACTORY_ON: u8 = 102;
    const FACTORY_OFF: u8 = 40;

    #[derive(Default)]
    struct ControlSink {
        hosts: Vec<HostCommandRequest>,
    }

    impl HostControl for ControlSink {
        fn request_host(&mut self, request: &HostCommandRequest) {
            self.hosts.push(request.clone());
        }
    }

    impl ControlSink {
        fn last_id(&self) -> u64 {
            self.hosts
                .last()
                .map(|request| request.request_id)
                .unwrap_or(0)
        }

        /// The two biases of every configuration a point applied. The session's
        /// opening `Current` carries no snapshot and does not appear here.
        fn applied_biases(&self) -> Vec<(i32, i32)> {
            self.applied_snapshots()
                .iter()
                .map(|snapshot| (snapshot.biases.diff_on, snapshot.biases.diff_off))
                .collect()
        }

        fn applied_snapshots(&self) -> Vec<CameraConfigurationSnapshotV1> {
            self.hosts
                .iter()
                .filter_map(|request| match &request.command {
                    HostCommand::ApplyCameraConfiguration {
                        configuration: CameraConfigurationSourceV1::Snapshot { snapshot },
                    } => Some(snapshot.clone()),
                    _ => None,
                })
                .collect()
        }

        fn restores(&self) -> usize {
            self.hosts
                .iter()
                .filter(|request| {
                    matches!(request.command, HostCommand::RestoreCameraConfiguration)
                })
                .count()
        }
    }

    const BASELINE_ON: i32 = 7;
    const BASELINE_OFF: i32 = -3;

    /// The configuration the host confirms when a survey opens its session.
    /// Everything except the two biases must survive the sweep untouched.
    fn baseline_snapshot() -> CameraConfigurationSnapshotV1 {
        CameraConfigurationSnapshotV1 {
            schema_version: 1,
            biases: CameraBiasOffsetsV1 {
                diff_on: BASELINE_ON,
                diff_off: BASELINE_OFF,
                fo: 4,
                hpf: 1,
                refr: -2,
            },
            roi: RoiV1 {
                x: 16,
                y: 32,
                width: 640,
                height: 480,
            },
            masked_pixels: vec![(3, 4), (5, 6)],
            digital_filter: CameraDigitalFilterV1 {
                stc_enabled: false,
                stc_threshold_us: 10_000,
                trail_enabled: false,
                erc_enabled: Some(false),
            },
            external_trigger: CameraExternalTriggerV1 {
                enabled: true,
                channel: 2,
            },
            global: CameraGlobalSettingsV1 {
                nm_per_pixel: 100.0,
                pixel_scale_calibrated: true,
                sensor_width: 1280,
                sensor_height: 720,
                acq_time_ms: 20,
                event_store_budget_mib: 512,
                preview_interval_ms: 33,
                point_cloud_interval_ms: 100,
                disk_writer_buffer_mib: 64,
                record_sensor_telemetry: true,
            },
        }
    }

    fn readback(on_offset: i64, off_offset: i64) -> SensorBiasReadbackV1 {
        SensorBiasReadbackV1 {
            current: SensorBiasCodesV1 {
                diff_on: expected_code(FACTORY_ON, on_offset),
                diff_off: expected_code(FACTORY_OFF, off_offset),
                fo: 55,
                hpf: 0,
                refr: 138,
            },
            factory_default: SensorBiasCodesV1 {
                diff_on: FACTORY_ON,
                diff_off: FACTORY_OFF,
                fo: 55,
                hpf: 0,
                refr: 138,
            },
        }
    }

    fn monitoring(on_offset: i64, off_offset: i64, age_s: f64) -> SensorMonitoringV1 {
        SensorMonitoringV1 {
            pixel_dead_time_us: Some(12.5),
            illumination_lux: Some(200.0),
            temperature_c: Some(41.0),
            bias_codes: Some(readback(on_offset, off_offset)),
            age_s,
        }
    }

    fn temp_folder(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("a4-{tag}-{}", now_unix_ms()));
        std::fs::create_dir_all(&dir).expect("test folder");
        dir
    }

    /// A plugin that has seen a camera reporting its biases, so `start_blocker`
    /// is satisfied and `live_offsets` has something to capture.
    fn ready_plugin(folder: &Path, protocol: &Path) -> StageAA4Plugin {
        StageAA4Plugin {
            output_folder: folder.display().to_string(),
            protocol_path: protocol.display().to_string(),
            measurement_id: "A4-TEST".into(),
            event_filters: Some(EventFiltersV1::default()),
            sensor: Some(monitoring(7, -3, 0.1)),
            ..StageAA4Plugin::default()
        }
    }

    fn write_protocol(folder: &Path, body: &str) -> PathBuf {
        let path = folder.join("survey.csv");
        std::fs::write(&path, body).expect("protocol written");
        path
    }

    /// Mark the settle as satisfied, the way a fresh monitoring frame would.
    fn deliver_fresh_sensor(plugin: &mut StageAA4Plugin, sensor: SensorMonitoringV1) {
        plugin.sensor_seq = plugin.sensor_seq.wrapping_add(1);
        if let Some(run) = plugin.run.as_mut() {
            if run.phase == RunPhase::Settling {
                run.point.saw_fresh_sensor = true;
            }
        }
        plugin.sensor = Some(sensor);
    }

    fn applied_reply(
        request_id: u64,
        on_offset: i64,
        off_offset: i64,
        age_s: f64,
    ) -> HostCommandReply {
        let mut snapshot = baseline_snapshot();
        snapshot.biases.diff_on = on_offset as i32;
        snapshot.biases.diff_off = off_offset as i32;
        HostCommandReply {
            request_id,
            outcome: HostCommandOutcome::CameraConfigurationApplied {
                snapshot,
                provenance: CameraConfigurationProvenanceV1 {
                    source: "snapshot".into(),
                    profile_name: None,
                    schema_version: 1,
                    profile_revision: None,
                    sha256: "0".repeat(64),
                },
                readback: readback(on_offset, off_offset),
                readback_age_s: age_s,
            },
        }
    }

    fn restored_reply(request_id: u64) -> HostCommandReply {
        HostCommandReply {
            request_id,
            outcome: HostCommandOutcome::CameraConfigurationRestored {
                readback: readback(BASELINE_ON as i64, BASELINE_OFF as i64),
                readback_age_s: 0.1,
            },
        }
    }

    fn started_reply(request_id: u64, path: &str) -> HostCommandReply {
        HostCommandReply {
            request_id,
            outcome: HostCommandOutcome::RecordingStarted {
                actual_raw_path: path.to_owned(),
                started_at: "2026-08-08T10:00:00Z".into(),
            },
        }
    }

    fn finalized_reply(
        request_id: u64,
        path: &str,
        size: u64,
        duration_us: u64,
    ) -> HostCommandReply {
        HostCommandReply {
            request_id,
            outcome: HostCommandOutcome::RecordingFinalized {
                actual_raw_path: path.to_owned(),
                size,
                sha256: "a".repeat(64),
                duration_us,
            },
        }
    }

    fn rejected_reply(request_id: u64, code: &str, message: &str) -> HostCommandReply {
        HostCommandReply {
            request_id,
            outcome: HostCommandOutcome::Rejected {
                code: code.into(),
                message: message.into(),
            },
        }
    }

    /// Mirrors the ordering of `process_control`.
    fn tick(plugin: &mut StageAA4Plugin, replies: Vec<HostCommandReply>, sink: &mut ControlSink) {
        for reply in &replies {
            plugin.on_host_reply(reply);
        }
        plugin.drive(sink);
    }

    /// Answer the run's closing restore, so it writes its receipt and ends.
    /// The receipt records whether the biases went back, so it is deliberately
    /// not written until that is known.
    fn settle_restore(plugin: &mut StageAA4Plugin, sink: &mut ControlSink) {
        let restore_id = sink.last_id();
        tick(plugin, vec![restored_reply(restore_id)], sink);
    }

    /// Press Run and answer the baseline confirmation every survey opens with.
    /// Leaves the run on its first point's configuration command — or paused,
    /// if the first row asks the operator for something.
    fn start_survey(plugin: &mut StageAA4Plugin, sink: &mut ControlSink) {
        plugin.start_pending = true;
        tick(plugin, vec![], sink);
        let session_id = sink.last_id();
        tick(
            plugin,
            vec![applied_reply(
                session_id,
                BASELINE_ON as i64,
                BASELINE_OFF as i64,
                0.1,
            )],
            sink,
        );
    }

    /// Walk one point from its bias command through a clean finalize. Returns
    /// the RAW path it was told to write.
    fn run_one_point(
        plugin: &mut StageAA4Plugin,
        sink: &mut ControlSink,
        folder: &Path,
        on_offset: i64,
        off_offset: i64,
    ) -> PathBuf {
        let bias_id = sink.last_id();
        tick(
            plugin,
            vec![applied_reply(bias_id, on_offset, off_offset, 0.2)],
            sink,
        );
        deliver_fresh_sensor(plugin, monitoring(on_offset, off_offset, 0.1));
        // Settle is 0 s in the test protocols, so the next tick starts it.
        tick(plugin, vec![], sink);

        let start_id = sink.last_id();
        let raw = folder.join(format!("point-{on_offset}-{off_offset}.raw"));
        std::fs::write(&raw, b"raw-bytes").expect("raw written");
        tick(
            plugin,
            vec![started_reply(start_id, &raw.display().to_string())],
            sink,
        );
        // Duration is 1 s in the test protocols; force the clock past it.
        if let Some(run) = plugin.run.as_mut() {
            run.point.started_unix_ms = now_unix_ms().saturating_sub(5_000);
        }
        tick(plugin, vec![], sink);

        let stop_id = sink.last_id();
        tick(
            plugin,
            vec![finalized_reply(
                stop_id,
                &raw.display().to_string(),
                9,
                1_000_000,
            )],
            sink,
        );
        tick(plugin, vec![], sink);
        raw
    }

    #[test]
    fn a_survey_sets_confirms_records_and_then_puts_the_biases_back() {
        let folder = temp_folder("happy");
        let protocol = write_protocol(
            &folder,
            "diff_on,diff_off,duration_s,settle_s\n-20,-10,1,0\n20,20,1,0\n",
        );
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();

        tick(&mut plugin, vec![], &mut sink);
        start_survey(&mut plugin, &mut sink);

        run_one_point(&mut plugin, &mut sink, &folder, -20, -10);
        run_one_point(&mut plugin, &mut sink, &folder, 20, 20);

        assert_eq!(
            sink.applied_biases(),
            vec![(-20, -10), (20, 20)],
            "each point applies its own two biases"
        );
        // The host preserved the pre-run configuration when the session opened,
        // so putting the bench back is its own verb, sent last.
        assert_eq!(sink.restores(), 1);
        assert!(matches!(
            sink.hosts.last().map(|request| &request.command),
            Some(HostCommand::RestoreCameraConfiguration),
        ));

        // The run only closes once the restore is answered.
        assert!(
            plugin.run.is_some(),
            "the run waits for its restore receipt"
        );
        let restore_id = sink.last_id();
        tick(&mut plugin, vec![restored_reply(restore_id)], &mut sink);
        assert!(plugin.run.is_none(), "the run ends after the restore");
        assert!(
            plugin.message.contains("2/2 recorded"),
            "{}",
            plugin.message
        );
        assert!(
            plugin.message.contains("Biases restored"),
            "{}",
            plugin.message
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_recorded_point_lands_in_the_measurement_folder_with_its_sidecar() {
        let folder = temp_folder("gather");
        let protocol = write_protocol(
            &folder,
            "label,diff_on,diff_off,duration_s,settle_s\nthr-01,12,-8,1,0\n",
        );
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);
        run_one_point(&mut plugin, &mut sink, &folder, 12, -8);
        settle_restore(&mut plugin, &mut sink);

        let dir = folder.join("A4-TEST");
        let sidecars: Vec<PathBuf> = std::fs::read_dir(&dir)
            .expect("measurement folder")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().ends_with(".a4.toml"))
            .collect();
        assert_eq!(sidecars.len(), 1, "one sidecar per point: {sidecars:?}");
        let text = std::fs::read_to_string(&sidecars[0]).expect("sidecar readable");

        // The absolute codes, not just the offsets the row asked for — this is
        // the whole reason the sidecar exists.
        assert!(
            text.contains(&format!("code_diff_on = {}", FACTORY_ON as i64 + 12)),
            "{text}"
        );
        assert!(
            text.contains(&format!("code_diff_off = {}", FACTORY_OFF as i64 - 8)),
            "{text}"
        );
        assert!(text.contains("requested_diff_on = 12"), "{text}");
        assert!(text.contains("confirmed = true"), "{text}");
        assert!(text.contains("complete = true"), "{text}");
        assert!(text.contains("label = \"thr-01\""), "{text}");
        // The protocol travels with the data, with its hash.
        assert!(dir.join("survey.csv").exists(), "the protocol is copied in");
        assert!(text.contains("sha256"), "{text}");
        // And the RAW was moved out of the host's folder into this one.
        assert!(
            dir.join("point-12--8.raw").exists(),
            "the RAW is gathered in"
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn codes_that_disagree_retry_the_same_point() {
        // The central guarantee: a point whose biases cannot be shown to be
        // the requested ones is not recorded at all.
        let folder = temp_folder("mismatch");
        let protocol = write_protocol(
            &folder,
            "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n10,10,1,0\n",
        );
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        // The sensor answers with codes for a different offset entirely.
        let bias_id = sink.last_id();
        tick(
            &mut plugin,
            vec![applied_reply(bias_id, 99, 99, 0.1)],
            &mut sink,
        );
        tick(&mut plugin, vec![], &mut sink);

        // No recording was ever started for that point, and the run moved on.
        assert!(
            !sink
                .hosts
                .iter()
                .any(|request| matches!(request.command, HostCommand::StartRecording { .. })),
            "a mismatched point must not be recorded"
        );
        let records = &plugin.run.as_ref().expect("still running").records;
        assert_eq!(records.len(), 1);
        assert!(matches!(records[0].outcome, PointOutcome::Failed(_)));
        assert!(
            records[0]
                .status_text()
                .contains("requested complete configuration"),
            "{}",
            records[0].status_text()
        );
        // And the second point is under way rather than the run being over.
        assert_eq!(plugin.run.as_ref().expect("still running").index, 0);
        assert_eq!(plugin.run.as_ref().unwrap().retry_count, 1);

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_stale_confirming_reading_is_not_evidence_about_this_point() {
        let folder = temp_folder("stale");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        let bias_id = sink.last_id();
        // Correct codes, but read far too long after the change.
        tick(
            &mut plugin,
            vec![applied_reply(bias_id, 5, 5, 9.0)],
            &mut sink,
        );
        tick(&mut plugin, vec![], &mut sink);

        let records = &plugin
            .run
            .as_ref()
            .map(|run| run.records.clone())
            .unwrap_or_default();
        assert_eq!(records.len(), 1);
        assert!(
            records[0].status_text().contains("old"),
            "{}",
            records[0].status_text()
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_refused_bias_change_quotes_the_hosts_own_reason() {
        // "Turn the STC filter off" tells the operator what to do; "bias
        // change failed" does not.
        let folder = temp_folder("refused");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        let bias_id = sink.last_id();
        tick(
            &mut plugin,
            vec![rejected_reply(
                bias_id,
                "event_filters_enabled",
                "turn the STC and Trail filters off before changing threshold biases",
            )],
            &mut sink,
        );
        tick(&mut plugin, vec![], &mut sink);

        let message = plugin
            .run
            .as_ref()
            .and_then(|run| run.records.first().map(|record| record.status_text()))
            .unwrap_or_default();
        assert!(message.contains("STC"), "{message}");
        assert!(message.contains("event_filters_enabled"), "{message}");

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_partial_receipt_is_never_counted_as_a_recorded_point() {
        let folder = temp_folder("partial");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n0,0,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        let bias_id = sink.last_id();
        tick(
            &mut plugin,
            vec![applied_reply(bias_id, 0, 0, 0.2)],
            &mut sink,
        );
        deliver_fresh_sensor(&mut plugin, monitoring(0, 0, 0.1));
        tick(&mut plugin, vec![], &mut sink);
        let start_id = sink.last_id();
        let raw = folder.join("partial.raw");
        std::fs::write(&raw, b"x").expect("raw written");
        tick(
            &mut plugin,
            vec![started_reply(start_id, &raw.display().to_string())],
            &mut sink,
        );
        if let Some(run) = plugin.run.as_mut() {
            run.point.started_unix_ms = now_unix_ms().saturating_sub(5_000);
        }
        tick(&mut plugin, vec![], &mut sink);

        let stop_id = sink.last_id();
        tick(
            &mut plugin,
            vec![HostCommandReply {
                request_id: stop_id,
                outcome: HostCommandOutcome::RecordingPartial {
                    actual_raw_path: raw.display().to_string(),
                    size: Some(1),
                    sha256: None,
                    duration_us: 1_000_000,
                    reason: "the writer did not flush".into(),
                },
            }],
            &mut sink,
        );
        tick(&mut plugin, vec![], &mut sink);
        assert_eq!(plugin.run.as_ref().unwrap().index, 0);
        assert_eq!(plugin.run.as_ref().unwrap().retry_count, 1);
        plugin.stop_pending = true;
        tick(&mut plugin, vec![], &mut sink);
        settle_restore(&mut plugin, &mut sink);

        let receipt = std::fs::read_to_string(folder.join("A4-TEST/A4-TEST.protocol-status.toml"))
            .expect("receipt written");
        assert!(receipt.contains("rows_recorded = 0"), "{receipt}");
        assert!(receipt.contains("rows_failed = 1"), "{receipt}");
        assert!(receipt.contains("did not finalize cleanly"), "{receipt}");

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_recording_cut_short_is_a_truncated_file_not_a_short_point() {
        let folder = temp_folder("short");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n0,0,60,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);
        let bias_id = sink.last_id();
        tick(
            &mut plugin,
            vec![applied_reply(bias_id, 0, 0, 0.2)],
            &mut sink,
        );
        deliver_fresh_sensor(&mut plugin, monitoring(0, 0, 0.1));
        tick(&mut plugin, vec![], &mut sink);
        let start_id = sink.last_id();
        let raw = folder.join("short.raw");
        std::fs::write(&raw, b"x").expect("raw written");
        tick(
            &mut plugin,
            vec![started_reply(start_id, &raw.display().to_string())],
            &mut sink,
        );
        if let Some(run) = plugin.run.as_mut() {
            run.point.started_unix_ms = now_unix_ms().saturating_sub(70_000);
        }
        tick(&mut plugin, vec![], &mut sink);

        // A clean receipt, but only 10 s of the 60 s asked for.
        let stop_id = sink.last_id();
        tick(
            &mut plugin,
            vec![finalized_reply(
                stop_id,
                &raw.display().to_string(),
                4096,
                10_000_000,
            )],
            &mut sink,
        );
        tick(&mut plugin, vec![], &mut sink);
        assert_eq!(plugin.run.as_ref().unwrap().index, 0);
        assert_eq!(plugin.run.as_ref().unwrap().retry_count, 1);
        plugin.stop_pending = true;
        tick(&mut plugin, vec![], &mut sink);
        settle_restore(&mut plugin, &mut sink);

        let receipt = std::fs::read_to_string(folder.join("A4-TEST/A4-TEST.protocol-status.toml"))
            .expect("receipt written");
        assert!(receipt.contains("rows_recorded = 0"), "{receipt}");
        assert!(receipt.contains("10.0 s of the 60 s"), "{receipt}");

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_settle_with_no_fresh_reading_never_starts_a_recording() {
        // Without a new reading there is no evidence the bench stopped moving,
        // and the point's start conditions would be copied from before the
        // bias change.
        let folder = temp_folder("nosettle");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n0,0,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);
        let bias_id = sink.last_id();
        tick(
            &mut plugin,
            vec![applied_reply(bias_id, 0, 0, 0.2)],
            &mut sink,
        );

        // Several ticks with no new monitoring sample.
        for _ in 0..3 {
            tick(&mut plugin, vec![], &mut sink);
        }
        assert!(
            !sink
                .hosts
                .iter()
                .any(|request| matches!(request.command, HostCommand::StartRecording { .. })),
            "no recording may start without a fresh reading"
        );
        assert_eq!(
            plugin.run.as_ref().map(|run| run.phase),
            Some(RunPhase::Settling)
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_row_that_needs_a_filter_change_waits_for_the_operator() {
        let folder = temp_folder("pause");
        let protocol = write_protocol(
            &folder,
            "diff_on,diff_off,duration_s,settle_s,pause_before,optical_state\n\
             0,0,1,0,yes,LP647+BP700\n",
        );
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        assert_eq!(
            plugin.run.as_ref().map(|run| run.phase),
            Some(RunPhase::PausedForOperator)
        );
        assert!(
            sink.applied_biases().is_empty(),
            "nothing moves while paused"
        );
        assert!(plugin.message.contains("LP647+BP700"), "{}", plugin.message);

        plugin.continue_pending = true;
        tick(&mut plugin, vec![], &mut sink);
        assert_eq!(sink.applied_biases(), vec![(0, 0)]);

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_survey_refuses_to_start_while_a_filter_is_dropping_events() {
        let folder = temp_folder("filters");
        let protocol = write_protocol(&folder, "diff_on,diff_off\n0,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        plugin.event_filters = Some(EventFiltersV1 {
            stc_enabled: true,
            trail_enabled: false,
            erc_enabled: false,
        });
        let mut sink = ControlSink::default();
        plugin.start_pending = true;
        tick(&mut plugin, vec![], &mut sink);

        assert!(plugin.run.is_none(), "the survey must not start");
        assert!(sink.hosts.is_empty(), "nothing is sent to the host");
        assert!(plugin.message.contains("STC"), "{}", plugin.message);

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_survey_refuses_to_start_without_a_bias_readback_to_confirm_against() {
        // Without one, every point would record biases nobody can show were
        // live — the method's central claim would be uncheckable.
        let folder = temp_folder("noreadback");
        let protocol = write_protocol(&folder, "diff_on,diff_off\n0,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        plugin.sensor = Some(SensorMonitoringV1 {
            bias_codes: None,
            ..monitoring(0, 0, 0.1)
        });
        let mut sink = ControlSink::default();
        plugin.start_pending = true;
        tick(&mut plugin, vec![], &mut sink);

        assert!(plugin.run.is_none());
        assert!(plugin.message.contains("bias codes"), "{}", plugin.message);

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn an_invalid_protocol_is_refused_before_a_single_bias_moves() {
        let folder = temp_folder("badfile");
        let protocol = write_protocol(&folder, "diff_on,diff_off\n0,900\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        plugin.start_pending = true;
        tick(&mut plugin, vec![], &mut sink);

        assert!(plugin.run.is_none());
        assert!(sink.hosts.is_empty(), "nothing reached the host");
        assert!(
            plugin.message.contains("Protocol rejected"),
            "{}",
            plugin.message
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn stop_ends_the_run_and_still_restores_the_biases() {
        let folder = temp_folder("stop");
        let protocol = write_protocol(
            &folder,
            "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n15,15,1,0\n25,25,1,0\n",
        );
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);
        run_one_point(&mut plugin, &mut sink, &folder, 5, 5);

        plugin.stop_pending = true;
        tick(&mut plugin, vec![], &mut sink);
        tick(&mut plugin, vec![], &mut sink);

        // Point 2 had already been targeted when Stop arrived — it is dropped
        // before it records — and point 3 was never reached at all.
        assert_eq!(
            sink.applied_biases(),
            vec![(5, 5), (15, 15)],
            "Stop must not target another point"
        );
        assert!(
            matches!(
                sink.hosts.last().map(|request| &request.command),
                Some(HostCommand::RestoreCameraConfiguration),
            ),
            "a stopped run still puts the bench back"
        );
        settle_restore(&mut plugin, &mut sink);
        assert!(plugin.message.contains("stopped"), "{}", plugin.message);
        assert!(
            plugin.message.contains("1/3 recorded"),
            "{}",
            plugin.message
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_host_that_never_answers_does_not_strand_an_unattended_survey() {
        let folder = temp_folder("timeout");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        // Push the send far enough into the past to trip the reply timeout.
        if let Some(run) = plugin.run.as_mut() {
            run.last_activity_ms = now_unix_ms().saturating_sub(REPLY_TIMEOUT_MS + 1_000);
        }
        tick(&mut plugin, vec![], &mut sink);

        assert!(
            plugin
                .run
                .as_ref()
                .is_some_and(|r| r.index == 0 && r.retry_count == 1),
            "{}",
            plugin.message
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn a_reply_to_a_request_the_run_is_not_waiting_on_is_ignored() {
        // The runtime caches and can re-emit replies; a stale one must not
        // advance a point that is waiting on a different request.
        let folder = temp_folder("stalereply");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);

        let waiting_on = plugin.run.as_ref().and_then(|run| run.pending_request);
        tick(
            &mut plugin,
            vec![applied_reply(9_999, 5, 5, 0.1)],
            &mut sink,
        );
        assert_eq!(
            plugin.run.as_ref().and_then(|run| run.pending_request),
            waiting_on,
            "an unrelated reply must not settle the point"
        );
        assert_eq!(
            plugin.run.as_ref().map(|run| run.phase),
            Some(RunPhase::ApplyingBiases)
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn only_diff_on_and_diff_off_are_ever_changed_from_the_baseline() {
        // The host contract carries a whole configuration, so the freeze on
        // fo/hpf/refr/ROI/mask/trigger is no longer structural — A4 keeps it by
        // cloning the confirmed baseline. That is exactly what this asserts: a
        // point's configuration must differ from the baseline in two fields and
        // nowhere else, or a threshold sweep could silently move the ROI.
        let folder = temp_folder("narrow");
        let protocol = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n");
        let mut plugin = ready_plugin(&folder, &protocol);
        let mut sink = ControlSink::default();
        start_survey(&mut plugin, &mut sink);
        run_one_point(&mut plugin, &mut sink, &folder, 5, 5);

        let snapshots = sink.applied_snapshots();
        assert_eq!(snapshots.len(), 1, "one point, one configuration");
        let mut expected = baseline_snapshot();
        expected.biases.diff_on = 5;
        expected.biases.diff_off = 5;
        assert_eq!(
            snapshots[0], expected,
            "a point must change the two biases and copy everything else forward"
        );

        // The session is opened by confirming what the bench is on, never by
        // naming a profile — the survey measures the bench as it stands.
        assert!(
            sink.hosts.iter().any(|request| matches!(
                &request.command,
                HostCommand::ApplyCameraConfiguration {
                    configuration: CameraConfigurationSourceV1::Current
                }
            )),
            "the survey must open its session against the live configuration"
        );

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn measurement_ids_are_file_safe() {
        assert_eq!(sanitize_stem("a/b:c"), "a_b_c");
        assert_eq!(sanitize_stem("   "), "A4");
        // A generated id must already be file-safe, or every unnamed run would
        // silently be filed under a sanitized variant of its own name.
        let generated = generate_measurement_id();
        assert!(generated.starts_with("A4-"), "{generated}");
        assert_eq!(sanitize_stem(&generated), generated);
    }

    #[test]
    fn expected_codes_saturate_the_way_the_sensor_does() {
        assert_eq!(expected_code(102, 12), 114);
        assert_eq!(expected_code(10, -85), 0);
        assert_eq!(expected_code(250, 140), 255);
    }

    #[test]
    fn compact_utc_formats_a_known_epoch() {
        assert_eq!(format_compact_utc(1_767_225_600), "20260101-000000");
        assert_eq!(format_iso_utc(1_767_225_600), "2026-01-01T00:00:00Z");
    }
    #[test]
    fn retries_are_bounded_and_do_not_advance_the_point() {
        let folder = temp_folder("bounded");
        let path = write_protocol(
            &folder,
            "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n10,10,1,0\n",
        );
        let mut p = ready_plugin(&folder, &path);
        let mut sink = ControlSink::default();
        start_survey(&mut p, &mut sink);
        for _ in 0..3 {
            let id = sink.last_id();
            tick(
                &mut p,
                vec![rejected_reply(id, "busy", "temporary")],
                &mut sink,
            );
        }
        let r = p.run.as_ref().unwrap();
        assert_eq!(r.index, 0);
        assert_eq!(r.retry_count, 2);
        assert_eq!(r.phase, RunPhase::RecoveryPaused);
        assert_eq!(r.records.len(), 3);
        p.continue_pending = true;
        tick(&mut p, vec![], &mut sink);
        assert_eq!(p.run.as_ref().unwrap().index, 0);
        assert_eq!(p.run.as_ref().unwrap().phase, RunPhase::ApplyingBiases);
    }
    #[test]
    fn rejected_restore_retains_the_session_and_can_be_retried() {
        let folder = temp_folder("restore-recovery");
        let path = write_protocol(&folder, "diff_on,diff_off\n5,5\n");
        let mut p = ready_plugin(&folder, &path);
        let mut sink = ControlSink::default();
        start_survey(&mut p, &mut sink);
        p.stop_pending = true;
        tick(&mut p, vec![], &mut sink);
        let id = sink.last_id();
        tick(
            &mut p,
            vec![rejected_reply(id, "busy", "try again")],
            &mut sink,
        );
        assert_eq!(p.run.as_ref().unwrap().phase, RunPhase::RestoringBiases);
        p.continue_pending = true;
        tick(&mut p, vec![], &mut sink);
        assert_ne!(id, sink.last_id());
        settle_restore(&mut p, &mut sink);
        assert!(p.run.is_none());
    }
    #[test]
    fn non_finite_readback_cannot_start_a_recording() {
        for age in [f64::NAN, f64::INFINITY, -1.0] {
            let folder = temp_folder("invalid-age");
            let path = write_protocol(&folder, "diff_on,diff_off\n5,5\n");
            let mut p = ready_plugin(&folder, &path);
            let mut sink = ControlSink::default();
            start_survey(&mut p, &mut sink);
            let id = sink.last_id();
            tick(&mut p, vec![applied_reply(id, 5, 5, age)], &mut sink);
            assert_eq!(p.run.as_ref().unwrap().retry_count, 1);
            assert!(!sink
                .hosts
                .iter()
                .any(|h| matches!(h.command, HostCommand::StartRecording { .. })));
            std::fs::remove_dir_all(folder).unwrap();
        }
    }
    #[test]
    fn capture_requests_the_selected_absolute_root() {
        let folder = temp_folder("direct-root");
        let path = write_protocol(&folder, "diff_on,diff_off,duration_s,settle_s\n5,5,1,0\n");
        let mut p = ready_plugin(&folder, &path);
        let mut sink = ControlSink::default();
        start_survey(&mut p, &mut sink);
        let id = sink.last_id();
        tick(&mut p, vec![applied_reply(id, 5, 5, 0.1)], &mut sink);
        deliver_fresh_sensor(&mut p, monitoring(5, 5, 0.1));
        tick(&mut p, vec![], &mut sink);
        match &sink.hosts.last().unwrap().command {
            HostCommand::StartRecording {
                root_dir,
                base_path,
                ..
            } => {
                assert_eq!(root_dir.as_deref(), Some(folder.to_str().unwrap()));
                assert!(base_path.starts_with("A4-TEST/"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(p.set_setting("output_folder", json!("/tmp/other")).is_err());
    }
    #[test]
    fn compact_export_preserves_original_monitoring_csv() {
        let folder = temp_folder("telemetry-preserved");
        let path = write_protocol(&folder, "diff_on,diff_off\n5,5\n");
        let mut p = ready_plugin(&folder, &path);
        let mut sink = ControlSink::default();
        start_survey(&mut p, &mut sink);
        let raw = folder.join("sample.raw");
        let csv = folder.join("sample.sensor-monitoring.csv");
        std::fs::write(&csv, "unrecognized raw evidence\n").unwrap();
        p.gather_sensor_readout(&folder, &raw.to_string_lossy());
        assert_eq!(
            std::fs::read_to_string(csv).unwrap(),
            "unrecognized raw evidence\n"
        );
    }

    #[test]
    fn bright_reference_runs_three_raw_pd_pairs_and_restores_the_original_camera() {
        let folder = temp_folder("bright-e2e");
        let mut p = ready_plugin(&folder, Path::new(""));
        p.devices = crate::devices::Devices::simulated(now_unix_ms());
        let mut sink = ControlSink::default();
        p.start_pending = true;
        tick(&mut p, vec![], &mut sink);
        let mut handled = 0;
        let mut raw = String::new();
        let mut starts = 0;
        for _ in 0..200 {
            if let Some(request) = p.devices.tick(now_unix_ms()) {
                p.devices.simulate_reply(&request);
            }
            while handled < sink.hosts.len() {
                let h = sink.hosts[handled].clone();
                handled += 1;
                let reply = match h.command {
                    HostCommand::ApplyCameraConfiguration { .. } => {
                        applied_reply(h.request_id, BASELINE_ON as i64, BASELINE_OFF as i64, 0.1)
                    }
                    HostCommand::StartRecording {
                        root_dir,
                        base_path,
                        ..
                    } => {
                        raw = Path::new(root_dir.as_ref().unwrap())
                            .join(base_path)
                            .display()
                            .to_string();
                        std::fs::write(&raw, b"test-raw").unwrap();
                        starts += 1;
                        started_reply(h.request_id, &raw)
                    }
                    HostCommand::StopRecording => {
                        finalized_reply(h.request_id, &raw, 8, 120_000_000)
                    }
                    HostCommand::RestoreCameraConfiguration => restored_reply(h.request_id),
                };
                p.on_host_reply(&reply);
            }
            if let Some(r) = p.run.as_mut() {
                if r.phase == RunPhase::Settling {
                    r.point.settle_until_ms = 0;
                    r.point.saw_fresh_sensor = true;
                }
                if r.phase == RunPhase::Recording {
                    r.point.started_unix_ms = now_unix_ms().saturating_sub(121000);
                }
            }
            p.drive(&mut sink);
            if p.run.is_none() {
                break;
            }
        }
        assert!(
            p.run.is_none(),
            "{} {:?}",
            p.message,
            p.run.as_ref().map(|r| r.phase)
        );
        assert_eq!(starts, 3);
        assert!(p.message.contains("3/3 recorded"), "{}", p.message);
        for snapshot in sink.applied_snapshots() {
            assert_eq!(snapshot, baseline_snapshot());
        }
        let files = std::fs::read_dir(folder.join("A4-TEST"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(
            files
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "raw"))
                .count(),
            3
        );
        assert_eq!(
            files
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "pdq"))
                .count(),
            3
        );
        assert_eq!(
            files
                .iter()
                .filter(|p| p.to_string_lossy().ends_with(".devices.json"))
                .count(),
            3
        );
        assert!(p.devices.released());
    }

    #[test]
    fn aging_cached_telemetry_is_not_a_new_sensor_read() {
        let old = monitoring(5, 5, 0.1);
        assert!(!monitoring_is_new(Some(old), monitoring(5, 5, 0.2)));
        assert!(monitoring_is_new(Some(old), monitoring(5, 5, 0.01)));
        assert!(!monitoring_is_new(Some(old), monitoring(5, 5, f64::NAN)));
    }
}
