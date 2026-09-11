use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use augur_plugin_api::{
    CameraConfigurationProvenanceV1, CameraConfigurationSnapshotV1, CameraConfigurationSourceV1,
    EventStoreHandle, ExecutionContext, GlobalSettings, HostCommand, HostCommandOutcome,
    HostCommandRequest, HostContext, HostOutput, PathDialogKind, Plugin, PluginCapabilities,
    PluginControlContext, PluginControlInbox, PluginControlSnapshot, PluginDiscontinuity,
    PluginFrame, PluginInput, PluginRuntimeRole, PluginServiceOutcome, PluginServiceReply,
    PluginServiceRequest, SensorMonitoringV1, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, CTX_GLOBAL_SETTINGS, CTX_SENSOR_MONITORING,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use stage_a_plugin_contract::{
    A2AcquisitionConfigV1, A2TimingReferenceV1, ClientId, ConnectionStateV1, LeaseId,
    ModulationCommandV1, ModulationRequestV1, ModulationResponseV1, ModulationStateV1,
    PdqReceiptV1, PdqStartSpecV1, PdqTerminationV1, PhotodiodeCommandV1, PhotodiodeDarkReferenceV1,
    PhotodiodeLevelV1, PhotodiodePlacementV1, PhotodiodeRequestV1, PhotodiodeResponseV1,
    PhotodiodeSummaryV1, RequestId, RequestOutcomeV1, RunId, SemanticRevision, StreamIntegrityV1,
    WaveformV1, CTX_STAGE_A_MODULATION_STATE_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
    SERVICE_STAGE_A_MODULATION_CONTROL_V1, SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
};

use crate::protocol::{self, Acquisition, ControllerSetup, Point, Protocol, TriggerValidation};
use stage_a_universal_runner::CameraSettings;

const ID: &str = "stage-a.a2";
const MOD_ID: &str = "stage-a.modulation";
const PD_ID: &str = "stage-a.photodiode";
const TIMEOUT_MS: u64 = 20_000;
const LEASE_TTL_MS: u64 = 60_000;
/// Conservative fail-closed limit for the first small-ROI A2 runs. The actual
/// peak is written per point; H21 can later lower this bound without changing
/// every protocol file.
const RECORDER_SAFETY_LIMIT_EVENTS_PER_US: u64 = 6;

fn materialize_universal_protocol(name: &str) -> Result<Option<String>, String> {
    if let Some(path) = stage_a_universal_runner::materialize_protocol(name)? {
        return Ok(Some(path));
    }
    let (extension, contents) = match name {
        "a2_low_light_final" => (
            "toml",
            include_str!("../protocols/a2_production_drive_sync.toml"),
        ),
        "a2_heldout_blink_validation" => (
            "toml",
            include_str!("../protocols/a2_fluorescence_chain_followup.toml"),
        ),
        _ => return Ok(None),
    };
    let path = std::env::temp_dir().join(format!("stage-a-universal-{name}.{extension}"));
    std::fs::write(&path, contents).map_err(|error| error.to_string())?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Settling half-period guard, in microseconds.
///
/// `min_half_us` has to cover two independent things. The pixel's refractory
/// period `tau_refr` is bounded by `5 * pixel_dead_time_us` from the sensor's
/// own telemetry. The photoreceptor settling time `tau_p` is not reported by any
/// Stage-A telemetry, so this is the conservative bound A1 qualified for it at
/// the dim end of the flux ladder.
///
/// This is a *floor*. Raising it can only make the runner refuse more
/// protocols, never fewer, which is what makes resolving it safe — guessing a
/// lobe or a threshold would not be.
const SETTLING_GUARD_US: u32 = 1_000;

/// Millivolts at the top of DAC2's unbuffered output, which drives the
/// comparator's threshold input.
///
/// Deliberately *not* the photodiode ADC's 3300 mV reference. The two
/// converters have different references over the same 12-bit code range, so an
/// ADC code is never a threshold code; the only honest currency between them is
/// the physical voltage. See `a2_comparator_main.cpp`,
/// `thresholdCodeForMillivolt`.
const THRESHOLD_FULL_SCALE_MILLIVOLT: f64 = 2_500.0;
const THRESHOLD_MAX_CODE: u16 = 4_095;
/// Highest DAC code the stimulus converter accepts.
const STIMULUS_MAX_CODE: u16 = 4_095;

/// How long a commanded plateau is given to settle optically before its level
/// is even looked at. Mirrors the 200 ms window of the `a` bring-up command,
/// with margin for the owner's own 20 ms level window.
const PLATEAU_SETTLE_MS: u64 = 250;
/// How long after that a level window whose start provably follows the
/// acknowledged drive is waited for, before the point is refused.
const PLATEAU_LEVEL_TIMEOUT_MS: u64 = 5_000;
/// Eight independent owner windows, each at least 20 ms, after settling.
const PLATEAU_WINDOWS: usize = 8;
const MIN_LEVEL_WINDOW_SECONDS: f64 = 0.020;
/// Keep at least two threshold-DAC codes on either side of the midpoint.
const MIN_PLATEAU_SPAN_VOLTS: f64 = 4.0 * 2.5 / 4095.0;
const MAX_PLATEAU_MEAN_SPREAD_FRACTION: f64 = 0.25;

/// Quantized `(mean_u_milli, depth_a_milli)` pedestal a threshold is measured
/// for. The milli units are exactly what `A2AcquisitionConfigV1` carries to the
/// firmware, so two points that share a key really do share a drive.
type PedestalKey = (u32, u32);

#[derive(Default, Clone, Copy)]
struct Press {
    value: u64,
    seen: Option<u64>,
}
impl Press {
    fn accept(&mut self, v: &Value) -> bool {
        if v.as_bool() == Some(true) {
            self.value += 1;
            self.seen = Some(self.value);
            return true;
        }
        let Some(v) = v.as_u64() else {
            return false;
        };
        match self.seen {
            None => {
                self.seen = Some(v);
                self.value = self.value.max(v);
                false
            }
            Some(old) if v > old => {
                self.seen = Some(v);
                self.value = self.value.max(v);
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    ApplyCamera,
    AcquireMod,
    AcquirePd,
    /// Holding the dim plateau of this point's pedestal, awaiting the owner's
    /// acknowledgement of the constant drive.
    PlateauLowDrive,
    /// The dim plateau is commanded; waiting for a photodiode level window that
    /// provably began after that acknowledgement.
    PlateauLowLevel,
    PlateauHighDrive,
    PlateauHighLevel,
    Prepare,
    QuietBeforeCapture,
    StartStimulus,
    Settle,
    StartCamera,
    StartPd,
    Recording,
    StopMod,
    FinalizePd,
    StopCamera,
    ReleasePd,
    ReleaseMod,
    RestoreCamera,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Mod,
    Pd,
    Host,
    RenewMod,
    RenewPd,
}

#[derive(Debug, Default, Serialize)]
struct PointEvidence {
    rising_triggers: u64,
    falling_triggers: u64,
    expected_triggers_per_polarity: u64,
    peak_events_per_us: u64,
    raw_path: Option<String>,
    raw_sha256: Option<String>,
    camera_configuration_sidecar_path: Option<String>,
    sensor_monitoring_path: Option<String>,
    pdq_path: Option<String>,
    pd_sidecar_path: Option<String>,
    pdq_sha256: Option<String>,
    pdq_marker_counts: Option<stage_a_plugin_contract::PdqMarkerCountsV1>,
    marker_diagnostics_before: Option<stage_a_plugin_contract::A2MarkerDiagnosticsV1>,
    marker_diagnostics_after: Option<stage_a_plugin_contract::A2MarkerDiagnosticsV1>,
    /// `"auto"` or `"frozen"`: whether this point's threshold was measured by
    /// the runner or asserted by the protocol.
    comparator_threshold_mode: Option<&'static str>,
    /// The code actually sent on the `CMP thr=` path for this point.
    comparator_threshold_dac: Option<u16>,
    /// Full plateau evidence when the threshold was measured. Shared by every
    /// point of the same pedestal, and recorded on each of them so a single
    /// sidecar is self-contained.
    threshold_measurement: Option<MeasuredThreshold>,
    acquisition_complete: bool,
    valid: bool,
    /// Quality concerns do not imply file corruption. Strict protocols stop;
    /// diagnostic protocols retain these captures for explicit offline review.
    warnings: Vec<String>,
    failure: Option<String>,
}

/// Kind-1 controller values resolved from their owners at preflight, with the
/// provenance a run has to cite precisely because it no longer retypes them.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ResolvedController {
    /// Where the lobe came from. There is only one source; recording it saves a
    /// sidecar reader from having to know that.
    lobe_source: &'static str,
    v_null_dac: u16,
    v_peak_dac: u16,
    /// Which measured Pockels transfer calibration produced the lobe.
    modulation_calibration_id: String,
    modulation_owner_instance: String,
    min_half_us: u32,
    /// `"protocol"` or `"sensor_telemetry"`.
    min_half_us_source: &'static str,
    pixel_dead_time_us: Option<f32>,
    refractory_floor_us: u32,
    settling_guard_us: u32,
}

/// The two static levels an A2 log-square alternates between at one pedestal,
/// resolved from the owner's lobe by the same inversion the firmware applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
struct PedestalPlateaus {
    mean_u_milli: u32,
    depth_a_milli: u32,
    low_dac: u16,
    high_dac: u16,
}

/// One commanded plateau and the evidence tying a level window to it.
#[derive(Debug, Clone, Copy)]
struct PlateauStep {
    level_dac: u16,
    /// Photodiode sample index at the moment the owner acknowledged the drive.
    /// A level window is only accepted when it *starts* at or after this.
    acknowledged_sample_index: u64,
    stream_epoch: u64,
    level: Option<PhotodiodeLevelV1>,
    windows: [Option<PhotodiodeLevelV1>; PLATEAU_WINDOWS],
    window_count: usize,
}

impl PlateauStep {
    fn new(level_dac: u16) -> Self {
        Self {
            level_dac,
            acknowledged_sample_index: 0,
            stream_epoch: 0,
            level: None,
            windows: [None; PLATEAU_WINDOWS],
            window_count: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PlateauProbe {
    mean_u_milli: u32,
    depth_a_milli: u32,
    low: PlateauStep,
    high: PlateauStep,
}

/// A measured `V_50` for one pedestal, with the window provenance that proves
/// each plateau was read after its drive was acknowledged.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct MeasuredThreshold {
    mean_u_milli: u32,
    depth_a_milli: u32,
    low_level_dac: u16,
    high_level_dac: u16,
    low_plateau_volts: f64,
    high_plateau_volts: f64,
    low_window_peak_to_peak_volts: f64,
    high_window_peak_to_peak_volts: f64,
    low_windows: Vec<PhotodiodeLevelV1>,
    high_windows: Vec<PhotodiodeLevelV1>,
    low_mean_spread_volts: f64,
    high_mean_spread_volts: f64,
    mean_difference_standard_error_volts: f64,
    noisy_crossing_requires_review: bool,
    span_volts: f64,
    midpoint_volts: f64,
    threshold_dac: u16,
    threshold_millivolt: f64,
    low_window_sample_count: u64,
    low_window_end_sample_index: u64,
    low_drive_acknowledged_sample_index: u64,
    high_window_sample_count: u64,
    high_window_end_sample_index: u64,
    high_drive_acknowledged_sample_index: u64,
    stream_epoch: u64,
    measured_at_unix_ms: u64,
}

/// PD-owned state frozen when the run starts. This replaces optical and
/// reference IDs copied by hand into the protocol.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct ResolvedPhotodiode {
    owner_instance: String,
    placement: PhotodiodePlacementV1,
    splitter_fraction: Option<f64>,
    reference_set_id: Option<String>,
    load_ohms: Option<f64>,
    dark_reference: Option<PhotodiodeDarkReferenceV1>,
    sample_rate_hz: Option<u32>,
    stream_epoch: u64,
    stream_integrity: StreamIntegrityV1,
}

struct Run {
    protocol: Protocol,
    /// Runner-owned settings used for this acquisition. These are not repeated
    /// in every point protocol and are written verbatim into the sidecar.
    controller: ControllerSetup,
    photodiode_setup: ResolvedPhotodiode,
    /// Owner-resolved controller values, frozen once at preflight so every
    /// point of the run cites the same lobe and the same step floor.
    resolved: ResolvedController,
    /// Plateau DAC codes per pedestal that needs an auto threshold, computed at
    /// preflight so an unreachable plateau refuses before any hardware moves.
    pedestals: BTreeMap<PedestalKey, PedestalPlateaus>,
    /// Thresholds already measured this run, one per pedestal.
    thresholds: BTreeMap<PedestalKey, MeasuredThreshold>,
    /// The plateau measurement currently in flight, if any.
    probe: Option<PlateauProbe>,
    /// Hard deadline for a qualifying level window in the current plateau step.
    plateau_timeout_ms: u64,
    protocol_path: String,
    protocol_sha256: String,
    protocol_archive_path: String,
    measurement_id: String,
    output_root: PathBuf,
    attempt_id: String,
    completed_points: usize,
    resumed_points: BTreeSet<usize>,
    resume_pause: bool,
    pending_mod_request: Option<PluginServiceRequest>,
    last_mod_poll_ms: u64,
    index: usize,
    phase: Phase,
    pending: Option<(PendingKind, u64, u64)>,
    lease: LeaseId,
    /// Stable run identity bound to both owner leases for the complete protocol.
    lease_run_id: String,
    /// Per-point identity used for RAW, PDQ and sidecar file names.
    run_id: String,
    deadline_ms: u64,
    next_renew_ms: u64,
    stop: bool,
    abort_reason: Option<String>,
    mod_leased: bool,
    pd_leased: bool,
    camera_recording: bool,
    camera_stop_attempts: u8,
    pd_recording: bool,
    modulation_active: bool,
    camera_session_active: bool,
    restore_attempts: u8,
    camera_snapshot: Option<CameraConfigurationSnapshotV1>,
    camera_override_applied: bool,
    camera_provenance: Option<CameraConfigurationProvenanceV1>,
    camera_readback_age_s: Option<f64>,
    pause_acknowledged: bool,
    last_event_bin_us: Option<u64>,
    last_event_bin_count: u64,
    evidence: PointEvidence,
    review_points: usize,
    cleanup_failures: Vec<String>,
    pd_progress: Option<(u64, u64)>,
}

pub struct StageAA2Plugin {
    requester_plugin_id: String,
    enabled: bool,
    role: PluginRuntimeRole,
    output_folder: String,
    output_folder_override: Option<String>,
    measurement_id: String,
    protocol_path: String,
    protocol_preview: Option<Protocol>,
    camera_override: Option<CameraSettings>,
    universal_request: Option<stage_a_universal_runner::ExecuteBlockRequest>,
    universal_terminal: Option<&'static str>,
    new_id: Press,
    start: Press,
    stop: Press,
    continue_press: Press,
    start_pending: bool,
    stop_pending: bool,
    continue_pending: bool,
    run: Option<Run>,
    request: u64,
    revision: u64,
    message: String,
    modulation: Option<ModulationStateV1>,
    photodiode: Option<PhotodiodeSummaryV1>,
    settings: Option<GlobalSettings>,
    sensor: Option<SensorMonitoringV1>,
}

impl Default for StageAA2Plugin {
    fn default() -> Self {
        Self {
            requester_plugin_id: ID.into(),
            enabled: true,
            role: PluginRuntimeRole::LiveWorker,
            output_folder: String::new(),
            output_folder_override: None,
            measurement_id: String::new(),
            protocol_path: String::new(),
            protocol_preview: None,
            camera_override: None,
            universal_request: None,
            universal_terminal: None,
            new_id: Press::default(),
            start: Press::default(),
            stop: Press::default(),
            continue_press: Press::default(),
            start_pending: false,
            stop_pending: false,
            continue_pending: false,
            run: None,
            request: 0,
            revision: 0,
            message: "Choose an A2 point protocol".into(),
            modulation: None,
            photodiode: None,
            settings: None,
            sensor: None,
        }
    }
}

trait Control {
    fn service(&mut self, request: &PluginServiceRequest);
    fn host(&mut self, request: &HostCommandRequest);
}
impl Control for PluginControlContext<'_> {
    fn service(&mut self, request: &PluginServiceRequest) {
        let _ = self.request_service(request);
    }
    fn host(&mut self, request: &HostCommandRequest) {
        let _ = self.request_host(request);
    }
}

impl StageAA2Plugin {
    /// Override the requester identity when this recorder is embedded by a
    /// protocol adapter such as A5. Hardware owners validate this identity.
    pub fn set_requester_plugin_id(&mut self, plugin_id: impl Into<String>) {
        self.requester_plugin_id = plugin_id.into();
    }

    /// Apply a complete camera configuration supplied by an orchestration
    /// plugin before the delegated protocol starts.
    pub fn set_camera_override(&mut self, camera: Option<CameraSettings>) {
        self.camera_override = camera;
    }

    /// Use the campaign root supplied by an orchestration plugin instead of
    /// falling back to the photodiode owner's data directory.
    pub fn set_output_folder_override(&mut self, folder: Option<String>) {
        self.output_folder_override = folder;
    }

    fn next_id(&mut self) -> u64 {
        self.request += 1;
        self.request
    }
    fn next_revision(&mut self) -> SemanticRevision {
        self.revision += 1;
        SemanticRevision(self.revision)
    }

    fn blocker(&self) -> Option<String> {
        if self.role != PluginRuntimeRole::LiveWorker {
            return Some("A2 hardware effects are allowed only on the live worker".into());
        }
        if self.run.is_some() {
            return Some("an A2 protocol is already running".into());
        }
        if self.protocol_path.trim().is_empty() {
            return Some("choose an A2 protocol".into());
        }
        let Some(settings) = self.settings.as_ref() else {
            return Some("start live camera preview so host camera settings are available".into());
        };
        if settings.event_filters.stc_enabled
            || settings.event_filters.trail_enabled
            || settings.event_filters.erc_enabled
        {
            return Some("disable STC, Trail and ERC before A2".into());
        }
        if !matches!(
            self.modulation.as_ref().map(|s| &s.connection),
            Some(ConnectionStateV1::Connected { .. })
        ) {
            return Some("connect the Stage-A modulation owner".into());
        }
        if !matches!(
            self.photodiode.as_ref().map(|s| &s.connection),
            Some(ConnectionStateV1::Connected { .. })
        ) {
            return Some("connect the Stage-A photodiode owner".into());
        }
        if self
            .photodiode
            .as_ref()
            .is_none_or(|summary| summary.placement != PhotodiodePlacementV1::EmissionPath)
        {
            return Some("set the photodiode owner to emission_path".into());
        }
        if self
            .photodiode
            .as_ref()
            .and_then(|summary| summary.splitter_fraction)
            .is_none_or(|fraction| (fraction - 0.5).abs() > 1e-6)
        {
            return Some("set and confirm the photodiode splitter fraction to 0.5".into());
        }
        if self
            .photodiode
            .as_ref()
            .and_then(|summary| summary.data_dir.as_deref())
            .is_none_or(|folder| folder.trim().is_empty())
        {
            return Some("choose the data folder in the photodiode plugin".into());
        }
        if self
            .modulation
            .as_ref()
            .is_none_or(|state| state.optical_lobe.is_none())
        {
            return Some(
                "In the modulation plugin, apply the measured transfer curve with 'Apply to \
                 V_null / V_peak'. Do not set mean_u or depth_a there; A2 sets every measurement \
                 point from its protocol automatically."
                    .into(),
            );
        }
        None
    }

    fn begin(&mut self, control: &mut impl Control) {
        if self.run.is_some() {
            self.message = "A2 refused: an A2 protocol is already running".into();
            return;
        }
        let text = match std::fs::read_to_string(self.protocol_path.trim()) {
            Ok(v) => v,
            Err(e) => {
                self.message = format!("Cannot read protocol: {e}");
                return;
            }
        };
        let plan = match protocol::parse(&text) {
            Ok(v) => v,
            Err(e) => {
                self.message = format!("A2 protocol refused before hardware moved: {e}");
                return;
            }
        };
        if let Some(folder) = self.output_folder_override.clone() {
            self.output_folder = folder;
        } else if let Some(folder) = self.photodiode.as_ref().and_then(|pd| pd.data_dir.as_ref()) {
            self.output_folder = folder.clone();
        }
        let measurement_id = if self.measurement_id.trim().is_empty() {
            format!("A2-{}", compact_time())
        } else {
            self.measurement_id.trim().to_owned()
        };
        if !valid_measurement_id(&measurement_id) {
            self.message = "A2 refused: measurement id must use 1–100 letters, digits, '-' or '_' and must not be a reserved Windows filename".into();
            return;
        }
        let output_root = match resolve_output_root(&self.output_folder) {
            Ok(root) => root,
            Err(error) => {
                self.message = format!("A2 refused: {error}");
                return;
            }
        };
        self.output_folder = output_root.to_string_lossy().into_owned();
        self.protocol_preview = Some(plan.clone());
        self.measurement_id = measurement_id.clone();
        let hash = hex_hash(text.as_bytes());
        let resumed_points = match crate::resume::completed(
            &output_root.join(&measurement_id),
            &measurement_id,
            &hash,
            &plan,
        ) {
            Ok(points) => points,
            Err(error) => {
                self.message = format!("A2 refused: cannot inspect existing measurement: {error}");
                return;
            }
        };
        let Some(index) = (0..plan.points.len()).find(|i| !resumed_points.contains(i)) else {
            self.message = format!(
                "A2 {}: all {} protocol points already complete; nothing to record",
                measurement_id,
                plan.points.len()
            );
            return;
        };
        let attempt_id = compact_time();
        let journal = output_root
            .join(&measurement_id)
            .join(format!("{attempt_id}_progress.jsonl"));
        if journal.exists() {
            self.message =
                "A2 refused: this run already exists; start again to create a new run".into();
            return;
        }
        if let Some(blocker) = self.blocker() {
            self.message = format!("A2 refused: {blocker}");
            return;
        }
        // Everything below happens before the camera configuration is applied and
        // before either owner lease is acquired: an unresolvable lobe, an
        // unresolvable step floor or an unreachable plateau must refuse while
        // the bench is still untouched.
        let controller = ControllerSetup {
            min_half_us: (plan.timing_reference == A2TimingReferenceV1::DriveSync).then_some(0),
            ..ControllerSetup::default()
        };
        let photodiode_setup = resolve_photodiode(
            self.photodiode
                .as_ref()
                .expect("blocker confirmed the photodiode owner"),
        );
        let resolved = match resolve_controller(&controller, self.modulation.as_ref(), self.sensor)
        {
            Ok(resolved) => resolved,
            Err(error) => {
                self.message = format!("A2 refused before hardware moved: {error}");
                return;
            }
        };
        if let Err(error) = check_half_periods(&plan, resolved.min_half_us) {
            self.message = format!("A2 refused before hardware moved: {error}");
            return;
        }
        let pedestals = match plan_pedestals(&plan, &resolved) {
            Ok(pedestals) => pedestals,
            Err(error) => {
                self.message = format!("A2 refused before hardware moved: {error}");
                return;
            }
        };
        let protocol_archive_path =
            match archive_protocol(&self.output_folder, &measurement_id, &hash, text.as_bytes()) {
                Ok(path) => path,
                Err(error) => {
                    self.message = format!("A2 refused: cannot archive exact protocol: {error}");
                    return;
                }
            };
        self.run = Some(Run {
            protocol: plan,
            controller,
            photodiode_setup,
            resolved,
            pedestals,
            thresholds: BTreeMap::new(),
            probe: None,
            plateau_timeout_ms: 0,
            protocol_path: self.protocol_path.clone(),
            protocol_sha256: hash,
            protocol_archive_path,
            measurement_id: measurement_id.clone(),
            output_root,
            attempt_id,
            completed_points: 0,
            resume_pause: !resumed_points.is_empty(),
            resumed_points,
            pending_mod_request: None,
            last_mod_poll_ms: 0,
            index,
            phase: Phase::ApplyCamera,
            pending: None,
            lease: LeaseId::new(format!("a2-{}", now_ms())),
            lease_run_id: measurement_id.clone(),
            run_id: String::new(),
            deadline_ms: 0,
            next_renew_ms: now_ms() + 30_000,
            stop: false,
            abort_reason: None,
            mod_leased: false,
            pd_leased: false,
            camera_recording: false,
            camera_stop_attempts: 0,
            pd_recording: false,
            modulation_active: false,
            camera_session_active: true,
            restore_attempts: 0,
            camera_snapshot: None,
            camera_override_applied: false,
            camera_provenance: None,
            camera_readback_age_s: None,
            pause_acknowledged: false,
            last_event_bin_us: None,
            last_event_bin_count: 0,
            evidence: PointEvidence::default(),
            review_points: 0,
            cleanup_failures: Vec::new(),
            pd_progress: None,
        });
        if let Err(error) = self.append_progress("run_started") {
            self.message = format!("A2 refused: cannot save run progress: {error}");
            self.run = None;
            return;
        }
        self.send_host(
            control,
            HostCommand::ApplyCameraConfiguration {
                configuration: CameraConfigurationSourceV1::Current,
            },
        );
    }

    fn point(&self) -> Option<&Point> {
        self.run
            .as_ref()?
            .protocol
            .points
            .get(self.run.as_ref()?.index)
    }

    fn send_mod(
        &mut self,
        control: &mut impl Control,
        command: ModulationCommandV1,
        revision: bool,
    ) {
        let pending_kind = if matches!(command, ModulationCommandV1::RenewLease { .. }) {
            PendingKind::RenewMod
        } else {
            PendingKind::Mod
        };
        let request_id = self.next_id();
        let (lease, lease_run_id, owner) = {
            let run = self.run.as_ref().unwrap();
            (
                run.lease.clone(),
                run.lease_run_id.clone(),
                self.modulation.as_ref().map(|s| s.owner_instance.clone()),
            )
        };
        let mut e = ModulationRequestV1::new(
            RequestId(request_id),
            ClientId::new(self.requester_plugin_id.clone()),
            command,
        );
        e.lease_id = Some(lease);
        e.target_owner_instance = owner;
        e.issued_at_unix_ms = now_ms();
        e.run_id = Some(RunId::new(lease_run_id));
        if revision {
            e.requested_revision = Some(self.next_revision());
        }
        let request = PluginServiceRequest {
            request_id,
            source_plugin_id: self.requester_plugin_id.clone(),
            target_plugin_id: MOD_ID.into(),
            service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
            payload: serde_json::to_value(e).unwrap(),
        };
        control.service(&request);
        let run = self.run.as_mut().unwrap();
        run.pending_mod_request = Some(request);
        run.last_mod_poll_ms = now_ms();
        run.pending = Some((pending_kind, request_id, now_ms()));
    }

    fn send_pd(
        &mut self,
        control: &mut impl Control,
        command: PhotodiodeCommandV1,
        revision: bool,
    ) {
        let pending_kind = if matches!(command, PhotodiodeCommandV1::RenewLease { .. }) {
            PendingKind::RenewPd
        } else {
            PendingKind::Pd
        };
        let request_id = self.next_id();
        let (lease, lease_run_id, owner) = {
            let run = self.run.as_ref().unwrap();
            (
                run.lease.clone(),
                run.lease_run_id.clone(),
                self.photodiode.as_ref().map(|s| s.owner_instance.clone()),
            )
        };
        let mut e = PhotodiodeRequestV1::new(
            RequestId(request_id),
            ClientId::new(self.requester_plugin_id.clone()),
            command,
        );
        e.lease_id = Some(lease);
        e.target_owner_instance = owner;
        e.issued_at_unix_ms = now_ms();
        e.run_id = Some(RunId::new(lease_run_id));
        if revision {
            e.requested_revision = Some(self.next_revision());
        }
        control.service(&PluginServiceRequest {
            request_id,
            source_plugin_id: self.requester_plugin_id.clone(),
            target_plugin_id: PD_ID.into(),
            service: SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1.into(),
            payload: serde_json::to_value(e).unwrap(),
        });
        self.run.as_mut().unwrap().pending = Some((pending_kind, request_id, now_ms()));
    }

    fn send_host(&mut self, control: &mut impl Control, command: HostCommand) {
        let id = self.next_id();
        control.host(&HostCommandRequest {
            request_id: id,
            command,
        });
        self.run.as_mut().unwrap().pending = Some((PendingKind::Host, id, now_ms()));
    }

    fn prepare(&mut self, control: &mut impl Control) {
        let p = {
            let r = self.run.as_ref().unwrap();
            r.protocol.points[r.index].clone()
        };
        let run = self.run.as_mut().unwrap();
        run.phase = Phase::Prepare;
        run.run_id = format!(
            "{}_r{:03}_{}_{}",
            run.measurement_id,
            run.index + 1,
            safe(&p.label),
            run.attempt_id
        );
        run.evidence = PointEvidence::default();
        run.last_event_bin_us = None;
        run.last_event_bin_count = 0;
        run.camera_stop_attempts = 0;
        run.probe = None;
        if should_pause(&p, self.run.as_ref().unwrap().pause_acknowledged)
            || (self.run.as_ref().unwrap().resume_pause
                && !self.run.as_ref().unwrap().pause_acknowledged)
        {
            self.run.as_mut().unwrap().phase = Phase::Paused;
            self.message = pause_message(&p);
            return;
        }
        self.run.as_mut().unwrap().resume_pause = false;
        match p.acquisition {
            Acquisition::Dark { .. } => self.send_mod(
                control,
                ModulationCommandV1::SafeOff {
                    reason: "A2 dark acquisition: force modulation safe/off".into(),
                },
                true,
            ),
            Acquisition::Stepped {
                mean_u,
                depth_a,
                comparator_threshold_dac,
                ..
            } => {
                if self.run.as_ref().unwrap().protocol.timing_reference
                    == A2TimingReferenceV1::DriveSync
                {
                    self.run
                        .as_mut()
                        .unwrap()
                        .evidence
                        .comparator_threshold_mode = Some("unused_drive_sync");
                    self.send_prepare_a2(control, 0);
                    return;
                }
                let key = pedestal_key(mean_u, depth_a);
                self.run
                    .as_mut()
                    .unwrap()
                    .evidence
                    .comparator_threshold_mode = Some(comparator_threshold_dac.mode());
                if let Some(code) = comparator_threshold_dac.frozen_code() {
                    self.send_prepare_a2(control, code);
                    return;
                }
                let plateaus = self
                    .run
                    .as_ref()
                    .and_then(|run| run.pedestals.get(&key).copied());
                let Some(plateaus) = plateaus else {
                    // Preflight computed a plateau pair for every auto point,
                    // so a miss here means the point set changed under us.
                    self.fail(
                        control,
                        format!(
                            "no preflight plateau pair for pedestal mean_u={} m, a={} m",
                            key.0, key.1
                        ),
                    );
                    return;
                };
                self.start_plateau_probe(control, plateaus);
            }
        }
    }

    /// Sends `PrepareA2` for the current point with an already-decided
    /// comparator threshold. The owner turns this into `CFG mode=A2`,
    /// `CMP thr=…` and `MOD wave=LOG_SQUARE …`.
    fn send_prepare_a2(&mut self, control: &mut impl Control, threshold_dac: u16) {
        let (point, controller, resolved) = {
            let r = self.run.as_ref().unwrap();
            (
                r.protocol.points[r.index].acquisition.clone(),
                r.controller.clone(),
                r.resolved.clone(),
            )
        };
        let Acquisition::Stepped {
            mean_u,
            depth_a,
            half_period_s,
            transitions_per_polarity,
            ..
        } = point
        else {
            self.fail(
                control,
                "internal: PrepareA2 requested for a non-stepped point".into(),
            );
            return;
        };
        let hz = 1.0 / (2.0 * half_period_s);
        let configuration = A2AcquisitionConfigV1 {
            timing_reference: self.run.as_ref().unwrap().protocol.timing_reference,
            mean_u_milli: (mean_u * 1000.0).round() as u32,
            depth_a_milli: (depth_a * 1000.0).round() as u32,
            frequency_millihz: (hz * 1000.0).round() as u64,
            min_half_us: resolved.min_half_us,
            v_null_dac: resolved.v_null_dac,
            v_peak_dac: resolved.v_peak_dac,
            comparator_threshold_dac: threshold_dac,
            comparator_hysteresis: controller.comparator_hysteresis,
            comparator_invert: controller.comparator_invert,
            sample_rate_hz: controller.sample_rate_hz,
            block_samples: controller.block_samples,
            emit_raw_samples: true,
            emit_summary: true,
        };
        {
            let run = self.run.as_mut().unwrap();
            run.phase = Phase::Prepare;
            run.modulation_active = true;
            run.probe = None;
            run.evidence.expected_triggers_per_polarity = u64::from(transitions_per_polarity);
            run.evidence.comparator_threshold_dac = (run.protocol.timing_reference
                == A2TimingReferenceV1::Comparator)
                .then_some(threshold_dac);
        }
        self.send_mod(
            control,
            ModulationCommandV1::PrepareA2 { configuration },
            true,
        );
    }

    /// Holds the dim plateau of this pedestal. Mirrors the `a` command of the
    /// `a2_comparator` bring-up image, one static level at a time so each
    /// plateau gets its own settled level window rather than a min/max over a
    /// running square.
    fn start_plateau_probe(&mut self, control: &mut impl Control, plateaus: PedestalPlateaus) {
        {
            let run = self.run.as_mut().unwrap();
            run.probe = Some(PlateauProbe {
                mean_u_milli: plateaus.mean_u_milli,
                depth_a_milli: plateaus.depth_a_milli,
                low: PlateauStep::new(plateaus.low_dac),
                high: PlateauStep::new(plateaus.high_dac),
            });
            run.phase = Phase::PlateauLowDrive;
            run.modulation_active = true;
        }
        self.message = format!(
            "Measuring V50 at mean_u={} m, a={} m: holding the dim plateau (DAC {})",
            plateaus.mean_u_milli, plateaus.depth_a_milli, plateaus.low_dac
        );
        self.send_mod(
            control,
            ModulationCommandV1::SetWaveform {
                waveform: WaveformV1::Constant {
                    level_dac: plateaus.low_dac,
                },
            },
            true,
        );
    }

    /// The owner acknowledged a constant plateau drive. Records where the
    /// photodiode sample clock stood at that moment, so a level window can
    /// later be *proven* to have begun after the drive rather than assumed to.
    fn plateau_drive_acknowledged(&mut self, control: &mut impl Control, low: bool) {
        let observed = self.photodiode.as_ref().and_then(|pd| {
            pd.stream
                .sample_range
                .map(|range| (range.end_sample_index_exclusive, pd.stream.stream_epoch))
        });
        let Some((acknowledged_sample_index, stream_epoch)) = observed else {
            self.fail(
                control,
                "photodiode owner published no sample range, so a plateau level cannot be tied to \
                 the acknowledged drive"
                    .into(),
            );
            return;
        };
        if self.run.as_ref().is_none_or(|run| run.probe.is_none()) {
            self.fail(
                control,
                "internal: plateau acknowledgement without an active probe".into(),
            );
            return;
        }
        let Some(_rate) = self
            .photodiode
            .as_ref()
            .and_then(|pd| pd.stream.sample_rate_hz)
            .filter(|r| *r > 0)
        else {
            self.fail(
                control,
                "photodiode sample rate is unavailable during V50 measurement".into(),
            );
            return;
        };
        let now = now_ms();
        let run = self.run.as_mut().unwrap();
        if let Some(probe) = run.probe.as_mut() {
            let step = if low { &mut probe.low } else { &mut probe.high };
            step.acknowledged_sample_index = acknowledged_sample_index;
            // The first accepted window must begin after the full settling guard.
            step.windows = [None; PLATEAU_WINDOWS];
            step.window_count = 0;
            step.stream_epoch = stream_epoch;
        }
        run.phase = if low {
            Phase::PlateauLowLevel
        } else {
            Phase::PlateauHighLevel
        };
        run.deadline_ms = now + PLATEAU_SETTLE_MS;
        run.plateau_timeout_ms = now + PLATEAU_SETTLE_MS + PLATEAU_LEVEL_TIMEOUT_MS;
    }

    /// Waits for a settled photodiode level whose whole averaging window lies
    /// after the acknowledged drive, on the same stream segment.
    fn poll_plateau(&mut self, control: &mut impl Control) {
        let Some((phase, deadline, timeout)) = self
            .run
            .as_ref()
            .map(|run| (run.phase, run.deadline_ms, run.plateau_timeout_ms))
        else {
            return;
        };
        let low = phase == Phase::PlateauLowLevel;
        let now = now_ms();
        if now < deadline {
            return;
        }
        let step = self.run.as_ref().and_then(|run| run.probe).map(|probe| {
            if low {
                probe.low
            } else {
                probe.high
            }
        });
        let Some(step) = step else {
            self.fail(
                control,
                "internal: plateau level polled without an active probe".into(),
            );
            return;
        };
        let observed = self
            .photodiode
            .as_ref()
            .map(|pd| (pd.stream.stream_epoch, pd.stream.level));
        let Some((stream_epoch, level)) = observed else {
            self.fail(
                control,
                "photodiode owner state disappeared during the plateau measurement".into(),
            );
            return;
        };
        if stream_epoch != step.stream_epoch {
            self.fail(
                control,
                format!(
                    "photodiode stream restarted during the plateau measurement (epoch {} -> \
                     {stream_epoch}); the level window cannot be tied to the drive",
                    step.stream_epoch
                ),
            );
            return;
        }
        let rate = self
            .photodiode
            .as_ref()
            .and_then(|pd| pd.stream.sample_rate_hz)
            .unwrap_or(0);
        let settled_index = step
            .acknowledged_sample_index
            .saturating_add(u64::from(rate) * PLATEAU_SETTLE_MS / 1000);
        let previous_end = step
            .windows
            .iter()
            .flatten()
            .last()
            .map_or(settled_index, |window| window.end_sample_index);
        let qualified = level.filter(|level| {
            rate > 0
                && level.sample_count >= (f64::from(rate) * MIN_LEVEL_WINDOW_SECONDS).ceil() as u64
                && level.end_sample_index >= level.sample_count
                && level.end_sample_index - level.sample_count >= previous_end
        });
        let Some(level) = qualified else {
            if now >= timeout {
                self.fail(
                    control,
                    format!(
                        "V50 {} plateau at DAC {}: only {}/{} fresh non-overlapping 20 ms windows \
                         began after the acknowledged drive and 250 ms settling guard within {} ms; \
                         last window end={:?}, required start >= {previous_end}. Check the PD stream for stale or short windows",
                        if low { "dim" } else { "bright" }, step.level_dac, step.window_count,
                        PLATEAU_WINDOWS, PLATEAU_LEVEL_TIMEOUT_MS, level.map(|v| v.end_sample_index)
                    ),
                );
            }
            return;
        };
        if level.clipped {
            self.fail(
                control,
                format!(
                    "plateau at DAC {} clips the photodiode ADC, so its mean is a truncated \
                     estimate and cannot anchor V50",
                    step.level_dac
                ),
            );
            return;
        }
        if !level.mean_volts.is_finite()
            || !level.peak_to_peak_volts.is_finite()
            || level.peak_to_peak_volts < 0.0
        {
            self.fail(
                control,
                "photodiode plateau contains invalid voltage statistics".into(),
            );
            return;
        }
        let aggregate = {
            let probe = self.run.as_mut().unwrap().probe.as_mut().unwrap();
            let step = if low { &mut probe.low } else { &mut probe.high };
            step.windows[step.window_count] = Some(level);
            step.window_count += 1;
            if step.window_count < PLATEAU_WINDOWS {
                return;
            }
            aggregate_levels(step)
        };
        let level = aggregate;
        if low {
            let high_dac = {
                let run = self.run.as_mut().unwrap();
                if let Some(probe) = run.probe.as_mut() {
                    probe.low.level = Some(level);
                }
                run.probe.map(|probe| probe.high.level_dac)
            };
            let Some(high_dac) = high_dac else {
                self.fail(
                    control,
                    "internal: plateau probe vanished between levels".into(),
                );
                return;
            };
            self.run.as_mut().unwrap().phase = Phase::PlateauHighDrive;
            self.message = format!("Measuring V50: holding the bright plateau (DAC {high_dac})");
            self.send_mod(
                control,
                ModulationCommandV1::SetWaveform {
                    waveform: WaveformV1::Constant {
                        level_dac: high_dac,
                    },
                },
                true,
            );
            return;
        }
        if let Some(probe) = self.run.as_mut().and_then(|run| run.probe.as_mut()) {
            probe.high.level = Some(level);
        }
        self.finish_plateau_probe(control);
    }

    /// Turns two settled plateaus into one comparator threshold, or refuses.
    fn finish_plateau_probe(&mut self, control: &mut impl Control) {
        let probe = self.run.as_ref().and_then(|run| run.probe);
        let Some(probe) = probe else {
            self.fail(
                control,
                "internal: plateau probe vanished before its threshold was computed".into(),
            );
            return;
        };
        let (Some(low), Some(high)) = (probe.low.level, probe.high.level) else {
            self.fail(
                control,
                "internal: plateau probe completed without both levels".into(),
            );
            return;
        };
        let measured = match measured_threshold(&probe, low, high) {
            Ok(measured) => measured,
            Err(error) => {
                self.fail(control, error);
                return;
            }
        };
        let code = measured.threshold_dac;
        if measured.noisy_crossing_requires_review {
            self.run.as_mut().unwrap().evidence.warnings.push(format!(
                "Photodiode raw noise is large: dim/bright peak-to-peak {:.1}/{:.1} mV, step {:.1} mV. \
                 The averaged V50 is resolved; individual trigger timing needs offline review",
                measured.low_window_peak_to_peak_volts * 1000.0,
                measured.high_window_peak_to_peak_volts * 1000.0, measured.span_volts * 1000.0));
        }
        self.message = format!(
            "V50 at mean_u={} m, a={} m: {:.1} mV span, threshold DAC {code}",
            measured.mean_u_milli,
            measured.depth_a_milli,
            measured.span_volts * 1000.0
        );
        {
            let run = self.run.as_mut().unwrap();
            run.thresholds.insert(
                (measured.mean_u_milli, measured.depth_a_milli),
                measured.clone(),
            );
            run.evidence.threshold_measurement = Some(measured);
        }
        self.send_prepare_a2(control, code);
    }

    fn metadata(&self) -> BTreeMap<String, String> {
        let r = self.run.as_ref().unwrap();
        let p = &r.protocol.points[r.index];
        let mut m = BTreeMap::new();
        for (k, v) in [
            ("experiment", "A2".into()),
            ("measurement_id", r.measurement_id.clone()),
            ("protocol_path", r.protocol_path.clone()),
            ("protocol_sha256", r.protocol_sha256.clone()),
            ("protocol_name", r.protocol.name.clone()),
            ("protocol_row", (r.index + 1).to_string()),
            ("label", p.label.clone()),
            ("role", p.role.clone()),
            ("acquisition_mode", acquisition_mode(&p.acquisition).into()),
            ("duration_s", p.acquisition_seconds().to_string()),
            ("transfer_scope", "fluorescence_chain".into()),
            ("scientific_status", "requires_offline_h4_h5_review".into()),
            ("photodiode_timebase", "continuous_dma_no_START".into()),
            (
                "sync_onset",
                if r.protocol.timing_reference == A2TimingReferenceV1::DriveSync {
                    "quiet_then_drive_after_both_recorders_open"
                } else {
                    "continuous_comparator"
                }
                .into(),
            ),
            (
                "timing_reference",
                format!("{:?}", r.protocol.timing_reference),
            ),
            (
                "pd_marker_sample_index",
                if r.evidence
                    .marker_diagnostics_before
                    .is_some_and(|d| d.dma_sample_clock)
                {
                    "dma_cursor_v1"
                } else {
                    "unverified_or_foreground_estimate"
                }
                .into(),
            ),
            (
                "trigger_validation",
                format!("{:?}", r.protocol.trigger_validation),
            ),
            (
                "photodiode_placement",
                photodiode_placement_name(r.photodiode_setup.placement).into(),
            ),
            // Owner-resolved, not retyped: the run still cites the lobe and the
            // step floor it actually ran on.
            ("v_null_dac", r.resolved.v_null_dac.to_string()),
            ("v_peak_dac", r.resolved.v_peak_dac.to_string()),
            ("lobe_source", r.resolved.lobe_source.into()),
            (
                "modulation_calibration_id",
                r.resolved.modulation_calibration_id.clone(),
            ),
            ("min_half_us", r.resolved.min_half_us.to_string()),
            ("min_half_us_source", r.resolved.min_half_us_source.into()),
        ] {
            m.insert(k.into(), v);
        }
        if let Some(fraction) = r.photodiode_setup.splitter_fraction {
            m.insert("splitter_fraction_to_pd".into(), fraction.to_string());
        }
        if let Some(reference_set_id) = r.photodiode_setup.reference_set_id.as_ref() {
            m.insert(
                "photodiode_reference_set_id".into(),
                reference_set_id.clone(),
            );
        }
        if let Some(load_ohms) = r.photodiode_setup.load_ohms {
            m.insert("photodiode_load_ohms".into(), load_ohms.to_string());
        }
        if let Some(reference) = r.photodiode_setup.dark_reference.as_ref() {
            m.insert("photodiode_dark_id".into(), reference.dark_id.clone());
            m.insert(
                "photodiode_dark_volts".into(),
                reference.dark_volts.to_string(),
            );
        }
        match p.acquisition {
            Acquisition::Dark { duration_s } => {
                m.insert("dark_duration_s".into(), duration_s.to_string());
            }
            Acquisition::Stepped {
                mean_u,
                depth_a,
                half_period_s,
                transitions_per_polarity,
                comparator_threshold_dac,
            } => {
                for (key, value) in [
                    ("mean_u", mean_u.to_string()),
                    ("depth_a_commanded", depth_a.to_string()),
                    ("half_period_s", half_period_s.to_string()),
                    (
                        "transitions_per_polarity",
                        transitions_per_polarity.to_string(),
                    ),
                    (
                        "comparator_threshold_mode",
                        comparator_threshold_dac.mode().into(),
                    ),
                ] {
                    m.insert(key.into(), value);
                }
                // The code that was actually sent on the `CMP thr=` path. Absent
                // only if this were ever reached before the threshold resolved,
                // which the phase order rules out.
                if let Some(code) = r.evidence.comparator_threshold_dac {
                    m.insert("comparator_threshold_dac".into(), code.to_string());
                }
                if let Some(measurement) = r.evidence.threshold_measurement.as_ref() {
                    for (key, value) in [
                        (
                            "v50_low_plateau_volts",
                            measurement.low_plateau_volts.to_string(),
                        ),
                        (
                            "v50_high_plateau_volts",
                            measurement.high_plateau_volts.to_string(),
                        ),
                        ("v50_span_volts", measurement.span_volts.to_string()),
                        (
                            "v50_low_window_end_sample_index",
                            measurement.low_window_end_sample_index.to_string(),
                        ),
                        (
                            "v50_high_window_end_sample_index",
                            measurement.high_window_end_sample_index.to_string(),
                        ),
                    ] {
                        m.insert(key.into(), value);
                    }
                }
            }
        }
        m
    }

    fn advance(&mut self, control: &mut impl Control) {
        let done = {
            let r = self.run.as_mut().unwrap();
            r.completed_points += 1;
            r.pause_acknowledged = false;
            let next =
                (r.index + 1..r.protocol.points.len()).find(|i| !r.resumed_points.contains(i));
            if !r.stop {
                if let Some(next) = next {
                    r.resume_pause = r.protocol.points[r.index + 1..next]
                        .iter()
                        .any(|p| p.pause_before);
                    r.index = next;
                }
            }
            next.is_none() || r.stop
        };
        if done {
            self.release_next(control);
        } else {
            self.prepare(control);
        }
    }

    fn release_next(&mut self, control: &mut impl Control) {
        if self.run.as_ref().is_some_and(|r| {
            r.abort_reason.is_some() && !r.run_id.is_empty() && r.index < r.protocol.points.len()
        }) {
            if let Err(error) = self.write_sidecar() {
                let run = self.run.as_mut().unwrap();
                let problem = format!("cannot save A2 sidecar: {error}");
                if !run.cleanup_failures.contains(&problem) {
                    run.cleanup_failures.push(problem);
                }
            }
        }
        let Some(run) = self.run.as_ref() else { return };
        if run.pd_leased {
            self.run.as_mut().unwrap().phase = Phase::ReleasePd;
            self.send_pd(
                control,
                PhotodiodeCommandV1::ReleaseLease {
                    finalize_recording: true,
                    reason: "A2 cleanup".into(),
                },
                false,
            );
        } else if run.mod_leased {
            self.run.as_mut().unwrap().phase = Phase::ReleaseMod;
            self.send_mod(
                control,
                ModulationCommandV1::ReleaseLease {
                    safe_off: true,
                    reason: "A2 cleanup".into(),
                },
                true,
            );
        } else if run.camera_session_active {
            if run.restore_attempts >= 3 {
                self.message = format!(
                    "{}; camera restore was not confirmed after 3 attempts",
                    self.message
                );
                self.run
                    .as_mut()
                    .unwrap()
                    .cleanup_failures
                    .push("camera restore was not confirmed after 3 attempts".into());
                self.run.as_mut().unwrap().camera_session_active = false;
                self.finish_run();
                return;
            }
            self.run.as_mut().unwrap().phase = Phase::RestoreCamera;
            self.run.as_mut().unwrap().restore_attempts += 1;
            self.send_host(control, HostCommand::RestoreCameraConfiguration);
        } else {
            self.finish_run();
        }
    }

    fn stop_camera(&mut self, control: &mut impl Control) {
        let run = self.run.as_mut().unwrap();
        run.phase = Phase::StopCamera;
        run.camera_stop_attempts = run.camera_stop_attempts.saturating_add(1);
        self.send_host(control, HostCommand::StopRecording);
    }

    fn finish_run(&mut self) {
        if self.run.as_ref().is_some_and(|r| !r.run_id.is_empty()) {
            if let Err(error) = self.write_sidecar() {
                self.run
                    .as_mut()
                    .unwrap()
                    .abort_reason
                    .get_or_insert(format!("cannot save final A2 sidecar: {error}"));
            }
        }
        if let Err(error) = self.append_progress("run_finished") {
            if let Some(run) = self.run.as_mut() {
                run.abort_reason
                    .get_or_insert(format!("cannot save final progress: {error}"));
            }
        }
        if let Some(run) = self.run.take() {
            self.universal_terminal = Some(
                if run.abort_reason.is_none()
                    && run.cleanup_failures.is_empty()
                    && run.completed_points + run.resumed_points.len() == run.protocol.points.len()
                {
                    "completed"
                } else {
                    "failed"
                },
            );
            self.message = if let Some(reason) = run.abort_reason {
                format!(
                    "A2 stopped: {reason}. Files retained in {}/{}",
                    self.output_folder, run.measurement_id
                )
            } else if run.review_points > 0 {
                format!(
                    "A2 capture finished: {}/{} points recorded; {} point(s) require offline timing review. Files: {}/{}",
                    run.completed_points + run.resumed_points.len(),
                    run.protocol.points.len(),
                    run.review_points,
                    self.output_folder,
                    run.measurement_id
                )
            } else {
                format!(
                    "A2 acquisition checks passed; H4/H5 and offline first-event analysis remain required. Files: {}/{}",
                    self.output_folder, run.measurement_id
                )
            };
            if !run.cleanup_failures.is_empty() {
                self.message.push_str(&format!(
                    "; CLEANUP NOT CONFIRMED: {}",
                    run.cleanup_failures.join("; ")
                ));
            }
        }
    }

    fn fail(&mut self, control: &mut impl Control, reason: String) {
        let Some(run) = self.run.as_mut() else { return };
        let reason = format!(
            "point {} '{}' ({:?}): {reason}",
            run.index + 1,
            run.protocol
                .points
                .get(run.index)
                .map_or("cleanup", |p| p.label.as_str()),
            run.phase
        );
        run.stop = true;
        run.evidence.valid = false;
        run.evidence.failure.get_or_insert(reason.clone());
        run.abort_reason.get_or_insert(reason);
        self.message = format!("A2 stopped: {}", run.abort_reason.as_deref().unwrap());
        run.pending = None;
        if run.modulation_active {
            run.phase = Phase::StopMod;
            self.send_mod(
                control,
                ModulationCommandV1::SetWaveform {
                    waveform: WaveformV1::Off,
                },
                true,
            );
        } else if run.pd_recording {
            run.phase = Phase::FinalizePd;
            self.send_pd(
                control,
                PhotodiodeCommandV1::FinalizeRecording {
                    termination: PdqTerminationV1::Aborted,
                },
                true,
            );
        } else if run.camera_recording {
            self.stop_camera(control);
        } else {
            self.release_next(control);
        }
    }

    fn drive(&mut self, control: &mut impl Control) {
        if self.start_pending {
            self.start_pending = false;
            self.begin(control);
        }
        if self.stop_pending {
            self.stop_pending = false;
            if let Some(r) = self.run.as_mut() {
                r.stop = true;
                r.abort_reason = Some("operator stopped A2".into());
            }
        }
        if self.continue_pending && !self.run.as_ref().is_some_and(|r| r.stop) {
            self.continue_pending = false;
            if self.run.as_ref().is_some_and(|r| r.phase == Phase::Paused) {
                self.run.as_mut().unwrap().pause_acknowledged = true;
                self.prepare(control);
            }
        }
        let Some(run) = self.run.as_ref() else { return };
        if !run.stop
            && !matches!(
                run.phase,
                Phase::ReleasePd | Phase::ReleaseMod | Phase::RestoreCamera
            )
        {
            let owner_changed = self.modulation.as_ref().is_none_or(|m| {
                m.owner_instance.as_str() != run.resolved.modulation_owner_instance
                    || !matches!(m.connection, ConnectionStateV1::Connected { .. })
            }) || self.photodiode.as_ref().is_none_or(|p| {
                p.owner_instance.as_str() != run.photodiode_setup.owner_instance
                    || !matches!(p.connection, ConnectionStateV1::Connected { .. })
            });
            if owner_changed {
                self.fail(
                    control,
                    "device owner disconnected or restarted during A2".into(),
                );
                return;
            }
        }
        if run.phase == Phase::Recording && !run.stop {
            let current = self
                .photodiode
                .as_ref()
                .and_then(|pd| pd.stream.sample_range)
                .map(|r| r.end_sample_index_exclusive);
            let now = now_ms();
            let run = self.run.as_mut().unwrap();
            let previous = run.pd_progress;
            if let Some(index) = current {
                if previous.is_none_or(|(old, _)| old != index) {
                    run.pd_progress = Some((index, now));
                }
            }
            if previous.is_some_and(|(_, at)| now.saturating_sub(at) > 5_000)
                && previous == run.pd_progress
            {
                self.fail(
                    control,
                    "photodiode samples stopped advancing for 5 s during recording".into(),
                );
                return;
            }
        }
        let run = self.run.as_ref().unwrap();
        if let Some((_, _, sent)) = run.pending {
            if now_ms().saturating_sub(sent) > TIMEOUT_MS {
                match run.phase {
                    Phase::ReleasePd => {
                        self.run
                            .as_mut()
                            .unwrap()
                            .cleanup_failures
                            .push("photodiode lease release timed out".into());
                        self.run.as_mut().unwrap().pd_leased = false;
                        self.run.as_mut().unwrap().pending = None;
                        self.release_next(control);
                    }
                    Phase::ReleaseMod => {
                        self.run
                            .as_mut()
                            .unwrap()
                            .cleanup_failures
                            .push("modulation safe-off/release timed out".into());
                        self.run.as_mut().unwrap().mod_leased = false;
                        self.run.as_mut().unwrap().pending = None;
                        self.release_next(control);
                    }
                    Phase::FinalizePd => {
                        let run = self.run.as_mut().unwrap();
                        run.pending = None;
                        run.pd_recording = false;
                        run.evidence
                            .failure
                            .get_or_insert("photodiode finalize timed out".into());
                        self.stop_camera(control);
                    }
                    Phase::StopCamera => {
                        self.run.as_mut().unwrap().pending = None;
                        if self.run.as_ref().unwrap().camera_stop_attempts < 3 {
                            self.stop_camera(control);
                        } else {
                            let run = self.run.as_mut().unwrap();
                            run.camera_recording = false;
                            run.evidence.valid = false;
                            run.stop = true;
                            let reason = "camera stop timed out after 3 attempts".to_string();
                            run.evidence.failure.get_or_insert(reason.clone());
                            run.abort_reason.get_or_insert(reason);
                            self.release_next(control);
                        }
                    }
                    Phase::StopMod => {
                        let run = self.run.as_mut().unwrap();
                        run.modulation_active = false;
                        run.cleanup_failures
                            .push("modulation stop timed out; release will retry safe-off".into());
                        self.fail(control, "modulation stop timed out".into());
                    }
                    _ => self.fail(
                        control,
                        format!("owner/host reply timed out after {TIMEOUT_MS} ms"),
                    ),
                }
            }
            return;
        }
        if run.stop
            && !matches!(
                run.phase,
                Phase::StopMod
                    | Phase::FinalizePd
                    | Phase::StopCamera
                    | Phase::ReleasePd
                    | Phase::ReleaseMod
                    | Phase::RestoreCamera
            )
        {
            let reason = run
                .abort_reason
                .clone()
                .or_else(|| run.evidence.failure.clone())
                .unwrap_or_else(|| "A2 stopped".into());
            self.fail(control, reason);
            return;
        }
        if run.mod_leased
            && run.pd_leased
            && !run.stop
            && !matches!(
                run.phase,
                Phase::StopMod
                    | Phase::FinalizePd
                    | Phase::StopCamera
                    | Phase::ReleasePd
                    | Phase::ReleaseMod
                    | Phase::RestoreCamera
            )
            && now_ms() >= run.next_renew_ms
        {
            self.send_mod(
                control,
                ModulationCommandV1::RenewLease {
                    ttl_ms: LEASE_TTL_MS,
                },
                false,
            );
            return;
        }
        match run.phase {
            Phase::Settle if now_ms() >= run.deadline_ms => {
                if let Err(error) = self.write_sidecar() {
                    self.fail(
                        control,
                        format!("cannot save A2 metadata before recording: {error}"),
                    );
                    return;
                }
                let meta = self.metadata();
                let (run_id, base) = {
                    let r = self.run.as_ref().unwrap();
                    (
                        r.run_id.clone(),
                        format!("{}/{}.raw", r.measurement_id, r.run_id),
                    )
                };
                self.run.as_mut().unwrap().phase = Phase::StartCamera;
                self.run.as_mut().unwrap().camera_recording = true;
                self.send_host(
                    control,
                    HostCommand::StartRecording {
                        run_id,
                        base_path: base,
                        root_dir: Some(
                            self.run
                                .as_ref()
                                .unwrap()
                                .output_root
                                .to_string_lossy()
                                .into_owned(),
                        ),
                        metadata: meta,
                    },
                );
            }
            Phase::Recording if now_ms() >= run.deadline_ms || run.stop => {
                if !starts_modulation(&self.point().unwrap().acquisition) {
                    self.run.as_mut().unwrap().phase = Phase::FinalizePd;
                    self.send_pd(
                        control,
                        PhotodiodeCommandV1::FinalizeRecording {
                            termination: PdqTerminationV1::Completed,
                        },
                        true,
                    );
                } else {
                    self.run.as_mut().unwrap().phase = Phase::StopMod;
                    self.send_mod(
                        control,
                        ModulationCommandV1::SetWaveform {
                            waveform: WaveformV1::Off,
                        },
                        true,
                    );
                }
            }
            Phase::PlateauLowLevel | Phase::PlateauHighLevel => self.poll_plateau(control),
            _ => {}
        }
    }

    fn accepted(&mut self, control: &mut impl Control, kind: PendingKind, payload: &Value) {
        let phase = self.run.as_ref().unwrap().phase;
        if kind != PendingKind::Host {
            let common = match kind {
                PendingKind::Mod | PendingKind::RenewMod => {
                    serde_json::from_value::<ModulationResponseV1>(payload.clone())
                        .map(|r| r.common)
                }
                _ => serde_json::from_value::<PhotodiodeResponseV1>(payload.clone())
                    .map(|r| r.common),
            };
            match common {
                Ok(common) => {
                    let run = self.run.as_ref().unwrap();
                    let owner = match kind {
                        PendingKind::Mod | PendingKind::RenewMod => {
                            &run.resolved.modulation_owner_instance
                        }
                        _ => &run.photodiode_setup.owner_instance,
                    };
                    if run
                        .pending
                        .is_none_or(|(_, id, _)| id != common.request_id.0)
                        || common.owner_instance.as_str() != owner
                        || common
                            .run_id
                            .as_ref()
                            .is_none_or(|id| id.as_str() != run.lease_run_id)
                    {
                        self.rejected(
                            control,
                            kind,
                            "stale or mismatched owner response identity".into(),
                        );
                        return;
                    }
                    if common.outcome == RequestOutcomeV1::Rejected {
                        let detail = common.error.map_or_else(
                            || "owner rejected without details".into(),
                            |e| owner_rejection_message(kind, &format!("{:?}", e.code), &e.message),
                        );
                        self.rejected(control, kind, detail);
                        return;
                    }
                    if common.outcome == RequestOutcomeV1::InProgress {
                        return;
                    }
                }
                Err(_) => {
                    if !cfg!(test) || !payload.is_null() {
                        self.rejected(control, kind, "owner returned a malformed response".into());
                        return;
                    }
                }
            }
        }
        if kind == PendingKind::Pd && phase == Phase::StartPd {
            let receipt = serde_json::from_value::<PhotodiodeResponseV1>(payload.clone())
                .ok()
                .and_then(|r| match r.receipt {
                    Some(PdqReceiptV1::Started(r)) => Some(r),
                    _ => None,
                });
            if let Some(receipt) = receipt {
                let pdq_path = self.resolve_owner_path(&receipt.pdq_path);
                let sidecar_path = self.resolve_owner_path(&receipt.sidecar_path);
                let error = self
                    .check_recording_directory(&pdq_path)
                    .err()
                    .or_else(|| self.check_recording_directory(&sidecar_path).err());
                let run = self.run.as_mut().unwrap();
                run.evidence.pdq_path = Some(pdq_path);
                run.evidence.pd_sidecar_path = Some(sidecar_path);
                if let Some(error) = error {
                    self.fail(control, format!("photodiode output path mismatch: {error}"));
                    return;
                }
                if let Err(error) = self.write_sidecar() {
                    self.fail(
                        control,
                        format!("cannot save opened photodiode paths: {error}"),
                    );
                    return;
                }
            } else if !cfg!(test) || !payload.is_null() {
                self.fail(
                    control,
                    "photodiode owner returned no opened-file receipt".into(),
                );
                return;
            }
        }
        if kind == PendingKind::RenewMod {
            self.run.as_mut().unwrap().pending = None;
            self.send_pd(
                control,
                PhotodiodeCommandV1::RenewLease {
                    ttl_ms: LEASE_TTL_MS,
                },
                false,
            );
            return;
        }
        if kind == PendingKind::RenewPd {
            self.run.as_mut().unwrap().pending = None;
            self.run.as_mut().unwrap().next_renew_ms = now_ms() + 20_000;
            return;
        }
        self.run.as_mut().unwrap().pending = None;
        if kind == PendingKind::Mod {
            if let Ok(response) = serde_json::from_value::<ModulationResponseV1>(payload.clone()) {
                let evidence = &mut self.run.as_mut().unwrap().evidence;
                if phase == Phase::Prepare || phase == Phase::StartStimulus {
                    evidence.marker_diagnostics_before = response.marker_diagnostics;
                }
                if phase == Phase::StopMod {
                    evidence.marker_diagnostics_after = response.marker_diagnostics;
                }
            }
        }
        if self.stop_pending || self.run.as_ref().unwrap().stop {
            self.stop_pending = false;
            let run = self.run.as_mut().unwrap();
            if kind == PendingKind::Mod && phase == Phase::AcquireMod {
                run.mod_leased = true;
            }
            if kind == PendingKind::Pd && phase == Phase::AcquirePd {
                run.pd_leased = true;
            }
            if !matches!(
                phase,
                Phase::StopMod
                    | Phase::FinalizePd
                    | Phase::StopCamera
                    | Phase::ReleasePd
                    | Phase::ReleaseMod
                    | Phase::RestoreCamera
            ) {
                self.fail(control, "operator stopped A2".into());
                return;
            }
        }
        match (kind, phase) {
            (PendingKind::Mod, Phase::AcquireMod) => {
                let run = self.run.as_mut().unwrap();
                run.mod_leased = true;
                run.phase = Phase::AcquirePd;
                self.send_pd(
                    control,
                    PhotodiodeCommandV1::AcquireLease {
                        ttl_ms: LEASE_TTL_MS,
                    },
                    false,
                );
            }
            (PendingKind::Pd, Phase::AcquirePd) => {
                self.run.as_mut().unwrap().pd_leased = true;
                self.prepare(control)
            }
            (PendingKind::Mod, Phase::PlateauLowDrive) => {
                self.plateau_drive_acknowledged(control, true)
            }
            (PendingKind::Mod, Phase::PlateauHighDrive) => {
                self.plateau_drive_acknowledged(control, false)
            }
            (PendingKind::Mod, Phase::Prepare)
                if self.run.as_ref().unwrap().protocol.timing_reference
                    == A2TimingReferenceV1::DriveSync
                    && starts_modulation(&self.point().unwrap().acquisition) =>
            {
                self.run.as_mut().unwrap().phase = Phase::QuietBeforeCapture;
                self.send_mod(
                    control,
                    ModulationCommandV1::SetWaveform {
                        waveform: WaveformV1::Off,
                    },
                    true,
                );
            }
            (PendingKind::Mod, Phase::Prepare | Phase::QuietBeforeCapture) => {
                let settle = (self.point().unwrap().settle_s * 1000.0) as u64;
                let r = self.run.as_mut().unwrap();
                if phase == Phase::QuietBeforeCapture {
                    r.modulation_active = false;
                }
                r.phase = Phase::Settle;
                r.deadline_ms = now_ms() + settle.max(250);
            }
            (PendingKind::Host, Phase::StartCamera) => {
                let r = self.run.as_ref().unwrap();
                let spec = PdqStartSpecV1 {
                    pdq_path: format!("{}/{}.pdq", r.measurement_id, r.run_id),
                    sidecar_path: format!("{}/{}.pd.json", r.measurement_id, r.run_id),
                    expected_sample_rate_hz: Some(r.controller.sample_rate_hz),
                    expected_stream_epoch: self.photodiode.as_ref().map(|p| p.stream.stream_epoch),
                    metadata: self.metadata(),
                    root_dir: Some(r.output_root.to_string_lossy().into_owned()),
                };
                let run = self.run.as_mut().unwrap();
                run.phase = Phase::StartPd;
                run.pd_recording = true;
                self.send_pd(
                    control,
                    PhotodiodeCommandV1::BeginRecording {
                        specification: spec,
                    },
                    true,
                );
            }
            (PendingKind::Pd, Phase::StartPd)
                if self.run.as_ref().unwrap().protocol.timing_reference
                    == A2TimingReferenceV1::DriveSync
                    && starts_modulation(&self.point().unwrap().acquisition) =>
            {
                // Both files are open before the first phase-zero pulse. The quiet
                // lead-in makes the first shared cycle identifiable independently
                // of USB/host recorder start latency.
                self.send_prepare_a2(control, 0);
                self.run.as_mut().unwrap().phase = Phase::StartStimulus;
            }
            (PendingKind::Pd, Phase::StartPd) | (PendingKind::Mod, Phase::StartStimulus) => {
                let seconds = self.point().unwrap().acquisition_seconds();
                let r = self.run.as_mut().unwrap();
                r.phase = Phase::Recording;
                r.pd_progress = Some((
                    self.photodiode
                        .as_ref()
                        .and_then(|pd| pd.stream.sample_range)
                        .map_or(0, |range| range.end_sample_index_exclusive),
                    now_ms(),
                ));
                r.deadline_ms = now_ms() + (seconds * 1000.0).ceil() as u64;
                self.message = format!(
                    "Recording point {}: {} ({seconds:.3} s)",
                    r.index + 1,
                    r.protocol.points[r.index].label
                );
                if let Err(error) = self.append_progress("point_started") {
                    self.fail(control, format!("cannot save point progress: {error}"));
                }
            }
            (PendingKind::Mod, Phase::StopMod) => {
                let run = self.run.as_mut().unwrap();
                run.modulation_active = false;
                if !run.pd_recording {
                    if run.camera_recording {
                        self.stop_camera(control);
                    } else {
                        self.release_next(control);
                    }
                    return;
                }
                run.phase = Phase::FinalizePd;
                let termination = if run.abort_reason.is_some() {
                    PdqTerminationV1::Aborted
                } else {
                    PdqTerminationV1::Completed
                };
                self.send_pd(
                    control,
                    PhotodiodeCommandV1::FinalizeRecording { termination },
                    true,
                );
            }
            (PendingKind::Pd, Phase::FinalizePd) => {
                let finalized = serde_json::from_value::<PhotodiodeResponseV1>(payload.clone())
                    .ok()
                    .and_then(|response| match response.receipt {
                        Some(PdqReceiptV1::Finalized(receipt)) => Some(receipt),
                        _ => None,
                    });
                let requested_seconds = self.point().unwrap().acquisition_seconds();
                let paths = finalized.as_ref().map(|receipt| {
                    (
                        self.resolve_owner_path(&receipt.pdq_path),
                        self.resolve_owner_path(&receipt.sidecar_path),
                    )
                });
                let e = &mut self.run.as_mut().unwrap().evidence;
                if let Some(receipt) = finalized {
                    let seconds = receipt
                        .sample_range
                        .zip(receipt.sample_rate_hz)
                        .filter(|(_, rate)| *rate > 0)
                        .map(|(range, rate)| range.sample_count as f64 / f64::from(rate));
                    if seconds.is_none_or(|s| s + 0.05 < requested_seconds) {
                        e.failure.get_or_insert(format!("PDQ does not cover the requested {:.3} s: sampled duration={seconds:?} s", requested_seconds));
                    }
                    e.pdq_marker_counts = receipt.marker_counts;
                    if let Some((pdq_path, sidecar_path)) = paths {
                        e.pdq_path = Some(pdq_path);
                        e.pd_sidecar_path = Some(sidecar_path);
                    }
                    e.pdq_sha256 = Some(receipt.sha256.to_string());
                    if !receipt.valid
                        || !receipt.integrity.is_clean()
                        || receipt.sample_frames_written == 0
                        || receipt.segment_count != 1
                        || receipt.termination != PdqTerminationV1::Completed
                    {
                        e.failure.get_or_insert(format!("PDQ receipt invalid: termination={:?}, sample frames={}, segments={}, integrity={:?}",
                            receipt.termination, receipt.sample_frames_written, receipt.segment_count, receipt.integrity));
                    }
                } else {
                    e.failure = Some("photodiode owner returned no finalized PDQ receipt".into());
                }
                self.run.as_mut().unwrap().pd_recording = false;
                self.stop_camera(control);
            }
            (PendingKind::Pd, Phase::ReleasePd) => {
                self.run.as_mut().unwrap().pd_leased = false;
                self.release_next(control);
            }
            (PendingKind::Mod, Phase::ReleaseMod) => {
                self.run.as_mut().unwrap().mod_leased = false;
                self.release_next(control);
            }
            _ => {}
        }
    }

    fn snapshots(&mut self, inbox: &PluginControlInbox) {
        for s in &inbox.snapshots {
            match (s.plugin_id.as_str(), s.topic.as_str()) {
                (MOD_ID, CTX_STAGE_A_MODULATION_STATE_V1) => {
                    if let Ok(v) = serde_json::from_value(s.payload.clone()) {
                        self.modulation = Some(v)
                    }
                }
                (PD_ID, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1) => {
                    if let Ok(v) = serde_json::from_value(s.payload.clone()) {
                        self.photodiode = Some(v)
                    }
                }
                _ => {}
            }
        }
    }

    fn rejected(&mut self, control: &mut impl Control, kind: PendingKind, reason: String) {
        let Some(run) = self.run.as_mut() else { return };
        run.pending = None;
        match run.phase {
            Phase::ReleasePd | Phase::ReleaseMod => {
                run.cleanup_failures.push(reason);
                if run.phase == Phase::ReleasePd {
                    run.pd_leased = false;
                } else {
                    run.mod_leased = false;
                }
                self.release_next(control);
            }
            Phase::FinalizePd => {
                run.pd_recording = false;
                run.evidence.failure.get_or_insert(reason.clone());
                run.abort_reason.get_or_insert(reason);
                run.stop = true;
                self.stop_camera(control);
            }
            Phase::StopMod => {
                run.modulation_active = false;
                run.cleanup_failures.push(format!(
                    "modulation stop rejected: {reason}; release will retry safe-off"
                ));
                self.fail(control, reason);
            }
            _ => {
                let _ = kind;
                self.fail(control, reason);
            }
        }
    }

    fn finish_async_mod(&mut self, control: &mut impl Control) {
        let Some((kind @ (PendingKind::Mod | PendingKind::RenewMod), request_id, sent)) =
            self.run.as_ref().and_then(|r| r.pending)
        else {
            return;
        };
        if let Some(response) = self
            .modulation
            .as_ref()
            .and_then(|s| s.last_response.as_ref())
        {
            let run = self.run.as_ref().unwrap();
            if response.common.request_id.0 == request_id
                && response.common.owner_instance.as_str() == run.resolved.modulation_owner_instance
                && response
                    .common
                    .run_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == run.lease_run_id)
                && response.common.outcome != RequestOutcomeV1::InProgress
            {
                let payload = serde_json::to_value(response).unwrap_or(Value::Null);
                self.accepted(control, kind, &payload);
                return;
            }
        }
        let now = now_ms();
        let run = self.run.as_mut().unwrap();
        if now.saturating_sub(sent) <= TIMEOUT_MS && now.saturating_sub(run.last_mod_poll_ms) >= 200
        {
            if let Some(request) = run
                .pending_mod_request
                .as_ref()
                .filter(|r| r.request_id == request_id)
            {
                control.service(request);
                run.last_mod_poll_ms = now;
            }
        }
    }

    fn finish_point(&mut self, control: &mut impl Control, outcome: &HostCommandOutcome) {
        if let HostCommandOutcome::RecordingFinalized {
            actual_raw_path,
            size,
            sha256,
            duration_us,
        } = outcome
        {
            let acquisition = {
                let run = self.run.as_ref().unwrap();
                run.protocol.points[run.index].acquisition.clone()
            };
            let reference = self.run.as_ref().unwrap().protocol.timing_reference;
            let e = &mut self.run.as_mut().unwrap().evidence;
            e.raw_path = Some(actual_raw_path.clone());
            e.raw_sha256 = Some(sha256.clone());
            if *size == 0 || *duration_us == 0 {
                e.failure
                    .get_or_insert("camera receipt contains no recorded data".into());
            }
            let trigger_problem =
                validate_trigger_counts(&acquisition, e.rising_triggers, e.falling_triggers).err();
            if let Some(problem) = trigger_problem {
                e.warnings.push(problem);
            }
            if starts_modulation(&acquisition) {
                match (e.marker_diagnostics_before, e.marker_diagnostics_after) {
                    (Some(before), Some(after)) if before.dma_sample_clock && after.dma_sample_clock
                        && before.marker_drops == after.marker_drops
                        && before.stream_marker_drops == after.stream_marker_drops => {},
                    counters => e.warnings.push(format!("Firmware marker-loss evidence needs review: {counters:?}. \
                        Missing DMA-clock confirmation/status counters, resets or increasing drops prevent an assumed one-to-one clock match")),
                }
                match e.pdq_marker_counts {
                    Some(c) if c.invalid_level == 0 && match reference {
                        A2TimingReferenceV1::Comparator => c.comparator_rising > 0 && c.comparator_falling > 0,
                        A2TimingReferenceV1::DriveSync => c.phase_zero > 0,
                    } => {},
                    counts => e.warnings.push(format!("PDQ shared time reference is missing or incomplete: {counts:?}. \
                        Do not align by edge ordinal alone; inspect marker source, level and device ticks against camera RAW triggers")),
                }
            }
            e.valid = e.failure.is_none() && e.warnings.is_empty();
            self.run.as_mut().unwrap().camera_recording = false;
        } else if let HostCommandOutcome::RecordingPartial {
            actual_raw_path,
            sha256,
            reason,
            ..
        } = outcome
        {
            let run = self.run.as_mut().unwrap();
            run.camera_recording = false;
            run.evidence.raw_path = Some(actual_raw_path.clone());
            run.evidence.raw_sha256 = sha256.clone();
            run.evidence
                .failure
                .get_or_insert(format!("RAW finalized partially: {reason}"));
        } else if self.run.as_ref().unwrap().camera_stop_attempts < 3 {
            self.stop_camera(control);
            return;
        } else {
            let run = self.run.as_mut().unwrap();
            run.camera_recording = false;
            run.evidence.failure = Some(format!(
                "camera stop was not confirmed after 3 attempts: {outcome:?}"
            ));
        }
        {
            let r = self.run.as_mut().unwrap();
            if r.evidence.failure.is_none() && !r.evidence.warnings.is_empty() {
                if r.protocol.trigger_validation == TriggerValidation::Strict {
                    r.evidence.failure = Some(r.evidence.warnings.join("; "));
                } else {
                    r.review_points += 1;
                }
            }
            if let Some(reason) = r.evidence.failure.clone() {
                r.evidence.valid = false;
                r.stop = true;
                r.abort_reason.get_or_insert(format!(
                    "point {} '{}': {reason}",
                    r.index + 1,
                    r.protocol.points[r.index].label
                ));
            }
        }
        {
            let run = self.run.as_mut().unwrap();
            run.evidence.acquisition_complete = run.evidence.failure.is_none() && !run.stop;
        }
        if let Err(error) = self
            .write_sidecar()
            .and_then(|()| self.append_progress("point_finished"))
        {
            let run = self.run.as_mut().unwrap();
            run.stop = true;
            run.evidence.valid = false;
            run.abort_reason
                .get_or_insert(format!("cannot save A2 sidecar: {error}"));
        }
        if self.run.as_ref().is_some_and(|run| run.stop) {
            self.release_next(control);
        } else {
            self.advance(control);
        }
    }

    fn host_reply(
        &mut self,
        control: &mut impl Control,
        request_id: u64,
        outcome: HostCommandOutcome,
    ) {
        let expected = self.run.as_ref().and_then(|run| run.pending);
        if expected.is_none_or(|(kind, id, _)| kind != PendingKind::Host || id != request_id) {
            return;
        }
        let phase = self.run.as_ref().unwrap().phase;
        self.run.as_mut().unwrap().pending = None;
        if phase == Phase::ApplyCamera {
            match outcome {
                HostCommandOutcome::CameraConfigurationApplied {
                    mut snapshot,
                    provenance,
                    readback: _,
                    readback_age_s,
                } => {
                    let override_camera = self.camera_override.clone();
                    let needs_override = self
                        .run
                        .as_ref()
                        .is_some_and(|run| !run.camera_override_applied)
                        && override_camera.as_ref().is_some_and(|camera| {
                            camera.diff_on.is_some()
                                || camera.diff_off.is_some()
                                || camera.fo.is_some()
                                || camera.hpf.is_some()
                                || camera.refr.is_some()
                                || camera.roi.is_some()
                                || camera.filters_off == Some(true)
                        });
                    if needs_override {
                        if let Some(camera) = override_camera {
                            if let Some(mode) = &camera.roi {
                                if let Err(reason) = apply_roi_mode(&mut snapshot, mode) {
                                    self.fail(control, reason);
                                    return;
                                }
                            }
                            if let Some(value) = camera.diff_on {
                                snapshot.biases.diff_on = value;
                            }
                            if let Some(value) = camera.diff_off {
                                snapshot.biases.diff_off = value;
                            }
                            if let Some(value) = camera.fo {
                                snapshot.biases.fo = value;
                            }
                            if let Some(value) = camera.hpf {
                                snapshot.biases.hpf = value;
                            }
                            if let Some(value) = camera.refr {
                                snapshot.biases.refr = value;
                            }
                            if camera.filters_off == Some(true) {
                                snapshot.digital_filter.stc_enabled = false;
                                snapshot.digital_filter.trail_enabled = false;
                                snapshot.digital_filter.erc_enabled = Some(false);
                            }
                        }
                        self.run.as_mut().unwrap().camera_override_applied = true;
                        self.send_host(
                            control,
                            HostCommand::ApplyCameraConfiguration {
                                configuration: CameraConfigurationSourceV1::Snapshot { snapshot },
                            },
                        );
                        return;
                    }
                    let refusal = camera_configuration_refusal(&snapshot, readback_age_s);
                    let run = self.run.as_mut().unwrap();
                    run.camera_snapshot = Some(snapshot);
                    run.camera_provenance = Some(provenance);
                    run.camera_readback_age_s = Some(readback_age_s);
                    if let Some(reason) = refusal {
                        self.fail(control, reason);
                    } else {
                        run.phase = Phase::AcquireMod;
                        self.send_mod(
                            control,
                            ModulationCommandV1::AcquireLease {
                                ttl_ms: LEASE_TTL_MS,
                            },
                            false,
                        );
                    }
                }
                outcome => self.fail(
                    control,
                    format!("camera configuration was not applied and confirmed: {outcome:?}"),
                ),
            }
        } else if phase == Phase::RestoreCamera {
            if matches!(
                outcome,
                HostCommandOutcome::CameraConfigurationRestored { .. }
            ) {
                self.run.as_mut().unwrap().camera_session_active = false;
                self.finish_run();
            } else if self.run.as_ref().unwrap().restore_attempts < 3 {
                self.run.as_mut().unwrap().restore_attempts += 1;
                self.send_host(control, HostCommand::RestoreCameraConfiguration);
            } else {
                self.run.as_mut().unwrap().cleanup_failures.push(format!(
                    "camera restore was not confirmed after 3 attempts: {outcome:?}"
                ));
                self.run.as_mut().unwrap().camera_session_active = false;
                self.finish_run();
            }
        } else if phase == Phase::StartCamera {
            match outcome {
                HostCommandOutcome::RecordingStarted {
                    actual_raw_path, ..
                } => {
                    let evidence = &mut self.run.as_mut().unwrap().evidence;
                    evidence.camera_configuration_sidecar_path =
                        Some(camera_sidecar_path(&actual_raw_path));
                    evidence.sensor_monitoring_path =
                        Some(sensor_monitoring_path(&actual_raw_path));
                    evidence.raw_path = Some(actual_raw_path.clone());
                    if let Err(error) = self.check_recording_directory(&actual_raw_path) {
                        self.fail(control, error);
                        return;
                    }
                    if let Err(error) = self.write_sidecar() {
                        self.fail(control, format!("cannot save opened camera path: {error}"));
                        return;
                    }
                    self.accepted(control, PendingKind::Host, &Value::Null)
                }
                outcome => {
                    self.run.as_mut().unwrap().camera_recording = false;
                    self.fail(control, format!("camera start failed: {outcome:?}"));
                }
            }
        } else if phase == Phase::StopCamera {
            self.finish_point(control, &outcome)
        }
    }

    /// Anchor an owner-reported recording path to the run's output root.
    ///
    /// The photodiode owner echoes a workflow path exactly as the request
    /// named it: relative to the `root_dir` it was given. Resolving it here
    /// keeps every later check and every recorded artefact path absolute.
    /// The components are pushed one by one because the request separates
    /// them with `/`, which a Windows verbatim root (`\\?\C:\...`, what
    /// `canonicalize` returns) does not read as a separator.
    fn resolve_owner_path(&self, reported: &str) -> String {
        let path = Path::new(reported);
        if path.is_absolute() {
            return reported.to_owned();
        }
        let mut resolved = self.run.as_ref().unwrap().output_root.clone();
        resolved.extend(path.components());
        resolved.to_string_lossy().into_owned()
    }

    fn check_recording_directory(&self, raw: &str) -> Result<(), String> {
        let run = self.run.as_ref().unwrap();
        let expected = run.output_root.join(&run.measurement_id);
        let parent = Path::new(raw)
            .parent()
            .ok_or("recorder returned no parent directory")?;
        let actual = parent
            .canonicalize()
            .map_err(|e| format!("cannot verify recording output directory: {e}"))?;
        if actual != expected.canonicalize().map_err(|e| e.to_string())? {
            return Err(format!(
                "recorder saved outside the measurement folder: {raw}; expected {}. Install the matching host with workflow recording-root support",
                expected.display()
            ));
        }
        Ok(())
    }

    fn append_progress(&self, event: &str) -> Result<(), String> {
        use std::io::Write;
        let Some(run) = self.run.as_ref() else {
            return Ok(());
        };
        let path = run
            .output_root
            .join(&run.measurement_id)
            .join(format!("{}_progress.jsonl", run.attempt_id));
        let entry = json!({"schema":"stage-a.a2.progress.v1", "event":event,
            "at_unix_ms":now_ms(), "measurement_id":run.measurement_id, "run_id":run.run_id,
            "protocol_sha256":run.protocol_sha256, "point_index":run.index+1,
            "point_total":run.protocol.points.len(), "completed_points":run.completed_points,
            "resumed_rows":run.resumed_points.iter().map(|i| i + 1).collect::<Vec<_>>(),
            "phase":format!("{:?}",run.phase), "evidence":run.evidence,
            "abort_reason":run.abort_reason, "cleanup_failures":run.cleanup_failures});
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{entry}").map_err(|e| e.to_string())?;
        file.sync_data().map_err(|e| e.to_string())
    }

    fn write_sidecar(&self) -> Result<(), String> {
        let r = self.run.as_ref().unwrap();
        let dir = r.output_root.join(&r.measurement_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        #[derive(Serialize)]
        struct Side<'a> {
            schema_version: u32,
            measurement_id: &'a str,
            attempt_id: &'a str,
            output_root: &'a Path,
            trigger_validation: TriggerValidation,
            cleanup_failures: &'a [String],
            experiment: &'static str,
            scientific_status: &'static str,
            timing_reference: A2TimingReferenceV1,
            protocol_path: &'a str,
            protocol_sha256: &'a str,
            protocol_archive_path: &'a str,
            protocol_name: &'a str,
            protocol_row: usize,
            camera_source: &'static str,
            camera_provenance: Option<&'a CameraConfigurationProvenanceV1>,
            camera_readback_age_s: Option<f64>,
            photodiode_setup: &'a ResolvedPhotodiode,
            controller: &'a ControllerSetup,
            /// Kind-1 values the protocol no longer states, with the owner
            /// provenance that replaces having typed them.
            resolved_controller: &'a ResolvedController,
            point: &'a Point,
            evidence: &'a PointEvidence,
            sensor_snapshot: Option<DynamicSensorMonitoring>,
        }
        #[derive(Serialize)]
        struct DynamicSensorMonitoring {
            pixel_dead_time_us: Option<f32>,
            illumination_lux: Option<f32>,
            temperature_c: Option<f32>,
            age_s: f64,
        }
        let s = Side {
            schema_version: 2,
            measurement_id: &r.measurement_id,
            attempt_id: &r.attempt_id,
            output_root: &r.output_root,
            trigger_validation: r.protocol.trigger_validation,
            cleanup_failures: &r.cleanup_failures,
            experiment: "A2",
            scientific_status: "requires_offline_h4_h5_review",
            timing_reference: r.protocol.timing_reference,
            protocol_path: &r.protocol_path,
            protocol_sha256: &r.protocol_sha256,
            protocol_archive_path: &r.protocol_archive_path,
            protocol_name: &r.protocol.name,
            protocol_row: r.index + 1,
            camera_source: "current_host_configuration",
            camera_provenance: r.camera_provenance.as_ref(),
            camera_readback_age_s: r.camera_readback_age_s,
            photodiode_setup: &r.photodiode_setup,
            controller: &r.controller,
            resolved_controller: &r.resolved,
            point: &r.protocol.points[r.index],
            evidence: &r.evidence,
            sensor_snapshot: self.sensor.map(|sensor| DynamicSensorMonitoring {
                pixel_dead_time_us: sensor.pixel_dead_time_us,
                illumination_lux: sensor.illumination_lux,
                temperature_c: sensor.temperature_c,
                age_s: sensor.age_s,
            }),
        };
        let bytes = serde_json::to_vec_pretty(&s).map_err(|e| e.to_string())?;
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(&dir).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.as_file().sync_all().map_err(|e| e.to_string())?;
        file.persist(dir.join(format!("{}.a2.json", r.run_id)))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl Plugin for StageAA2Plugin {
    fn name(&self) -> &'static str {
        "Stage-A A2 Latency"
    }
    fn description(&self) -> &'static str {
        "Runs qualified optical-step latency protocols; fitting stays offline."
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, v: bool) {
        self.enabled = v
    }
    fn set_runtime_role(&mut self, r: PluginRuntimeRole) {
        self.role = r
    }
    fn reset(&mut self) {
        // Host recording boundaries reset preview state, not the acquisition.
        if let Some(run) = self.run.as_mut() {
            run.last_event_bin_us = None;
            run.last_event_bin_count = 0;
        }
    }

    fn handle_service_request(
        &mut self,
        request: &PluginServiceRequest,
        execution: &ExecutionContext,
    ) -> PluginServiceReply {
        let outcome = if request.service == stage_a_universal_runner::SERVICE_READY_V1 {
            if execution.hardware_effects_allowed() {
                PluginServiceOutcome::Accepted {
                    payload: json!({"ready": !(self.run.is_some() || self.start_pending)}),
                }
            } else {
                PluginServiceOutcome::Rejected {
                    code: "effects_not_allowed".into(),
                    message: "Owner is not a live worker".into(),
                }
            }
        } else if request.service == stage_a_universal_runner::SERVICE_STOP_V1 {
            let matches = self.universal_request.as_ref().is_some_and(|r| {
                request
                    .payload
                    .get("measurement_id")
                    .and_then(Value::as_str)
                    == Some(r.measurement_id.as_str())
            });
            if execution.hardware_effects_allowed() && matches {
                self.stop_pending = true;
                PluginServiceOutcome::Accepted {
                    payload: json!({"stopping": true}),
                }
            } else {
                PluginServiceOutcome::Rejected {
                    code: "wrong_measurement".into(),
                    message: "Stop must name the active live universal measurement".into(),
                }
            }
        } else if self.run.is_some() || self.start_pending {
            PluginServiceOutcome::Rejected {
                code: "owner_busy".into(),
                message: "Finish the current measurement before another handoff".into(),
            }
        } else if request.service != stage_a_universal_runner::SERVICE_EXECUTE_BLOCK_V1 {
            PluginServiceOutcome::Rejected {
                code: "unsupported_service".into(),
                message: "A2 does not support this service".into(),
            }
        } else if !execution.hardware_effects_allowed() {
            PluginServiceOutcome::Rejected {
                code: "effects_not_allowed".into(),
                message: "A2 block execution requires the live worker".into(),
            }
        } else {
            match serde_json::from_value::<stage_a_universal_runner::ExecuteBlockRequest>(
                request.payload.clone(),
            ) {
                Ok(command) if command.experiment == stage_a_universal_runner::Experiment::A2 => {
                    if command.output_folder.trim().is_empty() {
                        return PluginServiceReply {
                            request_id: request.request_id,
                            source_plugin_id: request.source_plugin_id.clone(),
                            target_plugin_id: request.target_plugin_id.clone(),
                            service: request.service.clone(),
                            outcome: PluginServiceOutcome::Rejected {
                                code: "missing_output_folder".into(),
                                message: "Universal Runner did not provide a common output folder"
                                    .into(),
                            },
                        };
                    }
                    self.universal_request = Some(command.clone());
                    self.universal_terminal = None;
                    self.camera_override = Some(command.camera.clone());
                    self.output_folder_override = Some(command.output_folder.clone());
                    if !command.protocol.trim().is_empty() {
                        match materialize_universal_protocol(command.protocol.trim()) {
                            Ok(Some(path)) => self.protocol_path = path,
                            Ok(None) => self.protocol_path = command.protocol,
                            Err(error) => {
                                return PluginServiceReply {
                                    request_id: request.request_id,
                                    source_plugin_id: request.source_plugin_id.clone(),
                                    target_plugin_id: request.target_plugin_id.clone(),
                                    service: request.service.clone(),
                                    outcome: PluginServiceOutcome::Rejected {
                                        code: "protocol_materialization_failed".into(),
                                        message: error,
                                    },
                                };
                            }
                        }
                    }
                    self.measurement_id = command.measurement_id.clone();
                    self.start_pending = true;
                    PluginServiceOutcome::Accepted {
                        payload: json!({"measurement_id": command.measurement_id}),
                    }
                }
                Ok(command) => PluginServiceOutcome::Rejected {
                    code: "wrong_target".into(),
                    message: format!("A2 cannot execute {:?}", command.experiment),
                },
                Err(error) => PluginServiceOutcome::Rejected {
                    code: "invalid_payload".into(),
                    message: error.to_string(),
                },
            }
        };
        PluginServiceReply {
            request_id: request.request_id,
            source_plugin_id: request.source_plugin_id.clone(),
            target_plugin_id: request.target_plugin_id.clone(),
            service: request.service.clone(),
            outcome,
        }
    }
    fn on_discontinuity(&mut self, _: PluginDiscontinuity) {}
    fn input_kind(&self) -> PluginInput {
        PluginInput::RawEvents
    }
    fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities::default()
    }
    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        _: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _: &EventStoreHandle<'_>,
    ) {
        if let Ok(Some(v)) = context.get::<GlobalSettings>(CTX_GLOBAL_SETTINGS) {
            self.settings = Some(v);
        }
        if let Ok(Some(v)) = context.get::<SensorMonitoringV1>(CTX_SENSOR_MONITORING) {
            self.sensor = Some(v);
        }
        if self
            .run
            .as_ref()
            .is_some_and(|r| r.phase == Phase::Recording)
        {
            let r = self.run.as_mut().unwrap();
            for e in frame.events() {
                let bin = e.t_us.max(0) as u64;
                if r.last_event_bin_us == Some(bin) {
                    r.last_event_bin_count += 1;
                } else {
                    r.last_event_bin_us = Some(bin);
                    r.last_event_bin_count = 1;
                }
                r.evidence.peak_events_per_us =
                    r.evidence.peak_events_per_us.max(r.last_event_bin_count);
            }
            for t in frame.external_triggers() {
                if r.camera_snapshot
                    .as_ref()
                    .is_some_and(|s| i32::from(s.external_trigger.channel) != i32::from(t.id))
                {
                    continue;
                }
                if t.is_rising() {
                    r.evidence.rising_triggers += 1
                } else {
                    r.evidence.falling_triggers += 1
                }
            }
            if r.evidence.peak_events_per_us > RECORDER_SAFETY_LIMIT_EVENTS_PER_US
                && !r
                    .evidence
                    .warnings
                    .iter()
                    .any(|v| v.starts_with("Event-load review"))
            {
                r.evidence.warnings.push(format!("Event-load review: preview peak {} events/us exceeds the conservative {} events/us advisory; H21 must be evaluated offline",
                    r.evidence.peak_events_per_us, RECORDER_SAFETY_LIMIT_EVENTS_PER_US));
            }
        }
    }
    fn process_control(&mut self, c: &mut PluginControlContext<'_>) {
        let inbox = c.inbox().clone();
        self.snapshots(&inbox);
        if self
            .universal_request
            .as_ref()
            .and_then(|r| r.acquisition_deadline_unix_ms)
            .is_some_and(|deadline| now_ms() >= deadline)
            && (self.run.as_ref().is_some_and(|r| !r.stop) || self.start_pending)
        {
            self.stop_pending = true;
        }
        self.finish_async_mod(c);
        for reply in inbox.service_replies {
            let expected = self.run.as_ref().and_then(|r| r.pending);
            if expected.is_none_or(|(_, id, _)| id != reply.request_id) {
                continue;
            }
            match reply.outcome {
                PluginServiceOutcome::Accepted { payload } => {
                    self.accepted(c, expected.unwrap().0, &payload)
                }
                PluginServiceOutcome::Rejected { code, message } => {
                    self.rejected(
                        c,
                        expected.unwrap().0,
                        owner_rejection_message(expected.unwrap().0, &code, &message),
                    );
                }
            }
        }
        for reply in inbox.host_replies {
            self.host_reply(c, reply.request_id, reply.outcome);
        }
        self.drive(c);
    }

    fn control_snapshots(&self) -> Vec<PluginControlSnapshot> {
        let state = if self.run.is_some() {
            "running"
        } else if self.start_pending {
            "pending"
        } else {
            self.universal_terminal
                .unwrap_or(if self.universal_request.is_some() {
                    "failed"
                } else {
                    "idle"
                })
        };
        vec![PluginControlSnapshot {
            plugin_id: "stage-a.a2".into(),
            topic: "stage-a.universal.block".into(),
            revision: self.revision,
            payload: json!({"state": state, "measurement_id": self.universal_request.as_ref().map(|r| &r.measurement_id),
                "attempt": self.universal_request.as_ref().map(|r|r.attempt), "message": self.message}),
        }]
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "A2 protocol".into(),
                    description: Some(
                        "The file is validated completely before any owner lease or camera recording starts."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem { key: "measurement_id".into(), label: "Measurement id".into(), tooltip: Some("Names the folder and every file. Leave blank to generate an id; repeated runs keep distinct filenames.".into()), kind: SettingKind::Text { default: self.measurement_id.clone() } },
                        SettingItem { key: "new_id".into(), label: "New id".into(), tooltip: None, kind: SettingKind::Button { enabled: self.run.is_none() } },
                        SettingItem { key: "protocol_path".into(), label: "Protocol".into(), tooltip: None, kind: SettingKind::Path { dialog: PathDialogKind::OpenFile, default: self.protocol_path.clone() } },
                        SettingItem { key: "run_protocol".into(), label: "Run protocol".into(), tooltip: None, kind: SettingKind::Button { enabled: self.run.is_none() } },
                        // The UI mirror has no Run. The live worker validates these actions.
                        SettingItem { key: "continue_run".into(), label: "Continue".into(), tooltip: Some("Continue the current manual pause. Ignored when no pause is waiting.".into()), kind: SettingKind::Button { enabled: true } },
                        SettingItem { key: "stop_protocol".into(), label: "Stop".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
                    ],
                },
                SettingsSection {
                    label: "Before the first A2 run".into(),
                    description: Some(
                        "Choose the data folder and references in the photodiode plugin. Apply the optical calibration in modulation. A2 sets each protocol point automatically and saves all files in the measurement folder.\n\nDrive-sync protocols use J24 common markers and determine optical ON/OFF timing offline. Comparator protocols retain their separate threshold and polarity checks."
                            .into(),
                    ),
                    default_open: false,
                    items: Vec::new(),
                },
            ],
        }
    }
    fn get_setting(&self, k: &str) -> Option<Value> {
        match k {
            "output_folder" => Some(json!(self.output_folder)),
            "measurement_id" => Some(json!(self.measurement_id)),
            "protocol_path" => Some(json!(self.protocol_path)),
            "new_id" => Some(json!(self.new_id.value)),
            "run_protocol" => Some(json!(self.start.value)),
            "continue_run" => Some(json!(self.continue_press.value)),
            "stop_protocol" => Some(json!(self.stop.value)),
            _ => None,
        }
    }
    fn set_setting(&mut self, k: &str, v: Value) -> Result<(), String> {
        if self.run.is_some()
            && matches!(
                k,
                "output_folder" | "measurement_id" | "protocol_path" | "new_id"
            )
        {
            return Err(
                "Stop the current protocol before changing its measurement settings".into(),
            );
        }
        match k {
            "output_folder" => self.output_folder = v.as_str().ok_or("string required")?.into(),
            "measurement_id" => self.measurement_id = v.as_str().ok_or("string required")?.into(),
            "protocol_path" => {
                self.protocol_path = v.as_str().ok_or("string required")?.into();
                self.protocol_preview = std::fs::read_to_string(self.protocol_path.trim())
                    .ok()
                    .and_then(|s| protocol::parse(&s).ok());
            }
            "new_id" => {
                if self.new_id.accept(&v) {
                    self.measurement_id = format!("A2-{}", compact_time());
                }
            }
            "run_protocol" => {
                if self.start.accept(&v) {
                    self.start_pending = true
                }
            }
            "continue_run" => {
                if self.continue_press.accept(&v) {
                    self.continue_pending = true
                }
            }
            "stop_protocol" => {
                if self.stop.accept(&v) {
                    self.stop_pending = true
                }
            }
            _ => return Err(format!("unknown setting {k}")),
        }
        Ok(())
    }
    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut v = vec![StatusEntry::Text(self.message.clone())];
        if let Some(r) = &self.run {
            v.push(StatusEntry::Text(format!(
                "{}: point {}/{} ({:?})",
                r.protocol.name,
                r.index + 1,
                r.protocol.points.len(),
                r.phase
            )));
            v.push(StatusEntry::Text(format!(
                "{} reused, {} newly recorded; about {} remaining, plus file finalization and manual pauses",
                r.resumed_points.len(), r.completed_points,
                format_bench_time(remaining_seconds(r, now_ms()))
            )));
            if r.phase == Phase::Paused {
                v.push(StatusEntry::Text(
                    "Waiting for Continue; manual pause time is not included".into(),
                ));
            }
            v.push(StatusEntry::Text(format!(
                "Files: {}",
                r.output_root.join(&r.measurement_id).display()
            )));
        } else {
            if let Some(plan) = &self.protocol_preview {
                let seconds: f64 = plan.points.iter().map(point_seconds).sum();
                let pauses = plan.points.iter().filter(|p| p.pause_before).count();
                v.push(StatusEntry::Text(format!(
                    "{} points; about {} plus file finalization and {pauses} manual pauses",
                    plan.points.len(),
                    format_bench_time(seconds)
                )));
            }
            if let Some(b) = self.blocker() {
                v.push(StatusEntry::Text(format!("Not ready: {b}")));
            }
        }
        v
    }
}

fn valid_measurement_id(id: &str) -> bool {
    let upper = id.to_ascii_uppercase();
    !id.is_empty()
        && id.len() <= 100
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        && !matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !(upper.len() == 4
            && (upper.starts_with("COM") || upper.starts_with("LPT"))
            && matches!(upper.as_bytes()[3], b'1'..=b'9'))
}

fn resolve_output_root(folder: &str) -> Result<PathBuf, String> {
    let root = Path::new(folder.trim());
    if !root.is_absolute() {
        return Err("choose an absolute data folder in the photodiode plugin".into());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create data folder: {e}"))?;
    root.canonicalize()
        .map_err(|e| format!("cannot resolve data folder: {e}"))
}

fn point_seconds(point: &Point) -> f64 {
    point.acquisition_seconds() + point.settle_s.max(0.25)
}

fn apply_roi_mode(snapshot: &mut CameraConfigurationSnapshotV1, mode: &str) -> Result<(), String> {
    if mode != "center_half" {
        return Err(format!("Unsupported ROI mode '{mode}'"));
    }
    let roi = snapshot.roi;
    let (x, width) = if roi.width == 0 {
        (0, snapshot.global.sensor_width)
    } else {
        (roi.x, roi.width)
    };
    let (y, height) = if roi.height == 0 {
        (0, snapshot.global.sensor_height)
    } else {
        (roi.y, roi.height)
    };
    if width < 2
        || height < 2
        || u32::from(x) + u32::from(width) > u32::from(snapshot.global.sensor_width)
        || u32::from(y) + u32::from(height) > u32::from(snapshot.global.sensor_height)
    {
        return Err(
            "Cannot derive a nested ROI from an invalid or smaller-than-two-pixel baseline".into(),
        );
    }
    snapshot.roi = augur_plugin_api::RoiV1 {
        x: x + (width - width / 2) / 2,
        y: y + (height - height / 2) / 2,
        width: width / 2,
        height: height / 2,
    };
    Ok(())
}

fn remaining_seconds(run: &Run, now: u64) -> f64 {
    let later: f64 = run
        .protocol
        .points
        .iter()
        .enumerate()
        .skip(run.index + 1)
        .filter(|(i, _)| !run.resumed_points.contains(i))
        .map(|(_, p)| point_seconds(p))
        .sum();
    let point = &run.protocol.points[run.index];
    let current = match run.phase {
        Phase::Settle => {
            run.deadline_ms.saturating_sub(now) as f64 / 1000.0 + point.acquisition_seconds()
        }
        Phase::StartCamera | Phase::StartPd | Phase::StartStimulus => point.acquisition_seconds(),
        Phase::Recording => run.deadline_ms.saturating_sub(now) as f64 / 1000.0,
        Phase::StopMod | Phase::FinalizePd | Phase::StopCamera => 0.0,
        Phase::ReleasePd | Phase::ReleaseMod | Phase::RestoreCamera => return 0.0,
        _ => point_seconds(point),
    };
    if run.stop {
        0.0
    } else {
        later + current
    }
}

fn format_bench_time(seconds: f64) -> String {
    let seconds = seconds.ceil() as u64;
    if seconds >= 3600 {
        format!("{} h {:02} min", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{} min {:02} s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds} s")
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn compact_time() -> String {
    now_ms().to_string()
}
fn safe(v: &str) -> String {
    v.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
fn photodiode_placement_name(placement: PhotodiodePlacementV1) -> &'static str {
    match placement {
        PhotodiodePlacementV1::RejectedPort => "rejected_port",
        PhotodiodePlacementV1::CameraPath => "camera_path",
        PhotodiodePlacementV1::EmissionPath => "emission_path",
    }
}
fn hex_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn archive_protocol(
    output_folder: &str,
    measurement_id: &str,
    sha256: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let directory = Path::new(output_folder).join(measurement_id);
    if std::fs::symlink_metadata(&directory).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err("measurement folder must not be a symlink; choose a new measurement id".into());
    }
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let path = directory.join(format!("protocol-{sha256}.toml"));
    if path.exists() {
        let existing = std::fs::read(&path).map_err(|error| error.to_string())?;
        if existing != bytes {
            return Err("content-addressed protocol archive has different bytes".into());
        }
    } else {
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    }
    Ok(path.to_string_lossy().into_owned())
}

fn camera_sidecar_path(raw_path: &str) -> String {
    Path::new(raw_path)
        .with_extension("toml")
        .to_string_lossy()
        .into_owned()
}

fn sensor_monitoring_path(raw_path: &str) -> String {
    let path = Path::new(raw_path);
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{stem}.sensor-monitoring.csv"))
        .to_string_lossy()
        .into_owned()
}

/// Quantized pedestal key. The milli units are exactly what reaches the
/// firmware, so two points that round to the same key really do share a drive.
fn pedestal_key(mean_u: f64, depth_a: f64) -> PedestalKey {
    (
        (mean_u * 1000.0).round() as u32,
        (depth_a * 1000.0).round() as u32,
    )
}

fn resolve_photodiode(summary: &PhotodiodeSummaryV1) -> ResolvedPhotodiode {
    ResolvedPhotodiode {
        owner_instance: summary.owner_instance.to_string(),
        placement: summary.placement,
        splitter_fraction: summary.splitter_fraction,
        reference_set_id: summary.reference_set_id.clone(),
        load_ohms: summary.load_ohms,
        dark_reference: summary.dark_reference.clone(),
        sample_rate_hz: summary.stream.sample_rate_hz,
        stream_epoch: summary.stream.stream_epoch,
        stream_integrity: summary.stream.integrity,
    }
}

/// Resolves the Kind-1 controller values from their owners.
///
/// Every branch either produces a value with its provenance or refuses. There
/// is no default: an unresolvable lobe or an unresolvable step floor must stop
/// the run while the bench is still untouched (ADR 040).
fn resolve_controller(
    controller: &ControllerSetup,
    modulation: Option<&ModulationStateV1>,
    sensor: Option<SensorMonitoringV1>,
) -> Result<ResolvedController, String> {
    let Some(state) = modulation else {
        return Err(
            "the Stage-A modulation owner has published no state, so the A2 lobe cannot be resolved"
                .into(),
        );
    };
    let Some(lobe) = state.optical_lobe.as_ref() else {
        return Err(
            "the modulation plugin has no applied optical calibration. Open its transfer-curve \
             calibration and select 'Apply to V_null / V_peak'. A2 sets mean_u and depth_a from \
             the protocol; you do not set a measurement point in the modulation plugin"
                .into(),
        );
    };
    if lobe.v_peak_dac <= lobe.v_null_dac {
        return Err(format!(
            "resolved lobe is not usable: v_peak_dac={} must exceed v_null_dac={}",
            lobe.v_peak_dac, lobe.v_null_dac
        ));
    }
    if lobe.v_peak_dac > STIMULUS_MAX_CODE {
        return Err(format!(
            "resolved lobe maximum {} is outside the stimulus DAC range 0..={STIMULUS_MAX_CODE}",
            lobe.v_peak_dac
        ));
    }
    if controller.comparator_hysteresis > 3 {
        return Err(format!(
            "comparator_hysteresis={} is outside the comparator's 0..=3 range",
            controller.comparator_hysteresis
        ));
    }
    if controller.min_half_us == Some(0) {
        let dead_time = sensor
            .and_then(|s| s.pixel_dead_time_us)
            .filter(|v| v.is_finite() && *v > 0.0);
        return Ok(ResolvedController {
            lobe_source: "modulation_owner.optical_lobe",
            v_null_dac: lobe.v_null_dac,
            v_peak_dac: lobe.v_peak_dac,
            modulation_calibration_id: lobe.calibration_id.clone(),
            modulation_owner_instance: state.owner_instance.to_string(),
            min_half_us: 0,
            min_half_us_source: "offline_timing_review",
            pixel_dead_time_us: dead_time,
            refractory_floor_us: dead_time.map_or(0, |v| (5.0 * f64::from(v)).ceil() as u32),
            settling_guard_us: 0,
        });
    }
    let Some(pixel_dead_time_us) = sensor.and_then(|s| s.pixel_dead_time_us) else {
        return Err(
            "sensor pixel-dead-time readout is missing, so the A2 step floor cannot be resolved"
                .into(),
        );
    };
    if !pixel_dead_time_us.is_finite() || pixel_dead_time_us <= 0.0 {
        return Err(format!(
            "sensor pixel-dead-time readout is {pixel_dead_time_us}, which is not a usable floor"
        ));
    }
    let refractory_floor_us = (5.0 * f64::from(pixel_dead_time_us)).ceil() as u32;
    let (min_half_us, min_half_us_source) = match controller.min_half_us {
        // A frozen floor is kept verbatim, and still has to clear the sensor's
        // own refractory bound: freezing a number may tighten the floor, never
        // loosen it.
        Some(frozen) => {
            if f64::from(frozen) < 5.0 * f64::from(pixel_dead_time_us) {
                return Err(format!(
                    "min_half_us={frozen} is below 5 x sensor dead time \
                     ({pixel_dead_time_us:.2} us)"
                ));
            }
            (frozen, "runner_configuration")
        }
        None => (
            refractory_floor_us.max(SETTLING_GUARD_US),
            "sensor_telemetry",
        ),
    };
    Ok(ResolvedController {
        lobe_source: "modulation_owner.optical_lobe",
        v_null_dac: lobe.v_null_dac,
        v_peak_dac: lobe.v_peak_dac,
        modulation_calibration_id: lobe.calibration_id.clone(),
        modulation_owner_instance: state.owner_instance.to_string(),
        min_half_us,
        min_half_us_source,
        pixel_dead_time_us: Some(pixel_dead_time_us),
        refractory_floor_us,
        settling_guard_us: SETTLING_GUARD_US,
    })
}

/// Checks every stepped half period against the resolved floor.
///
/// [`crate::protocol::Protocol::validate`] can only do this when the protocol
/// froze the floor itself; a protocol that leaves it to owner resolution is
/// checked here instead, still before the camera apply and the owner leases.
fn check_half_periods(plan: &Protocol, min_half_us: u32) -> Result<(), String> {
    for (index, point) in plan.points.iter().enumerate() {
        if !point.acquisition_seconds().is_finite()
            || point.acquisition_seconds() > 86_400.0
            || point.settle_s > 86_400.0
        {
            return Err(format!(
                "point {} exceeds the 24-hour duration limit",
                index + 1
            ));
        }
        if let Acquisition::Stepped { half_period_s, .. } = point.acquisition {
            let frequency = (500.0 / half_period_s).round() as u64;
            if !stage_a_plugin_contract::drive_frequency_supported(frequency)
                || 500_000_000.0 / (frequency as f64) < f64::from(min_half_us)
            {
                return Err(format!(
                    "point {} half-period cannot be produced within the firmware frequency range and resolved min_half_us",
                    index + 1
                ));
            }
        }

        let Acquisition::Stepped { half_period_s, .. } = point.acquisition else {
            continue;
        };
        if half_period_s * 1e6 < f64::from(min_half_us) {
            return Err(format!(
                "point {} half-period {:.0} us is below the resolved min_half_us={min_half_us}",
                index + 1,
                half_period_s * 1e6
            ));
        }
    }
    Ok(())
}

/// DAC code producing normalised optical intensity `u` on the increasing lobe:
/// `V(u) = V_null + (2 Vpi / pi) * arcsin(sqrt(u))`.
///
/// The same inversion the firmware's `dacForU` applies, so a plateau this
/// plugin holds is exactly one of the two levels the `LOG_SQUARE` drive will
/// alternate between at the same pedestal.
fn dac_for_u(u: f64, v_null: f64, v_pi: f64) -> f64 {
    v_null + (2.0 * v_pi / std::f64::consts::PI) * u.clamp(0.0, 1.0).sqrt().asin()
}

/// The two static levels an A2 log-square alternates between at one pedestal.
///
/// Mirrors `configureLogSquare`: `ln u` swings by `+-a/2` around `ln(mean_u)`,
/// and each level is inverted through the resolved lobe. Refuses for the same
/// reasons the firmware refuses, so an unreachable pedestal is caught at
/// preflight instead of half way through the run.
fn resolve_plateaus(
    mean_u_milli: u32,
    depth_a_milli: u32,
    v_null_dac: u16,
    v_peak_dac: u16,
) -> Result<PedestalPlateaus, String> {
    if !(1..=1_000).contains(&mean_u_milli) {
        return Err(format!(
            "pedestal mean_u={mean_u_milli} m is outside the lobe coordinate range (0, 1]"
        ));
    }
    if depth_a_milli == 0 {
        return Err("pedestal depth a quantises to zero".into());
    }
    if v_peak_dac <= v_null_dac {
        return Err("resolved lobe has no span".into());
    }
    let mean_u = f64::from(mean_u_milli) / 1000.0;
    let depth_a = f64::from(depth_a_milli) / 1000.0;
    let v_null = f64::from(v_null_dac);
    let v_pi = f64::from(v_peak_dac) - v_null;
    let u_high = mean_u * (0.5 * depth_a).exp();
    let u_low = mean_u * (-0.5 * depth_a).exp();
    if u_high > 1.0 + 1e-6 {
        return Err(format!(
            "pedestal mean_u={mean_u:.3} at depth a={depth_a:.3} peaks at u={u_high:.3}, past the \
             lobe maximum"
        ));
    }
    let code_high = dac_for_u(u_high, v_null, v_pi).round();
    let code_low = dac_for_u(u_low, v_null, v_pi).round();
    if !code_high.is_finite() || !code_low.is_finite() {
        return Err("pedestal plateau inversion is not finite".into());
    }
    let range = 0.0..=f64::from(STIMULUS_MAX_CODE);
    if !range.contains(&code_low) || !range.contains(&code_high) {
        return Err(format!(
            "pedestal plateaus at DAC {code_low:.0}/{code_high:.0} leave the stimulus range \
             0..={STIMULUS_MAX_CODE}"
        ));
    }
    let low_dac = code_low as u16;
    let high_dac = code_high as u16;
    if low_dac == high_dac {
        return Err(format!(
            "pedestal mean_u={mean_u:.3} at depth a={depth_a:.3} quantises to a single DAC code \
             {low_dac}; there is no step to place a threshold inside"
        ));
    }
    Ok(PedestalPlateaus {
        mean_u_milli,
        depth_a_milli,
        low_dac,
        high_dac,
    })
}

/// Plateau pairs for every distinct pedestal that asks for an auto threshold.
fn plan_pedestals(
    plan: &Protocol,
    resolved: &ResolvedController,
) -> Result<BTreeMap<PedestalKey, PedestalPlateaus>, String> {
    let mut pedestals = BTreeMap::new();
    for (index, point) in plan.points.iter().enumerate() {
        let Acquisition::Stepped {
            mean_u,
            depth_a,
            comparator_threshold_dac,
            ..
        } = point.acquisition
        else {
            continue;
        };
        let _ = comparator_threshold_dac;
        let key = pedestal_key(mean_u, depth_a);
        if pedestals.contains_key(&key) {
            continue;
        }
        let plateaus = resolve_plateaus(key.0, key.1, resolved.v_null_dac, resolved.v_peak_dac)
            .map_err(|error| format!("point {}: {error}", index + 1))?;
        pedestals.insert(key, plateaus);
    }
    Ok(pedestals)
}

/// Comparator threshold DAC code for a level measured on the photodiode ADC.
///
/// The two converters do not share a reference: the photodiode ADC spans
/// 0..3300 mV over 4095 codes, the threshold DAC 0..2500 mV over 4095 codes. An
/// ADC code is therefore *not* a threshold code, and the only honest currency
/// between them is the physical voltage. `PhotodiodeLevelV1::mean_volts` has
/// already had the ADC affine map applied by the owner, so this converts volts
/// into the DAC's own scale — and refuses what the DAC cannot reach, because
/// clamping would park the threshold on a rail and still report success.
fn threshold_code_for_volts(volts: f64) -> Result<u16, String> {
    if !volts.is_finite() {
        return Err("plateau midpoint is not a finite voltage".into());
    }
    let millivolt = volts * 1000.0;
    let code = (millivolt * f64::from(THRESHOLD_MAX_CODE) / THRESHOLD_FULL_SCALE_MILLIVOLT).round();
    if !(0.0..=THRESHOLD_FULL_SCALE_MILLIVOLT).contains(&millivolt)
        || !(1.0..=f64::from(THRESHOLD_MAX_CODE)).contains(&code)
    {
        return Err(format!(
            "plateau midpoint {millivolt:.1} mV is outside the 0..2500 mV comparator threshold \
             DAC range"
        ));
    }
    Ok(code as u16)
}

fn aggregate_levels(step: &PlateauStep) -> PhotodiodeLevelV1 {
    let levels: Vec<_> = step.windows.iter().flatten().copied().collect();
    let count: u64 = levels.iter().map(|v| v.sample_count).sum();
    PhotodiodeLevelV1 {
        mean_volts: levels
            .iter()
            .map(|v| v.mean_volts * v.sample_count as f64)
            .sum::<f64>()
            / count as f64,
        peak_to_peak_volts: levels
            .iter()
            .map(|v| v.peak_to_peak_volts)
            .fold(0.0, f64::max),
        sample_count: count,
        end_sample_index: levels.last().unwrap().end_sample_index,
        clipped: levels.iter().any(|v| v.clipped),
    }
}

fn mean_statistics(windows: &[PhotodiodeLevelV1]) -> (f64, f64) {
    if windows.len() < 2 {
        return (0.0, 0.0);
    }
    let n = windows.len() as f64;
    let mean = windows.iter().map(|v| v.mean_volts).sum::<f64>() / n;
    let variance = windows
        .iter()
        .map(|v| (v.mean_volts - mean).powi(2))
        .sum::<f64>()
        / (n - 1.0);
    let min = windows
        .iter()
        .map(|v| v.mean_volts)
        .fold(f64::INFINITY, f64::min);
    let max = windows
        .iter()
        .map(|v| v.mean_volts)
        .fold(f64::NEG_INFINITY, f64::max);
    (max - min, (variance / n).sqrt())
}

/// Turns two settled plateaus into a `V_50`, or explains why it will not.
fn measured_threshold(
    probe: &PlateauProbe,
    low: PhotodiodeLevelV1,
    high: PhotodiodeLevelV1,
) -> Result<MeasuredThreshold, String> {
    let span_volts = high.mean_volts - low.mean_volts;
    if !span_volts.is_finite() || span_volts < MIN_PLATEAU_SPAN_VOLTS {
        return Err(format!(
            "plateau span {:.3} mV is under the {:.3} mV threshold-DAC resolution floor at mean_u={} m, a={} m; increase the optical step",
            span_volts * 1000.0,
            MIN_PLATEAU_SPAN_VOLTS * 1000.0,
            probe.mean_u_milli,
            probe.depth_a_milli
        ));
    }
    let low_windows: Vec<_> = probe.low.windows.iter().flatten().copied().collect();
    let high_windows: Vec<_> = probe.high.windows.iter().flatten().copied().collect();
    let (low_spread, low_se) = mean_statistics(&low_windows);
    let (high_spread, high_se) = mean_statistics(&high_windows);
    let difference_se = low_se.hypot(high_se);
    if low_spread > MAX_PLATEAU_MEAN_SPREAD_FRACTION * span_volts
        || high_spread > MAX_PLATEAU_MEAN_SPREAD_FRACTION * span_volts
        || span_volts <= 6.0 * difference_se
    {
        return Err(format!(
            "V50 unresolved: dim/bright mean {:.2}/{:.2} mV; step {:.2} mV; \
            spread of independent window means {:.2}/{:.2} mV; mean-difference SE {:.2} mV. \
            Raw peak-to-peak {:.1}/{:.1} mV. Increase optical step or reduce drift/noise",
            low.mean_volts * 1000.0,
            high.mean_volts * 1000.0,
            span_volts * 1000.0,
            low_spread * 1000.0,
            high_spread * 1000.0,
            difference_se * 1000.0,
            low.peak_to_peak_volts * 1000.0,
            high.peak_to_peak_volts * 1000.0
        ));
    }
    let midpoint_volts = 0.5 * (low.mean_volts + high.mean_volts);
    let threshold_dac = threshold_code_for_volts(midpoint_volts)?;
    Ok(MeasuredThreshold {
        mean_u_milli: probe.mean_u_milli,
        depth_a_milli: probe.depth_a_milli,
        low_level_dac: probe.low.level_dac,
        high_level_dac: probe.high.level_dac,
        low_plateau_volts: low.mean_volts,
        high_plateau_volts: high.mean_volts,
        low_window_peak_to_peak_volts: low.peak_to_peak_volts,
        high_window_peak_to_peak_volts: high.peak_to_peak_volts,
        low_windows,
        high_windows,
        low_mean_spread_volts: low_spread,
        high_mean_spread_volts: high_spread,
        mean_difference_standard_error_volts: difference_se,
        noisy_crossing_requires_review: low.peak_to_peak_volts > 0.5 * span_volts
            || high.peak_to_peak_volts > 0.5 * span_volts,
        span_volts,
        midpoint_volts,
        threshold_dac,
        threshold_millivolt: f64::from(threshold_dac) * THRESHOLD_FULL_SCALE_MILLIVOLT
            / f64::from(THRESHOLD_MAX_CODE),
        low_window_sample_count: low.sample_count,
        low_window_end_sample_index: low.end_sample_index,
        low_drive_acknowledged_sample_index: probe.low.acknowledged_sample_index,
        high_window_sample_count: high.sample_count,
        high_window_end_sample_index: high.end_sample_index,
        high_drive_acknowledged_sample_index: probe.high.acknowledged_sample_index,
        stream_epoch: probe.high.stream_epoch,
        measured_at_unix_ms: now_ms(),
    })
}

fn acquisition_mode(acquisition: &Acquisition) -> &'static str {
    match acquisition {
        Acquisition::Dark { .. } => "dark",
        Acquisition::Stepped { .. } => "stepped",
    }
}

fn starts_modulation(acquisition: &Acquisition) -> bool {
    matches!(acquisition, Acquisition::Stepped { .. })
}

fn should_pause(point: &Point, acknowledged: bool) -> bool {
    point.pause_before && !acknowledged
}

fn pause_message(point: &Point) -> String {
    if point.role == "blocked_drive_sham" {
        return format!(
            "Paused before '{}': keep the optical path blocked. A2 will run the drive \
            and record the blocked-drive electrical/crosstalk reference. Then press Continue.",
            point.label
        );
    }
    match point.acquisition {
        Acquisition::Dark { duration_s } => format!(
            "Paused before '{}': close or block the optical path so no light reaches the camera \
             or photodiode. Check that the photodiode trace shows only the dark baseline. Then \
             press Continue. A2 switches modulation off and records {duration_s:.3} s \
             automatically.",
            point.label
        ),
        Acquisition::Stepped {
            mean_u,
            depth_a,
            half_period_s,
            ..
        } => format!(
            "Paused before '{}': open the optical path and confirm the sample is ready. Do not \
             set a measurement point in the modulation plugin. A2 automatically commands \
             mean_u={mean_u:.3}, depth_a={depth_a:.3}, and half-period={half_period_s:.6} s from \
             this protocol. Then press Continue.",
            point.label
        ),
    }
}

fn owner_rejection_message(kind: PendingKind, code: &str, message: &str) -> String {
    let owner = match kind {
        PendingKind::Mod | PendingKind::RenewMod => "modulation plugin",
        PendingKind::Pd | PendingKind::RenewPd => "photodiode plugin",
        PendingKind::Host => "camera host",
    };
    let lease_problem = code.to_ascii_lowercase().contains("lease")
        || message.to_ascii_lowercase().contains("lease");
    if lease_problem {
        return format!(
            "The {owner} rejected the A2 run because its lease state does not match ({code}: \
             {message}). This is a software state problem, not a hardware measurement failure. \
             Stop A2, reload A2, the modulation plugin and the photodiode plugin, then start the \
             protocol again."
        );
    }
    format!(
        "The {owner} rejected the current A2 step ({code}: {message}). Check that plugin's status \
         line for the required action, then start A2 again."
    )
}

fn validate_trigger_counts(
    acquisition: &Acquisition,
    rising: u64,
    falling: u64,
) -> Result<(), String> {
    let Acquisition::Stepped {
        transitions_per_polarity,
        ..
    } = acquisition
    else {
        return Ok(());
    };
    let expected = u64::from(*transitions_per_polarity);
    let minimum = expected.saturating_sub(1);
    let maximum = expected.saturating_add(1);
    if !(minimum..=maximum).contains(&rising) || !(minimum..=maximum).contains(&falling) {
        return Err(format!(
            "live-preview trigger count differs (preview is best-effort; verify RAW offline): expected {expected} +/- 1 per polarity, observed rising={rising}, falling={falling}"
        ));
    }
    Ok(())
}

fn camera_configuration_refusal(
    snapshot: &CameraConfigurationSnapshotV1,
    readback_age_s: f64,
) -> Option<String> {
    if snapshot.digital_filter.stc_enabled
        || snapshot.digital_filter.trail_enabled
        || snapshot.digital_filter.erc_enabled != Some(false)
    {
        return Some(
            "applied camera configuration must explicitly confirm STC, Trail and ERC off".into(),
        );
    }
    if !snapshot.external_trigger.enabled {
        return Some("applied camera configuration has EXT_TRIGGER disabled".into());
    }
    if !snapshot.global.record_sensor_telemetry {
        return Some("applied camera configuration does not record sensor telemetry".into());
    }
    if !readback_age_s.is_finite() || readback_age_s < 0.0 {
        return Some("host returned an invalid sensor readback age".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_owner_rows_parse_and_match_campaign_timing() {
        for entry in stage_a_universal_runner::builtin_protocols()
            .iter()
            .filter(|entry| {
                matches!(
                    entry.experiment,
                    stage_a_universal_runner::Experiment::A2
                        | stage_a_universal_runner::Experiment::A5
                )
            })
        {
            let plan =
                protocol::parse(entry.contents).unwrap_or_else(|e| panic!("{}: {e}", entry.name));
            assert_eq!(plan.points.len(), entry.points, "{}", entry.name);
            let seconds = plan
                .points
                .iter()
                .map(|p| p.acquisition_seconds() + p.settle_s + 0.25 + 11.0)
                .sum::<f64>();
            assert!(
                (seconds - entry.seconds).abs() < 0.01,
                "{}: {seconds} != {}",
                entry.name,
                entry.seconds
            );
        }
    }

    use crate::protocol::ComparatorThreshold;

    #[test]
    fn nested_roi_is_centered_and_does_not_change_the_baseline() {
        let mut snapshot = qualified_camera().0;
        snapshot.global.sensor_width = 1280;
        snapshot.global.sensor_height = 720;
        snapshot.roi = augur_plugin_api::RoiV1 {
            x: 100,
            y: 40,
            width: 200,
            height: 160,
        };
        let original = snapshot.clone();
        apply_roi_mode(&mut snapshot, "center_half").unwrap();
        assert_eq!(
            snapshot.roi,
            augur_plugin_api::RoiV1 {
                x: 150,
                y: 80,
                width: 100,
                height: 80
            }
        );
        assert_eq!(snapshot.biases, original.biases);
        assert_eq!(original.roi.width, 200);
        assert!(apply_roi_mode(&mut snapshot, "unknown").is_err());
    }

    #[derive(Default)]
    struct MockControl {
        services: Vec<PluginServiceRequest>,
        hosts: Vec<HostCommandRequest>,
    }

    impl Control for MockControl {
        fn service(&mut self, request: &PluginServiceRequest) {
            self.services.push(request.clone());
        }
        fn host(&mut self, request: &HostCommandRequest) {
            self.hosts.push(request.clone());
        }
    }

    fn test_protocol() -> &'static str {
        r#"
name="a2-e2e"
[[point]]
label="dark"
role="floor"
acquisition_mode="dark"
duration_s=0.001
settle_s=0
pause_before=true
[[point]]
label="step"
role="identification"
acquisition_mode="stepped"
mean_u=0.3
depth_a=0.45
half_period_s=0.001
transitions_per_polarity=2
comparator_threshold_dac=500
settle_s=0
"#
    }

    fn test_folder(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("stage-a-a2-{label}-{}", now_ms()))
    }

    fn ready_plugin(label: &str) -> StageAA2Plugin {
        use stage_a_plugin_contract::{
            ControllerStateV1, FreshnessV1, OwnerInstanceId, PhotodiodeStreamV1, StreamIntegrityV1,
            SynchronizationV1, UnsyncedReasonV1, CONTRACT_VERSION_V1,
        };
        let folder = test_folder(label);
        std::fs::create_dir_all(&folder).unwrap();
        let protocol_path = folder.join("protocol.toml");
        std::fs::write(&protocol_path, test_protocol()).unwrap();
        let settings = GlobalSettings {
            nm_per_pixel: 1.0,
            sensor_width: 1280,
            sensor_height: 720,
            acq_time_ms: 1,
            event_store_budget_bytes: 1 << 20,
            record_sensor_telemetry: true,
            roi: augur_plugin_api::RoiV1 {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            },
            masked_pixels: vec![],
            event_filters: augur_plugin_api::EventFiltersV1::default(),
        };
        let sensor = SensorMonitoringV1 {
            pixel_dead_time_us: Some(10.0),
            illumination_lux: Some(0.1),
            temperature_c: Some(25.0),
            bias_codes: None,
            age_s: 0.1,
        };
        let modulation = ModulationStateV1 {
            contract_version: CONTRACT_VERSION_V1,
            owner_instance: OwnerInstanceId::new("mod-test"),
            service_revision: 1,
            connection: ConnectionStateV1::Connected {
                port_label: "mock".into(),
                firmware_version: Some("test".into()),
            },
            capabilities: vec![],
            lease: None,
            controller_state: ControllerStateV1::Configured,
            controller_mode: Some("A2".into()),
            active_run_id: None,
            requested: None,
            acknowledged: None,
            synchronization: SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::NoLease,
                detail: None,
            },
            last_response: None,
            freshness: FreshnessV1 {
                observed_at_unix_ms: now_ms(),
                valid_for_ms: 60_000,
            },
            calibration_id: Some("lobe-1".into()),
            optical_lobe: Some(stage_a_plugin_contract::OpticalLobeStateV1 {
                calibration_id: "lobe-1".into(),
                v_null_dac: 100,
                v_peak_dac: 1_000,
            }),
            optical_drive: None,
        };
        let photodiode = PhotodiodeSummaryV1 {
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
                sample_rate_hz: Some(500_000),
                latest_adc_code: Some(1000),
                integrity: StreamIntegrityV1::default(),
                level: None,
            },
            data_dir: Some(folder.to_string_lossy().into_owned()),
            active_recording: None,
            last_finalized_recording: None,
            optical_summary: None,
            optical_unavailable: None,
            placement: PhotodiodePlacementV1::EmissionPath,
            splitter_fraction: Some(0.5),
            reference_set_id: Some("pdref-test".into()),
            load_ohms: Some(470_000.0),
            dark_reference: None,
            synchronization: SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::NoLease,
                detail: None,
            },
            last_response: None,
            freshness: FreshnessV1 {
                observed_at_unix_ms: now_ms(),
                valid_for_ms: 60_000,
            },
        };
        StageAA2Plugin {
            output_folder: folder.to_string_lossy().into_owned(),
            protocol_path: protocol_path.to_string_lossy().into_owned(),
            measurement_id: format!("A2-{label}"),
            settings: Some(settings),
            sensor: Some(sensor),
            modulation: Some(modulation),
            photodiode: Some(photodiode),
            ..StageAA2Plugin::default()
        }
    }

    fn qualified_camera() -> (
        CameraConfigurationSnapshotV1,
        CameraConfigurationProvenanceV1,
    ) {
        use augur_plugin_api::{
            CameraBiasOffsetsV1, CameraDigitalFilterV1, CameraExternalTriggerV1,
            CameraGlobalSettingsV1, RoiV1,
        };
        (
            CameraConfigurationSnapshotV1 {
                schema_version: 1,
                biases: CameraBiasOffsetsV1::default(),
                roi: RoiV1 {
                    x: 0,
                    y: 0,
                    width: 1280,
                    height: 720,
                },
                masked_pixels: vec![],
                digital_filter: CameraDigitalFilterV1 {
                    stc_enabled: false,
                    stc_threshold_us: 0,
                    trail_enabled: false,
                    erc_enabled: Some(false),
                },
                external_trigger: CameraExternalTriggerV1 {
                    enabled: true,
                    channel: 0,
                },
                global: CameraGlobalSettingsV1 {
                    nm_per_pixel: 1.0,
                    pixel_scale_calibrated: true,
                    sensor_width: 1280,
                    sensor_height: 720,
                    acq_time_ms: 1,
                    event_store_budget_mib: 512,
                    preview_interval_ms: 16,
                    point_cloud_interval_ms: 50,
                    disk_writer_buffer_mib: 64,
                    record_sensor_telemetry: true,
                },
            },
            CameraConfigurationProvenanceV1 {
                source: "named_profile".into(),
                profile_name: Some("A2 qualified".into()),
                schema_version: 1,
                profile_revision: Some(1),
                sha256: "ab".repeat(32),
            },
        )
    }

    fn applied_camera_outcome() -> HostCommandOutcome {
        let (snapshot, provenance) = qualified_camera();
        HostCommandOutcome::CameraConfigurationApplied {
            snapshot,
            provenance,
            readback: augur_plugin_api::SensorBiasReadbackV1::default(),
            readback_age_s: 0.1,
        }
    }

    fn started_pd_payload(request_id: u64, run_id: &str, pdq: &str, sidecar: &str) -> Value {
        use stage_a_plugin_contract::{
            OwnerInstanceId, PdqStartedReceiptV1, ResponseCommonV1, CONTRACT_VERSION_V1,
        };
        serde_json::to_value(PhotodiodeResponseV1 {
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: RequestId(request_id),
                owner_instance: OwnerInstanceId::new("pd-test"),
                run_id: Some(RunId::new(run_id.split("_r").next().unwrap())),
                requested_revision: None,
                acknowledged_revision: None,
                outcome: RequestOutcomeV1::Applied,
                completed_at_unix_ms: Some(now_ms()),
                error: None,
            },
            receipt: Some(PdqReceiptV1::Started(PdqStartedReceiptV1 {
                run_id: RunId::new(run_id),
                pdq_path: pdq.into(),
                sidecar_path: sidecar.into(),
                opened_at_unix_ms: now_ms(),
                stream_epoch: 1,
                first_sample_index: Some(0),
            })),
        })
        .unwrap()
    }

    fn finalized_pd_payload(request_id: u64, run_id: &str) -> Value {
        use stage_a_plugin_contract::{
            OwnerInstanceId, PdqFinalizedReceiptV1, ResponseCommonV1, Sha256V1, StreamIntegrityV1,
            CONTRACT_VERSION_V1,
        };
        serde_json::to_value(PhotodiodeResponseV1 {
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: RequestId(request_id),
                owner_instance: OwnerInstanceId::new("pd-test"),
                run_id: Some(RunId::new(run_id.split("_r").next().unwrap())),
                requested_revision: None,
                acknowledged_revision: None,
                outcome: RequestOutcomeV1::Applied,
                completed_at_unix_ms: Some(now_ms()),
                error: None,
            },
            receipt: Some(PdqReceiptV1::Finalized(PdqFinalizedReceiptV1 {
                run_id: RunId::new(run_id),
                pdq_path: format!("{run_id}.pdq"),
                sidecar_path: format!("{run_id}.pd.json"),
                opened_at_unix_ms: now_ms(),
                finalized_at_unix_ms: now_ms(),
                file_size_bytes: 1,
                sha256: Sha256V1::parse("cd".repeat(32)).unwrap(),
                frames_written: 1,
                sample_frames_written: 1,
                marker_counts: Some(stage_a_plugin_contract::PdqMarkerCountsV1 {
                    comparator_rising: 2,
                    comparator_falling: 2,
                    phase_zero: 2,
                    invalid_level: 0,
                }),
                sample_range: Some(stage_a_plugin_contract::SampleRangeV1 {
                    first_sample_index: 0,
                    end_sample_index_exclusive: 5_000,
                    sample_count: 5_000,
                }),
                sample_rate_hz: Some(500_000),
                segment_count: 1,
                integrity: StreamIntegrityV1::default(),
                termination: PdqTerminationV1::Completed,
                valid: true,
            })),
        })
        .unwrap()
    }

    fn paused_point() -> Point {
        Point {
            label: "shutter".into(),
            role: "dark".into(),
            settle_s: 0.0,
            pause_before: true,
            acquisition: Acquisition::Dark { duration_s: 30.0 },
        }
    }

    #[test]
    fn continue_acknowledges_a_pause_once_for_the_current_point() {
        let point = paused_point();
        assert!(should_pause(&point, false));
        assert!(!should_pause(&point, true));
    }

    #[test]
    fn pause_messages_separate_physical_actions_from_automatic_settings() {
        let dark = pause_message(&paused_point());
        assert!(dark.contains("close or block the optical path"), "{dark}");
        assert!(dark.contains("press Continue"), "{dark}");
        assert!(dark.contains("automatically"), "{dark}");

        let stepped = Point {
            label: "technical_pre".into(),
            role: "technical".into(),
            settle_s: 0.0,
            pause_before: true,
            acquisition: Acquisition::Stepped {
                mean_u: 0.3,
                depth_a: 0.45,
                half_period_s: 0.001,
                transitions_per_polarity: 2,
                comparator_threshold_dac: ComparatorThreshold::Auto,
            },
        };
        let message = pause_message(&stepped);
        assert!(message.contains("open the optical path"), "{message}");
        assert!(
            message.contains("Do not set a measurement point"),
            "{message}"
        );
        assert!(message.contains("mean_u=0.300"), "{message}");
        assert!(message.contains("depth_a=0.450"), "{message}");
        assert!(message.contains("press Continue"), "{message}");
    }

    #[test]
    fn lease_rejection_names_the_problem_and_recovery_action() {
        let message = owner_rejection_message(
            PendingKind::Mod,
            "lease_mismatch",
            "request run does not match the leased run",
        );
        assert!(message.contains("software state problem"), "{message}");
        assert!(message.contains("reload A2"), "{message}");
        assert!(message.contains("modulation plugin"), "{message}");
    }

    #[test]
    fn dark_points_do_not_require_external_triggers() {
        assert!(validate_trigger_counts(&paused_point().acquisition, 0, 0).is_ok());
        assert!(!starts_modulation(&paused_point().acquisition));
    }

    #[test]
    fn stepped_trigger_counts_must_match_the_commanded_count() {
        let acquisition = Acquisition::Stepped {
            mean_u: 0.3,
            depth_a: 0.45,
            half_period_s: 1.0,
            transitions_per_polarity: 100,
            comparator_threshold_dac: ComparatorThreshold::Frozen(500),
        };
        assert!(validate_trigger_counts(&acquisition, 100, 99).is_ok());
        assert!(validate_trigger_counts(&acquisition, 2, 2).is_err());
    }

    #[test]
    fn camera_configuration_requires_explicit_erc_off() {
        use augur_plugin_api::{
            CameraBiasOffsetsV1, CameraDigitalFilterV1, CameraExternalTriggerV1,
            CameraGlobalSettingsV1, RoiV1,
        };
        let mut snapshot = CameraConfigurationSnapshotV1 {
            schema_version: 1,
            biases: CameraBiasOffsetsV1::default(),
            roi: RoiV1 {
                x: 0,
                y: 0,
                width: 1280,
                height: 720,
            },
            masked_pixels: vec![],
            digital_filter: CameraDigitalFilterV1 {
                stc_enabled: false,
                stc_threshold_us: 0,
                trail_enabled: false,
                erc_enabled: None,
            },
            external_trigger: CameraExternalTriggerV1 {
                enabled: true,
                channel: 0,
            },
            global: CameraGlobalSettingsV1 {
                nm_per_pixel: 1.0,
                pixel_scale_calibrated: true,
                sensor_width: 1280,
                sensor_height: 720,
                acq_time_ms: 1,
                event_store_budget_mib: 512,
                preview_interval_ms: 16,
                point_cloud_interval_ms: 50,
                disk_writer_buffer_mib: 64,
                record_sensor_telemetry: true,
            },
        };
        assert!(camera_configuration_refusal(&snapshot, 0.1)
            .unwrap()
            .contains("ERC"));
        snapshot.digital_filter.erc_enabled = Some(false);
        assert!(camera_configuration_refusal(&snapshot, 0.1).is_none());
    }

    #[test]
    fn a2_ui_reads_owner_settings_but_offers_a_measurement_id() {
        let plugin = StageAA2Plugin::default();
        let keys = plugin
            .settings_schema()
            .sections
            .into_iter()
            .flat_map(|section| section.items)
            .map(|item| item.key)
            .collect::<Vec<_>>();
        assert!(!keys.iter().any(|key| key == "output_folder"));
        assert!(keys.iter().any(|key| key == "measurement_id"));
        assert!(keys.iter().any(|key| key == "protocol_path"));
    }

    #[test]
    fn manifest_declares_every_camera_command_used_by_a2() {
        let manifest: toml::Value = toml::from_str(include_str!("../plugin.toml")).unwrap();
        let commands = manifest["host_commands"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .collect::<Vec<_>>();
        for required in [
            "start_recording",
            "stop_recording",
            "apply_camera_configuration",
            "restore_camera_configuration",
        ] {
            assert!(commands.contains(&required), "missing {required}");
        }
    }

    #[test]
    fn end_to_end_runs_paused_dark_then_stepped_and_restores_camera() {
        let mut plugin = ready_plugin("e2e-success");
        let expected_output_folder = plugin
            .photodiode
            .as_ref()
            .and_then(|summary| summary.data_dir.clone())
            .unwrap();
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        assert_eq!(
            Path::new(&plugin.output_folder),
            Path::new(&expected_output_folder).canonicalize().unwrap()
        );
        assert!(plugin.measurement_id.starts_with("A2-"));
        let measurement_id = plugin.measurement_id.clone();
        assert!(matches!(
            &control.hosts.last().unwrap().command,
            HostCommand::ApplyCameraConfiguration {
                configuration: CameraConfigurationSourceV1::Current
            }
        ));
        let apply_id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, apply_id, applied_camera_outcome());
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::AcquireMod);

        let acquire_mod: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let lease_run_id = acquire_mod.run_id.clone().expect("modulation lease run id");

        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        let acquire_pd: PhotodiodeRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert_eq!(acquire_pd.run_id.as_ref(), Some(&lease_run_id));
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Paused);
        plugin.continue_pending = true;
        plugin.drive(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Prepare);
        let dark_prepare: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert_eq!(dark_prepare.run_id.as_ref(), Some(&lease_run_id));
        assert!(matches!(
            dark_prepare.command,
            ModulationCommandV1::SafeOff { .. }
        ));
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        let camera_start = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            camera_start,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: Path::new(&expected_output_folder)
                    .join(&measurement_id)
                    .join("dark.raw")
                    .to_string_lossy()
                    .into_owned(),
                started_at: "now".into(),
            },
        );
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Recording);
        assert!(!plugin.run.as_ref().unwrap().modulation_active);
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::FinalizePd);
        let pd_id = plugin.run.as_ref().unwrap().pending.unwrap().1;
        let run_id = plugin.run.as_ref().unwrap().run_id.clone();
        plugin.accepted(
            &mut control,
            PendingKind::Pd,
            &finalized_pd_payload(pd_id, &run_id),
        );
        let stop_camera_id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            stop_camera_id,
            HostCommandOutcome::RecordingFinalized {
                actual_raw_path: Path::new(&expected_output_folder)
                    .join(&measurement_id)
                    .join("dark.raw")
                    .to_string_lossy()
                    .into_owned(),
                size: 1,
                sha256: "ef".repeat(32),
                duration_us: 1_000,
            },
        );

        let sidecar_path = Path::new(&expected_output_folder)
            .join(&measurement_id)
            .join(format!("{run_id}.a2.json"));
        let sidecar: Value = serde_json::from_slice(&std::fs::read(sidecar_path).unwrap()).unwrap();
        assert_eq!(sidecar["camera_source"], "current_host_configuration");
        assert_eq!(
            sidecar["photodiode_setup"]["reference_set_id"],
            "pdref-test"
        );
        assert_eq!(sidecar["photodiode_setup"]["load_ohms"], 470_000.0);
        assert_eq!(
            sidecar["scientific_status"],
            "requires_offline_h4_h5_review"
        );

        assert_eq!(plugin.run.as_ref().unwrap().index, 1);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Prepare);
        let stepped_prepare: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert!(matches!(
            stepped_prepare.command,
            ModulationCommandV1::PrepareA2 { .. }
        ));
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        let camera_start = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            camera_start,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: Path::new(&expected_output_folder)
                    .join(&measurement_id)
                    .join("step.raw")
                    .to_string_lossy()
                    .into_owned(),
                started_at: "now".into(),
            },
        );
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Recording);
        assert!(
            !control.services.iter().any(|request| {
                serde_json::from_value::<ModulationRequestV1>(request.payload.clone())
                    .is_ok_and(|r| matches!(r.command, ModulationCommandV1::StartAcquisition))
            }),
            "A2 must never reset the PD sample clock with START"
        );
        {
            let run = plugin.run.as_mut().unwrap();
            run.evidence.rising_triggers = 2;
            run.evidence.falling_triggers = 2;
            let counters = stage_a_plugin_contract::A2MarkerDiagnosticsV1 {
                dma_sample_clock: true,
                marker_drops: 0,
                stream_marker_drops: 0,
                observed_at_unix_ms: now_ms(),
            };
            run.evidence.marker_diagnostics_before = Some(counters);
            run.evidence.marker_diagnostics_after = Some(counters);
            run.deadline_ms = 0;
        }
        plugin.drive(&mut control);
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        let pd_id = plugin.run.as_ref().unwrap().pending.unwrap().1;
        let run_id = plugin.run.as_ref().unwrap().run_id.clone();
        plugin.accepted(
            &mut control,
            PendingKind::Pd,
            &finalized_pd_payload(pd_id, &run_id),
        );
        let stop_camera_id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            stop_camera_id,
            HostCommandOutcome::RecordingFinalized {
                actual_raw_path: Path::new(&expected_output_folder)
                    .join(&measurement_id)
                    .join("step.raw")
                    .to_string_lossy()
                    .into_owned(),
                size: 1,
                sha256: "12".repeat(32),
                duration_us: 4_000,
            },
        );
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::ReleasePd);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::ReleaseMod);
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::RestoreCamera);
        let restore_id = control.hosts.last().unwrap().request_id;
        assert!(matches!(
            control.hosts.last().unwrap().command,
            HostCommand::RestoreCameraConfiguration
        ));
        plugin.host_reply(
            &mut control,
            restore_id,
            HostCommandOutcome::CameraConfigurationRestored {
                readback: augur_plugin_api::SensorBiasReadbackV1::default(),
                readback_age_s: 0.1,
            },
        );
        assert!(plugin.run.is_none());
    }

    #[test]
    fn failure_before_pd_lease_releases_owned_resources_then_restores_camera() {
        let mut plugin = ready_plugin("e2e-failure");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let apply_id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, apply_id, applied_camera_outcome());
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert!(plugin.run.as_ref().unwrap().mod_leased);
        assert!(!plugin.run.as_ref().unwrap().pd_leased);

        plugin.fail(&mut control, "photodiode lease rejected".into());
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::ReleaseMod);
        let release: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert!(matches!(
            release.command,
            ModulationCommandV1::ReleaseLease { safe_off: true, .. }
        ));
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::RestoreCamera);
        let restore_id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            restore_id,
            HostCommandOutcome::CameraConfigurationRestored {
                readback: augur_plugin_api::SensorBiasReadbackV1::default(),
                readback_age_s: 0.1,
            },
        );
        assert!(plugin.run.is_none());
        assert!(plugin.message.contains("A2 stopped"));
    }

    // ---------------------------------------------------------------------
    // Stage 1 — Kind-1 values resolved from their owners.
    // ---------------------------------------------------------------------

    fn test_sensor(pixel_dead_time_us: Option<f32>) -> SensorMonitoringV1 {
        SensorMonitoringV1 {
            pixel_dead_time_us,
            illumination_lux: Some(0.1),
            temperature_c: Some(25.0),
            bias_codes: None,
            age_s: 0.1,
        }
    }

    fn parsed(text: &str) -> Protocol {
        protocol::parse(text).unwrap()
    }

    fn calibrated_lobe(calibration_id: &str) -> stage_a_plugin_contract::OpticalLobeStateV1 {
        stage_a_plugin_contract::OpticalLobeStateV1 {
            calibration_id: calibration_id.into(),
            v_null_dac: 100,
            v_peak_dac: 1_000,
        }
    }

    fn modulation_state(
        lobe: Option<stage_a_plugin_contract::OpticalLobeStateV1>,
    ) -> ModulationStateV1 {
        let mut plugin = ready_plugin("resolve-helper");
        let mut state = plugin.modulation.take().unwrap();
        state.optical_lobe = lobe;
        state.optical_drive = None;
        state
    }

    #[test]
    fn an_applied_lobe_is_required_but_an_armed_measurement_point_is_not() {
        let controller = ControllerSetup::default();
        let state = modulation_state(None);
        let error = resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0))))
            .unwrap_err();
        assert!(error.contains("Apply to V_null / V_peak"), "{error}");

        let state = modulation_state(Some(calibrated_lobe("lobe-1")));
        assert!(state.optical_drive.is_none());
        resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0)))).unwrap();

        let error =
            resolve_controller(&controller, None, Some(test_sensor(Some(10.0)))).unwrap_err();
        assert!(error.contains("modulation owner"), "{error}");
    }

    #[test]
    fn a_degenerate_lobe_refuses_against_the_resolved_values() {
        let controller = ControllerSetup::default();
        let mut lobe = calibrated_lobe("lobe-1");
        lobe.v_peak_dac = lobe.v_null_dac;
        let state = modulation_state(Some(lobe));
        let error = resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0))))
            .unwrap_err();
        assert!(error.contains("v_peak_dac"), "{error}");
    }

    #[test]
    fn the_resolved_lobe_and_its_calibration_id_reach_the_run_provenance() {
        let controller = ControllerSetup::default();
        let state = modulation_state(Some(calibrated_lobe("pockels-2026-08-01")));
        let resolved =
            resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0)))).unwrap();
        assert_eq!(resolved.v_null_dac, 100);
        assert_eq!(resolved.v_peak_dac, 1_000);
        assert_eq!(resolved.lobe_source, "modulation_owner.optical_lobe");
        assert_eq!(resolved.modulation_calibration_id, "pockels-2026-08-01");
    }

    #[test]
    fn an_absent_min_half_us_resolves_to_the_larger_of_refractory_and_settling_floors() {
        let controller = ControllerSetup::default();
        let state = modulation_state(Some(calibrated_lobe("lobe-1")));

        // 5 x 10 us = 50 us, so the settling guard dominates.
        let resolved =
            resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0)))).unwrap();
        assert_eq!(resolved.refractory_floor_us, 50);
        assert_eq!(resolved.min_half_us, SETTLING_GUARD_US);
        assert_eq!(resolved.min_half_us_source, "sensor_telemetry");

        // 5 x 400 us = 2000 us, so the sensor dominates.
        let resolved =
            resolve_controller(&controller, Some(&state), Some(test_sensor(Some(400.0)))).unwrap();
        assert_eq!(resolved.refractory_floor_us, 2_000);
        assert_eq!(resolved.min_half_us, 2_000);
        assert_eq!(resolved.min_half_us_source, "sensor_telemetry");
    }

    #[test]
    fn a_frozen_min_half_us_is_kept_and_still_checked_against_the_sensor() {
        let controller = ControllerSetup {
            min_half_us: Some(1_000),
            ..ControllerSetup::default()
        };
        let state = modulation_state(Some(calibrated_lobe("lobe-1")));
        let resolved =
            resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0)))).unwrap();
        assert_eq!(resolved.min_half_us, 1_000);
        assert_eq!(resolved.min_half_us_source, "runner_configuration");

        // The pre-existing runtime gate: a frozen floor under 5 x dead time is
        // still refused, and refused before anything moves.
        let error = resolve_controller(&controller, Some(&state), Some(test_sensor(Some(300.0))))
            .unwrap_err();
        assert!(error.contains("5 x sensor dead time"), "{error}");
    }

    #[test]
    fn a_missing_pixel_dead_time_refuses_rather_than_resolving_a_floor_from_nothing() {
        let controller = ControllerSetup::default();
        let state = modulation_state(Some(calibrated_lobe("lobe-1")));
        let error =
            resolve_controller(&controller, Some(&state), Some(test_sensor(None))).unwrap_err();
        assert!(error.contains("pixel-dead-time"), "{error}");
        let error = resolve_controller(&controller, Some(&state), None).unwrap_err();
        assert!(error.contains("pixel-dead-time"), "{error}");
    }

    #[test]
    fn every_stepped_half_period_is_checked_against_the_resolved_floor() {
        let plan = parsed(test_protocol());
        // The protocol's stepped point has a 1000 us half period.
        assert!(check_half_periods(&plan, 1_000).is_ok());
        let error = check_half_periods(&plan, 1_001).unwrap_err();
        assert!(error.contains("resolved min_half_us"), "{error}");
    }

    // ---------------------------------------------------------------------
    // Stage 2 — V_50 measured per pedestal.
    // ---------------------------------------------------------------------

    /// The firmware's own inversion, written out independently of the plugin's
    /// helper so a wiring mistake in the plugin cannot hide behind it.
    fn firmware_dac_for_u(u: f64, v_null: f64, v_pi: f64) -> u16 {
        (v_null + (2.0 * v_pi / std::f64::consts::PI) * u.sqrt().asin()).round() as u16
    }

    #[test]
    fn plateau_codes_match_the_firmware_log_square_inversion() {
        let plateaus = resolve_plateaus(300, 450, 100, 1_000).unwrap();
        let expected_low = firmware_dac_for_u(0.3 * (-0.225_f64).exp(), 100.0, 900.0);
        let expected_high = firmware_dac_for_u(0.3 * (0.225_f64).exp(), 100.0, 900.0);
        assert_eq!(plateaus.low_dac, expected_low);
        assert_eq!(plateaus.high_dac, expected_high);
        assert!(plateaus.low_dac < plateaus.high_dac);
        assert_eq!(plateaus.mean_u_milli, 300);
        assert_eq!(plateaus.depth_a_milli, 450);
    }

    #[test]
    fn a_pedestal_that_would_saturate_the_lobe_refuses_at_preflight() {
        // 0.9 * e^(0.4) = 1.34, past the lobe maximum.
        let error = resolve_plateaus(900, 800, 100, 1_000).unwrap_err();
        assert!(error.contains("past the lobe maximum"), "{error}");
    }

    #[test]
    fn a_pedestal_whose_step_quantises_away_refuses_at_preflight() {
        // A one-milli depth on a narrow lobe collapses both plateaus onto one
        // code: there is no step for a threshold to sit inside.
        let error = resolve_plateaus(300, 1, 100, 110).unwrap_err();
        assert!(error.contains("single DAC code"), "{error}");
    }

    #[test]
    fn plan_pedestals_validates_frozen_points_too() {
        let plan = parsed(auto_protocol());
        let state = modulation_state(Some(calibrated_lobe("lobe-1")));
        let resolved = resolve_controller(
            &ControllerSetup::default(),
            Some(&state),
            Some(test_sensor(Some(10.0))),
        )
        .unwrap();
        let pedestals = plan_pedestals(&plan, &resolved).unwrap();
        // Two auto points share (300, 450); the third is frozen.
        assert_eq!(pedestals.len(), 1);
        assert!(pedestals.contains_key(&(300, 450)));

        let frozen_only = parsed(test_protocol());
        assert_eq!(plan_pedestals(&frozen_only, &resolved).unwrap().len(), 1);
    }

    #[test]
    fn an_adc_level_is_never_copied_across_as_a_threshold_code() {
        // ADC code 2048 on a 3300 mV reference is 1.6505 V; the threshold DAC
        // spans 2500 mV, so the same voltage is a very different code.
        let volts = 2_048.0 * 3.3 / 4_095.0;
        let code = threshold_code_for_volts(volts).unwrap();
        assert_ne!(code, 2_048);
        let expected = (volts * 1000.0 * 4_095.0 / 2_500.0).round() as u16;
        assert_eq!(code, expected);
        assert!(
            code > 2_048,
            "the DAC's smaller span must give a larger code"
        );
    }

    #[test]
    fn a_midpoint_the_threshold_dac_cannot_reach_refuses() {
        assert!(threshold_code_for_volts(3.0).unwrap_err().contains("2500"));
        assert!(threshold_code_for_volts(0.0).is_err());
        assert!(threshold_code_for_volts(f64::NAN).is_err());
    }

    fn level(
        mean_volts: f64,
        peak_to_peak_volts: f64,
        end_sample_index: u64,
        sample_count: u64,
        clipped: bool,
    ) -> PhotodiodeLevelV1 {
        PhotodiodeLevelV1 {
            mean_volts,
            peak_to_peak_volts,
            sample_count,
            end_sample_index,
            clipped,
        }
    }

    fn probe(low: PhotodiodeLevelV1, high: PhotodiodeLevelV1) -> PlateauProbe {
        PlateauProbe {
            mean_u_milli: 300,
            depth_a_milli: 450,
            low: PlateauStep {
                level_dac: 393,
                acknowledged_sample_index: 1_000,
                stream_epoch: 1,
                level: Some(low),
                windows: [Some(low); PLATEAU_WINDOWS],
                window_count: PLATEAU_WINDOWS,
            },
            high: PlateauStep {
                level_dac: 478,
                acknowledged_sample_index: 2_000,
                stream_epoch: 1,
                level: Some(high),
                windows: [Some(high); PLATEAU_WINDOWS],
                window_count: PLATEAU_WINDOWS,
            },
        }
    }

    #[test]
    fn the_threshold_is_the_plateau_midpoint_converted_through_millivolts() {
        let low = level(1.0, 0.001, 1_500, 400, false);
        let high = level(1.2, 0.001, 2_500, 400, false);
        let measured = measured_threshold(&probe(low, high), low, high).unwrap();
        assert!((measured.span_volts - 0.2).abs() < 1e-9);
        assert!((measured.midpoint_volts - 1.1).abs() < 1e-9);
        // 1100 mV * 4095 / 2500 = 1801.8
        assert_eq!(measured.threshold_dac, 1_802);
        assert_eq!(measured.low_window_end_sample_index, 1_500);
        assert_eq!(measured.low_drive_acknowledged_sample_index, 1_000);
        assert_eq!(measured.high_window_end_sample_index, 2_500);
        assert_eq!(measured.high_drive_acknowledged_sample_index, 2_000);
    }

    #[test]
    fn a_span_too_small_to_hold_a_threshold_refuses_rather_than_centring_in_noise() {
        let low = level(1.000, 0.0005, 1_500, 400, false);
        let high = level(1.001, 0.0005, 2_500, 400, false);
        let error = measured_threshold(&probe(low, high), low, high).unwrap_err();
        assert!(error.contains("threshold-DAC resolution floor"), "{error}");
    }

    #[test]
    fn an_inverted_plateau_pair_refuses() {
        let low = level(1.2, 0.001, 1_500, 400, false);
        let high = level(1.0, 0.001, 2_500, 400, false);
        assert!(measured_threshold(&probe(low, high), low, high).is_err());
    }

    #[test]
    fn stable_means_with_large_raw_noise_remain_recordable_but_require_review() {
        let low = level(0.380, 0.380, 1_500, 400, false);
        let high = level(0.400, 0.380, 2_500, 400, false);
        let result = measured_threshold(&probe(low, high), low, high).unwrap();
        assert!((result.midpoint_volts - 0.390).abs() < 1e-10);
        assert!(result.noisy_crossing_requires_review);
        assert_eq!(result.low_windows.len(), PLATEAU_WINDOWS);
    }

    #[test]
    fn drifting_window_means_refuse_even_when_raw_samples_do_not_clip() {
        let low = level(0.38, 0.380, 1_500, 400, false);
        let high = level(0.40, 0.380, 2_500, 400, false);
        let mut p = probe(low, high);
        p.low.windows[3].as_mut().unwrap().mean_volts += 0.015;
        let error = measured_threshold(&p, low, high).unwrap_err();
        assert!(error.contains("V50 unresolved"), "{error}");
    }

    fn auto_protocol() -> &'static str {
        r#"
name="a2-auto"
[[point]]
label="auto_a"
role="identification"
acquisition_mode="stepped"
mean_u=0.3
depth_a=0.45
half_period_s=0.001
transitions_per_polarity=2
settle_s=0
[[point]]
label="auto_b"
role="identification"
acquisition_mode="stepped"
mean_u=0.3
depth_a=0.45
half_period_s=0.001
transitions_per_polarity=2
settle_s=0
[[point]]
label="frozen_c"
role="identification"
acquisition_mode="stepped"
mean_u=0.3
depth_a=0.45
half_period_s=0.001
transitions_per_polarity=2
settle_s=0
comparator_threshold_dac=500
"#
    }

    fn ready_auto_plugin(label: &str) -> StageAA2Plugin {
        let plugin = ready_plugin(label);
        std::fs::write(&plugin.protocol_path, auto_protocol()).unwrap();
        plugin
    }

    fn publish_stream(
        plugin: &mut StageAA2Plugin,
        stream_epoch: u64,
        end_sample_index: u64,
        published: Option<PhotodiodeLevelV1>,
    ) {
        let pd = plugin.photodiode.as_mut().unwrap();
        pd.stream.stream_epoch = stream_epoch;
        pd.stream.sample_range = Some(stage_a_plugin_contract::SampleRangeV1 {
            first_sample_index: 0,
            end_sample_index_exclusive: end_sample_index,
            sample_count: end_sample_index,
        });
        pd.stream.level = published;
    }

    fn publish_plateau_windows(
        plugin: &mut StageAA2Plugin,
        control: &mut MockControl,
        mean: f64,
        raw_spread: f64,
    ) {
        let run = plugin.run.as_ref().unwrap();
        let probe = run.probe.unwrap();
        let step = if run.phase == Phase::PlateauLowLevel {
            probe.low
        } else {
            probe.high
        };
        let mut end = step.acknowledged_sample_index + 125_000;
        for _ in 0..PLATEAU_WINDOWS {
            end += 10_000;
            publish_stream(
                plugin,
                step.stream_epoch,
                end,
                Some(level(mean, raw_spread, end, 10_000, false)),
            );
            plugin.run.as_mut().unwrap().deadline_ms = 0;
            plugin.drive(control);
        }
    }

    /// Walks a fresh plugin as far as "the dim plateau is commanded and
    /// acknowledged", which is where every Stage-2 refusal path branches.
    fn plugin_at_low_plateau(label: &str) -> (StageAA2Plugin, MockControl) {
        let mut plugin = ready_auto_plugin(label);
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let apply = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, apply, applied_camera_outcome());
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        publish_stream(&mut plugin, 1, 1_000, None);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(
            plugin.run.as_ref().unwrap().phase,
            Phase::PlateauLowDrive,
            "an auto point must measure before it prepares"
        );
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::PlateauLowLevel);
        (plugin, control)
    }

    #[test]
    fn an_auto_point_holds_both_plateaus_before_it_prepares() {
        let plateaus = resolve_plateaus(300, 450, 100, 1_000).unwrap();
        let mut plugin = ready_auto_plugin("auto-v50");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let apply = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, apply, applied_camera_outcome());
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        publish_stream(&mut plugin, 1, 1_000, None);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);

        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert_eq!(
            request.command,
            ModulationCommandV1::SetWaveform {
                waveform: WaveformV1::Constant {
                    level_dac: plateaus.low_dac
                }
            }
        );

        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        publish_plateau_windows(&mut plugin, &mut control, 1.0, 0.001);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::PlateauHighDrive);
        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert_eq!(
            request.command,
            ModulationCommandV1::SetWaveform {
                waveform: WaveformV1::Constant {
                    level_dac: plateaus.high_dac
                }
            }
        );

        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        let high_ack = plugin
            .run
            .as_ref()
            .unwrap()
            .probe
            .unwrap()
            .high
            .acknowledged_sample_index;
        publish_plateau_windows(&mut plugin, &mut control, 1.2, 0.001);

        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Prepare);
        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let ModulationCommandV1::PrepareA2 { configuration } = request.command else {
            panic!("expected PrepareA2 after the plateau measurement");
        };
        assert_eq!(configuration.comparator_threshold_dac, 1_802);
        assert_eq!(configuration.v_null_dac, 100);
        assert_eq!(configuration.v_peak_dac, 1_000);
        assert_eq!(configuration.min_half_us, 1_000);

        let evidence = &plugin.run.as_ref().unwrap().evidence;
        assert_eq!(evidence.comparator_threshold_mode, Some("auto"));
        assert_eq!(evidence.comparator_threshold_dac, Some(1_802));
        let measurement = evidence.threshold_measurement.as_ref().unwrap();
        assert_eq!(measurement.low_level_dac, plateaus.low_dac);
        assert_eq!(measurement.high_level_dac, plateaus.high_dac);
        assert_eq!(measurement.low_drive_acknowledged_sample_index, 1_000);
        assert_eq!(measurement.high_drive_acknowledged_sample_index, high_ack);

        // Repeated coordinates are remeasured: flux or baseline may have drifted.
        {
            let run = plugin.run.as_mut().unwrap();
            run.pending = None;
            run.index = 1;
        }
        plugin.prepare(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::PlateauLowDrive);

        // A frozen point keeps its own code and is recorded as a claim.
        {
            let run = plugin.run.as_mut().unwrap();
            run.pending = None;
            run.index = 2;
        }
        plugin.prepare(&mut control);
        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let ModulationCommandV1::PrepareA2 { configuration } = request.command else {
            panic!("expected PrepareA2 for the frozen point");
        };
        assert_eq!(configuration.comparator_threshold_dac, 500);
        assert_eq!(
            plugin
                .run
                .as_ref()
                .unwrap()
                .evidence
                .comparator_threshold_mode,
            Some("frozen")
        );
    }

    #[test]
    fn a_clipped_plateau_refuses_the_point() {
        let (mut plugin, mut control) = plugin_at_low_plateau("auto-clipped");
        publish_stream(
            &mut plugin,
            1,
            136_000,
            Some(level(3.29, 0.001, 136_000, 10_000, true)),
        );
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        assert!(plugin.message.contains("clips"), "{}", plugin.message);
        assert!(plugin.run.as_ref().unwrap().stop);
    }

    #[test]
    fn a_level_window_that_started_before_the_drive_is_never_accepted() {
        let (mut plugin, mut control) = plugin_at_low_plateau("auto-stale-window");
        // Ends at 1200 over 400 samples, so it starts at 800 — before the drive
        // was acknowledged at 1000.
        publish_stream(
            &mut plugin,
            1,
            1_200,
            Some(level(1.0, 0.001, 1_200, 400, false)),
        );
        {
            let run = plugin.run.as_mut().unwrap();
            run.deadline_ms = 0;
            run.plateau_timeout_ms = 0;
        }
        plugin.drive(&mut control);
        assert!(
            plugin.message.contains("began after the acknowledged"),
            "{}",
            plugin.message
        );
    }

    #[test]
    fn a_restarted_photodiode_stream_invalidates_the_plateau_measurement() {
        let (mut plugin, mut control) = plugin_at_low_plateau("auto-epoch");
        publish_stream(
            &mut plugin,
            2,
            9_000,
            Some(level(1.0, 0.001, 9_000, 400, false)),
        );
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        assert!(
            plugin.message.contains("stream restarted"),
            "{}",
            plugin.message
        );
    }

    #[test]
    fn a_plateau_drive_without_a_published_sample_range_refuses() {
        let mut plugin = ready_auto_plugin("auto-no-range");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let apply = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, apply, applied_camera_outcome());
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::PlateauLowDrive);
        // The owner has published no sample range, so nothing can anchor the
        // level window to the drive.
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert!(
            plugin.message.contains("no sample range"),
            "{}",
            plugin.message
        );
    }

    #[test]
    fn an_unresolvable_lobe_refuses_before_the_camera_is_touched() {
        let mut plugin = ready_auto_plugin("auto-no-lobe");
        plugin.modulation.as_mut().unwrap().optical_lobe = None;
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        assert!(plugin.run.is_none());
        assert!(control.hosts.is_empty(), "no camera command may be issued");
        assert!(control.services.is_empty(), "no owner lease may be taken");
        assert!(plugin.message.contains("refused"), "{}", plugin.message);
    }
    #[test]
    fn repeated_or_overlapping_pd_windows_do_not_create_false_precision() {
        let (mut plugin, mut control) = plugin_at_low_plateau("overlap");
        publish_stream(
            &mut plugin,
            1,
            136_000,
            Some(level(0.38, 0.380, 136_000, 10_000, false)),
        );
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        for _ in 0..20 {
            plugin.drive(&mut control);
        }
        assert_eq!(
            plugin.run.as_ref().unwrap().probe.unwrap().low.window_count,
            1
        );
        publish_stream(
            &mut plugin,
            1,
            140_000,
            Some(level(0.38, 0.380, 140_000, 10_000, false)),
        );
        plugin.drive(&mut control);
        assert_eq!(
            plugin.run.as_ref().unwrap().probe.unwrap().low.window_count,
            1
        );
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::PlateauLowLevel);
    }

    #[test]
    fn a_window_sampled_before_settling_cannot_be_accepted_late() {
        let (mut plugin, mut control) = plugin_at_low_plateau("settle-samples");
        publish_stream(
            &mut plugin,
            1,
            21_000,
            Some(level(0.38, 0.001, 21_000, 10_000, false)),
        );
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        assert_eq!(
            plugin.run.as_ref().unwrap().probe.unwrap().low.window_count,
            0
        );
    }

    #[test]
    fn lease_renewal_does_not_advance_the_plateau_state_machine() {
        let (mut plugin, mut control) = plugin_at_low_plateau("renewal-phase");
        plugin.run.as_mut().unwrap().next_renew_ms = 0;
        plugin.drive(&mut control);
        assert_eq!(
            plugin.run.as_ref().unwrap().pending.unwrap().0,
            PendingKind::RenewMod
        );
        plugin.accepted(&mut control, PendingKind::RenewMod, &Value::Null);
        assert_eq!(
            plugin.run.as_ref().unwrap().pending.unwrap().0,
            PendingKind::RenewPd
        );
        plugin.accepted(&mut control, PendingKind::RenewPd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::PlateauLowLevel);
        assert!(plugin.run.as_ref().unwrap().next_renew_ms > now_ms());
    }

    #[test]
    fn aborting_a_constant_probe_turns_off_the_drive_without_finalizing_an_unopened_pdq() {
        let (mut plugin, mut control) = plugin_at_low_plateau("stop-plateau");
        plugin.fail(&mut control, "test reason".into());
        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert!(matches!(
            request.command,
            ModulationCommandV1::SetWaveform {
                waveform: WaveformV1::Off
            }
        ));
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::ReleasePd);
        assert!(!control.services.iter().any(|r| {
            serde_json::from_value::<PhotodiodeRequestV1>(r.payload.clone())
                .is_ok_and(|r| matches!(r.command, PhotodiodeCommandV1::FinalizeRecording { .. }))
        }));
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("test reason"));
    }

    fn plugin_at_finalization(
        label: &str,
        policy: TriggerValidation,
    ) -> (StageAA2Plugin, MockControl) {
        let mut plugin = ready_plugin(label);
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let run = plugin.run.as_mut().unwrap();
        run.index = 1;
        run.run_id = format!("{}_r002_step", run.measurement_id);
        run.protocol.trigger_validation = policy;
        run.phase = Phase::StopCamera;
        run.pending = None;
        run.camera_recording = true;
        run.mod_leased = true;
        run.pd_leased = true;
        run.evidence.pdq_marker_counts = Some(stage_a_plugin_contract::PdqMarkerCountsV1 {
            comparator_rising: 2,
            comparator_falling: 2,
            phase_zero: 2,
            invalid_level: 0,
        });
        (plugin, control)
    }

    fn raw_finalized() -> HostCommandOutcome {
        HostCommandOutcome::RecordingFinalized {
            actual_raw_path: "/tmp/a2-test.raw".into(),
            size: 100,
            sha256: "12".repeat(32),
            duration_us: 4000,
        }
    }

    #[test]
    fn a_bad_strict_point_never_advances_or_reports_success() {
        let (mut plugin, mut control) =
            plugin_at_finalization("strict-failure", TriggerValidation::Strict);
        plugin.finish_point(&mut control, &raw_finalized());
        assert_eq!(plugin.run.as_ref().unwrap().index, 1);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::ReleasePd);
        assert!(!plugin.run.as_ref().unwrap().evidence.valid);
        assert!(plugin.run.as_ref().unwrap().abort_reason.is_some());
        plugin.finish_run();
        assert!(plugin.message.contains("A2 stopped"), "{}", plugin.message);
    }

    #[test]
    fn offline_capture_keeps_noisy_trigger_data_and_marks_it_for_review() {
        let (mut plugin, mut control) =
            plugin_at_finalization("offline-noise", TriggerValidation::OfflineReview);
        plugin.finish_point(&mut control, &raw_finalized());
        assert_eq!(plugin.run.as_ref().unwrap().review_points, 1);
        assert!(plugin.run.as_ref().unwrap().abort_reason.is_none());
        plugin.finish_run();
        assert!(
            plugin.message.contains("require offline timing review"),
            "{}",
            plugin.message
        );
        assert!(!plugin.message.contains("checks passed"));
    }

    #[test]
    fn file_corruption_stops_even_an_offline_capture() {
        let (mut plugin, mut control) =
            plugin_at_finalization("offline-corrupt", TriggerValidation::OfflineReview);
        plugin.run.as_mut().unwrap().evidence.failure = Some("PDQ sample gap".into());
        plugin.finish_point(&mut control, &raw_finalized());
        assert_eq!(plugin.run.as_ref().unwrap().index, 1);
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("PDQ sample gap"));
    }

    #[test]
    fn sidecar_write_failure_is_not_silently_ignored() {
        let (mut plugin, mut control) =
            plugin_at_finalization("sidecar-io", TriggerValidation::OfflineReview);
        let blocked = Path::new(&plugin.output_folder).join("not-a-directory");
        std::fs::write(&blocked, b"blocked").unwrap();
        plugin.run.as_mut().unwrap().output_root = blocked;
        plugin.finish_point(&mut control, &raw_finalized());
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("cannot save A2 sidecar"));
    }

    #[test]
    fn a_stop_timeout_does_not_repeat_the_stop_forever() {
        let (mut plugin, mut control) = plugin_at_low_plateau("stop-timeout");
        plugin.fail(&mut control, "original plateau failure".into());
        plugin.run.as_mut().unwrap().pending.as_mut().unwrap().2 = 0;
        plugin.drive(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::ReleasePd);
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("original plateau failure"));
        assert!(!plugin.run.as_ref().unwrap().cleanup_failures.is_empty());
    }

    #[test]
    fn drive_sync_has_no_pd_amplitude_or_dead_time_precondition() {
        let mut plugin = ready_auto_plugin("drive-sync");
        let text = format!(
            "timing_reference=\"drive_sync\"\ntrigger_validation=\"offline_review\"\n{}",
            auto_protocol()
        );
        std::fs::write(&plugin.protocol_path, text).unwrap();
        plugin.sensor = None;
        plugin.photodiode.as_mut().unwrap().stream.level = None;
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let apply = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, apply, applied_camera_outcome());
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Prepare);
        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let ModulationCommandV1::PrepareA2 { configuration } = request.command else {
            panic!("expected PrepareA2")
        };
        assert_eq!(
            configuration.timing_reference,
            A2TimingReferenceV1::DriveSync
        );
        assert_eq!(configuration.comparator_threshold_dac, 0);
        assert_eq!(configuration.min_half_us, 0);
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .evidence
            .threshold_measurement
            .is_none());
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .resolved
            .pixel_dead_time_us
            .is_none());
    }

    #[test]
    fn voltage_above_dac_rail_is_not_rounded_back_into_range() {
        assert!(threshold_code_for_volts(2.5001).is_err());
    }

    #[test]
    fn blocked_drive_sham_instructions_do_not_open_the_optical_path() {
        let mut p = paused_point();
        p.role = "blocked_drive_sham".into();
        let message = pause_message(&p);
        assert!(message.contains("keep the optical path blocked"));
        assert!(!message.contains("open the optical path"));
    }

    #[test]
    fn stopped_pd_stream_aborts_instead_of_saving_a_short_valid_file() {
        let (mut plugin, mut control) = plugin_at_low_plateau("stalled-pd");
        let index = plugin
            .photodiode
            .as_ref()
            .unwrap()
            .stream
            .sample_range
            .unwrap()
            .end_sample_index_exclusive;
        let run = plugin.run.as_mut().unwrap();
        run.phase = Phase::Recording;
        run.pd_recording = true;
        run.pd_progress = Some((index, now_ms() - 6000));
        plugin.drive(&mut control);
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("stopped advancing"));
    }

    #[test]
    fn exhausted_camera_stop_timeout_cannot_report_success() {
        let (mut plugin, mut control) = plugin_at_low_plateau("camera-stop-timeout");
        let run = plugin.run.as_mut().unwrap();
        run.phase = Phase::StopCamera;
        run.camera_stop_attempts = 3;
        run.pending = Some((PendingKind::Host, 99, now_ms() - TIMEOUT_MS - 1));
        run.camera_recording = true;
        plugin.drive(&mut control);
        let run = plugin.run.as_ref().unwrap();
        assert!(run.stop);
        assert!(run
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("camera stop timed out"));
    }
    #[test]
    fn drive_sync_starts_its_identifiable_pulse_train_after_both_recorders_open() {
        let (mut plugin, mut control) = plugin_at_low_plateau("sync-onset");
        let run = plugin.run.as_mut().unwrap();
        run.protocol.timing_reference = A2TimingReferenceV1::DriveSync;
        run.phase = Phase::Prepare;
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(
            plugin.run.as_ref().unwrap().phase,
            Phase::QuietBeforeCapture
        );
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Settle);
        assert!(!plugin.run.as_ref().unwrap().modulation_active);
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::StartCamera);
        plugin.accepted(&mut control, PendingKind::Host, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::StartPd);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        let run = plugin.run.as_ref().unwrap();
        assert!(run.pd_recording && run.camera_recording);
        assert_eq!(run.phase, Phase::StartStimulus);
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Recording);
    }
    #[test]
    fn metadata_is_saved_before_the_camera_is_started() {
        let (mut plugin, mut control) =
            plugin_at_finalization("initial-metadata", TriggerValidation::OfflineReview);
        let run = plugin.run.as_mut().unwrap();
        run.phase = Phase::Settle;
        run.pending = None;
        run.deadline_ms = 0;
        run.next_renew_ms = u64::MAX;
        let path = Path::new(&plugin.output_folder)
            .join(&run.measurement_id)
            .join(format!("{}.a2.json", run.run_id));
        plugin.drive(&mut control);
        assert!(
            path.is_file(),
            "point evidence must exist before acquisition"
        );
    }
    fn waiting_camera(label: &str) -> (StageAA2Plugin, MockControl) {
        let (mut plugin, mut control) =
            plugin_at_finalization(label, TriggerValidation::OfflineReview);
        let run = plugin.run.as_mut().unwrap();
        run.phase = Phase::Settle;
        run.pending = None;
        run.deadline_ms = 0;
        run.next_renew_ms = u64::MAX;
        plugin.drive(&mut control);
        (plugin, control)
    }

    #[test]
    fn camera_and_pd_use_the_same_frozen_root_even_if_the_owner_folder_changes() {
        let (mut plugin, mut control) = waiting_camera("frozen-root");
        let HostCommand::StartRecording {
            root_dir: Some(root),
            base_path,
            ..
        } = control.hosts.last().unwrap().command.clone()
        else {
            panic!("missing explicit root")
        };
        assert!(Path::new(&root).is_absolute());
        let raw = Path::new(&root).join(&base_path);
        plugin.photodiode.as_mut().unwrap().data_dir = Some("/different/owner/folder".into());
        let id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            id,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: raw.to_string_lossy().into_owned(),
                started_at: "now".into(),
            },
        );
        let request: PhotodiodeRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let PhotodiodeCommandV1::BeginRecording { specification } = request.command else {
            panic!("not a PD start")
        };
        assert_eq!(specification.root_dir.as_deref(), Some(root.as_str()));
        assert_eq!(
            Path::new(&root).join(&specification.pdq_path).parent(),
            raw.parent()
        );
        let sidecar = raw.with_extension("a2.json");
        let value: Value = serde_json::from_slice(&std::fs::read(sidecar).unwrap()).unwrap();
        assert_eq!(
            value["evidence"]["raw_path"],
            raw.to_string_lossy().as_ref()
        );
        assert_eq!(value["evidence"]["acquisition_complete"], false);
    }

    #[test]
    fn photodiode_paths_reported_relative_to_the_root_are_anchored_to_it() {
        let (mut plugin, mut control) = waiting_camera("relative-pd-paths");
        let HostCommand::StartRecording {
            root_dir: Some(root),
            base_path,
            ..
        } = control.hosts.last().unwrap().command.clone()
        else {
            panic!("missing explicit root")
        };
        let raw = Path::new(&root).join(&base_path);
        let id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            id,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: raw.to_string_lossy().into_owned(),
                started_at: "now".into(),
            },
        );
        let request: PhotodiodeRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let PhotodiodeCommandV1::BeginRecording { specification } = request.command else {
            panic!("not a PD start")
        };
        // The owner echoes the requested paths, which are relative to the root.
        let pd_id = plugin.run.as_ref().unwrap().pending.unwrap().1;
        let run_id = plugin.run.as_ref().unwrap().run_id.clone();
        plugin.accepted(
            &mut control,
            PendingKind::Pd,
            &started_pd_payload(
                pd_id,
                &run_id,
                &specification.pdq_path,
                &specification.sidecar_path,
            ),
        );
        let run = plugin.run.as_ref().unwrap();
        assert!(!run.stop, "{:?}", run.abort_reason);
        assert_eq!(run.abort_reason, None);
        let measurement_dir = run
            .output_root
            .join(&run.measurement_id)
            .canonicalize()
            .unwrap();
        for (recorded, requested) in [
            (&run.evidence.pdq_path, &specification.pdq_path),
            (&run.evidence.pd_sidecar_path, &specification.sidecar_path),
        ] {
            let recorded = Path::new(recorded.as_deref().expect("path recorded"));
            assert!(recorded.is_absolute(), "{}", recorded.display());
            assert_eq!(
                recorded.parent().unwrap().canonicalize().unwrap(),
                measurement_dir
            );
            assert_eq!(
                recorded.file_name(),
                Path::new(requested).file_name(),
                "{}",
                recorded.display()
            );
        }
    }

    #[test]
    fn an_old_host_ignoring_the_root_is_stopped_before_pd_start() {
        let (mut plugin, mut control) = waiting_camera("wrong-host-root");
        let id = control.hosts.last().unwrap().request_id;
        let wrong = test_folder("wrong-raw-location");
        std::fs::create_dir_all(&wrong).unwrap();
        plugin.host_reply(
            &mut control,
            id,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: wrong.join("recording.raw").to_string_lossy().into_owned(),
                started_at: "now".into(),
            },
        );
        assert!(plugin.run.as_ref().unwrap().stop);
        assert!(plugin
            .run
            .as_ref()
            .unwrap()
            .abort_reason
            .as_ref()
            .unwrap()
            .contains("outside the measurement folder"));
        assert!(matches!(
            control.hosts.last().unwrap().command,
            HostCommand::StopRecording
        ));
        assert!(!control.services.iter().any(|r| {
            serde_json::from_value::<PhotodiodeRequestV1>(r.payload.clone())
                .is_ok_and(|r| matches!(r.command, PhotodiodeCommandV1::BeginRecording { .. }))
        }));
    }

    #[test]
    fn rejected_camera_start_does_not_stop_an_unrelated_recording() {
        let (mut plugin, mut control) = waiting_camera("camera-busy");
        let id = control.hosts.last().unwrap().request_id;
        let host_count = control.hosts.len();
        plugin.host_reply(
            &mut control,
            id,
            HostCommandOutcome::Rejected {
                code: "recording_busy".into(),
                message: "another recording is active".into(),
            },
        );
        assert!(!plugin.run.as_ref().unwrap().camera_recording);
        assert!(!control.hosts[host_count..]
            .iter()
            .any(|r| matches!(r.command, HostCommand::StopRecording)));
    }

    #[test]
    fn measurement_id_is_preserved_and_repeated_runs_do_not_overwrite() {
        let mut plugin = ready_plugin("repeat-id");
        plugin
            .set_setting("measurement_id", json!("A2-Atto647-test"))
            .unwrap();
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        plugin.prepare(&mut control);
        let first = plugin.run.as_ref().unwrap().run_id.clone();
        plugin.write_sidecar().unwrap();
        let path = plugin
            .run
            .as_ref()
            .unwrap()
            .output_root
            .join(&plugin.measurement_id)
            .join(format!("{first}.a2.json"));
        plugin.finish_run();
        let before = std::fs::read(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        plugin.begin(&mut control);
        plugin.prepare(&mut control);
        plugin.write_sidecar().unwrap();
        assert_eq!(plugin.measurement_id, "A2-Atto647-test");
        assert_ne!(plugin.run.as_ref().unwrap().run_id, first);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn unsafe_or_reserved_measurement_ids_refuse_before_hardware() {
        for (i, id) in ["../escape", "a/b", "C:\\data", "CON", "LPT1", "nul"]
            .iter()
            .enumerate()
        {
            let mut plugin = ready_plugin(&format!("id-validation-{i}"));
            plugin.measurement_id = (*id).into();
            let mut control = MockControl::default();
            plugin.begin(&mut control);
            assert!(plugin.run.is_none(), "accepted {id}");
            assert!(control.hosts.is_empty() && control.services.is_empty());
        }
    }

    #[test]
    fn active_measurement_settings_are_frozen() {
        let (mut plugin, _) = waiting_camera("settings-frozen");
        for key in ["measurement_id", "output_folder", "protocol_path", "new_id"] {
            assert!(plugin.set_setting(key, json!("replacement")).is_err());
        }
        let id = plugin.run.as_ref().unwrap().run_id.clone();
        plugin.reset();
        assert_eq!(
            plugin.run.as_ref().unwrap().run_id,
            id,
            "host reset lost active acquisition"
        );
    }

    #[test]
    fn rest_time_counts_down_only_timed_phases_and_excludes_pauses() {
        let mut plugin = ready_plugin("eta");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let run = plugin.run.as_mut().unwrap();
        let expected: f64 = run.protocol.points.iter().map(point_seconds).sum();
        run.phase = Phase::Paused;
        assert_eq!(remaining_seconds(run, 0), expected);
        assert_eq!(remaining_seconds(run, 1_000_000), expected);
        run.phase = Phase::Recording;
        run.deadline_ms = 500;
        let later = point_seconds(&run.protocol.points[1]);
        assert!((remaining_seconds(run, 100) - (0.4 + later)).abs() < 1e-9);
        assert!((remaining_seconds(run, 400) - (0.1 + later)).abs() < 1e-9);
        run.phase = Phase::RestoreCamera;
        assert_eq!(remaining_seconds(run, 0), 0.0);
    }

    #[test]
    fn loading_a_protocol_shows_its_duration_before_start() {
        let mut plugin = ready_plugin("preview-estimate");
        plugin
            .set_setting("protocol_path", json!(plugin.protocol_path))
            .unwrap();
        let status = format!("{:?}", plugin.status_entries());
        assert!(
            status.contains("2 points") && status.contains("manual pauses"),
            "{status}"
        );
        assert!(plugin.run.is_none());
    }

    #[test]
    fn pending_modulation_is_polled_with_unchanged_identity_and_timeout() {
        let mut plugin = ready_plugin("poll-command");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, id, applied_camera_outcome());
        let original = control.services.last().unwrap().clone();
        let pending = plugin.run.as_ref().unwrap().pending;
        plugin.run.as_mut().unwrap().last_mod_poll_ms = 0;
        plugin.finish_async_mod(&mut control);
        let poll = control.services.last().unwrap();
        assert_eq!(poll.request_id, original.request_id);
        assert_eq!(poll.payload, original.payload);
        assert_eq!(plugin.run.as_ref().unwrap().pending, pending);
        let count = control.services.len();
        plugin.finish_async_mod(&mut control);
        assert_eq!(control.services.len(), count, "poll must be throttled");
    }

    #[test]
    fn another_clients_snapshot_cannot_abort_a_pending_command() {
        use stage_a_plugin_contract::{
            ControllerStateV1, OwnerInstanceId, ResponseCommonV1, CONTRACT_VERSION_V1,
        };
        let mut plugin = ready_plugin("snapshot-collision");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let id = control.hosts.last().unwrap().request_id;
        plugin.host_reply(&mut control, id, applied_camera_outcome());
        let pending = plugin.run.as_ref().unwrap().pending;
        let response = ModulationResponseV1 {
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: RequestId(pending.unwrap().1),
                owner_instance: OwnerInstanceId::new(
                    plugin
                        .run
                        .as_ref()
                        .unwrap()
                        .resolved
                        .modulation_owner_instance
                        .clone(),
                ),
                run_id: Some(RunId::new("different-workflow")),
                requested_revision: None,
                acknowledged_revision: None,
                outcome: RequestOutcomeV1::Applied,
                completed_at_unix_ms: Some(now_ms()),
                error: None,
            },
            controller_state: ControllerStateV1::Running,
            acknowledged_target: None,
            marker_diagnostics: None,
        };
        plugin.modulation.as_mut().unwrap().last_response = Some(response);
        plugin.finish_async_mod(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().pending, pending);
        assert!(!plugin.run.as_ref().unwrap().stop);
        let response = plugin
            .modulation
            .as_mut()
            .unwrap()
            .last_response
            .as_mut()
            .unwrap();
        response.common.run_id = Some(RunId::new(
            plugin.run.as_ref().unwrap().lease_run_id.clone(),
        ));
        plugin.finish_async_mod(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::AcquirePd);
    }

    #[test]
    fn completed_capture_and_progress_survive_cleanup() {
        let (mut plugin, mut control) =
            plugin_at_finalization("progress-capture", TriggerValidation::OfflineReview);
        plugin.finish_point(&mut control, &raw_finalized());
        let run = plugin.run.as_ref().unwrap();
        let sidecar = run
            .output_root
            .join(&run.measurement_id)
            .join(format!("{}.a2.json", run.run_id));
        let journal = run
            .output_root
            .join(&run.measurement_id)
            .join(format!("{}_progress.jsonl", run.attempt_id));
        assert_eq!(run.completed_points, 1);
        plugin.finish_run();
        let value: Value = serde_json::from_slice(&std::fs::read(sidecar).unwrap()).unwrap();
        assert_eq!(value["evidence"]["acquisition_complete"], true);
        assert_eq!(value["protocol_row"], 2);
        let events: Vec<Value> = std::fs::read_to_string(journal)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert!(events.iter().any(|v| v["event"] == "point_finished"));
        assert_eq!(events.last().unwrap()["event"], "run_finished");
        assert_eq!(events.last().unwrap()["completed_points"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_measurement_directory_refuses_before_hardware() {
        let mut plugin = ready_plugin("symlink-directory");
        let outside = test_folder("outside-directory");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            Path::new(&plugin.output_folder).join(&plugin.measurement_id),
        )
        .unwrap();
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        assert!(plugin.run.is_none());
        assert!(control.hosts.is_empty());
        assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    }
    fn save_completed_row(plugin: &mut StageAA2Plugin, index: usize) {
        let run = plugin.run.as_mut().unwrap();
        run.index = index;
        run.run_id = format!("{}_r{:03}_saved", run.measurement_id, index + 1);
        let dir = run.output_root.join(&run.measurement_id);
        let mut paths = Vec::new();
        for suffix in ["raw", "toml", "pdq", "pd.json"] {
            let name = format!("{}.{}", run.run_id, suffix);
            std::fs::write(dir.join(&name), b"recorded").unwrap();
            paths.push(format!(r"C:\old\{name}"));
        }
        run.evidence.raw_path = Some(paths[0].clone());
        run.evidence.camera_configuration_sidecar_path = Some(paths[1].clone());
        run.evidence.pdq_path = Some(paths[2].clone());
        run.evidence.pd_sidecar_path = Some(paths[3].clone());
        run.evidence.acquisition_complete = true;
        plugin.write_sidecar().unwrap();
    }

    #[test]
    fn resume_requires_same_protocol_and_complete_local_artifacts() {
        let mut plugin = ready_plugin("resume-evidence");
        plugin.begin(&mut MockControl::default());
        save_completed_row(&mut plugin, 1);
        let run = plugin.run.as_ref().unwrap();
        let dir = run.output_root.join(&run.measurement_id);
        let scan = || {
            crate::resume::completed(
                &dir,
                &run.measurement_id,
                &run.protocol_sha256,
                &run.protocol,
            )
            .unwrap()
        };
        assert_eq!(scan(), BTreeSet::from([1]));
        assert!(
            crate::resume::completed(&dir, &run.measurement_id, "changed", &run.protocol)
                .unwrap()
                .is_empty()
        );
        let pd = dir.join(format!("{}.pdq", run.run_id));
        std::fs::write(&pd, b"").unwrap();
        assert!(scan().is_empty());
        std::fs::remove_file(pd).unwrap();
        assert!(scan().is_empty());
    }

    #[test]
    fn resume_skips_gaps_without_renumbering_and_all_complete_needs_no_hardware() {
        let mut plugin = ready_plugin("resume-gaps");
        plugin.begin(&mut MockControl::default());
        save_completed_row(&mut plugin, 1);
        // Remove the current journal so this test can restart within the same millisecond.
        let run = plugin.run.take().unwrap();
        std::fs::remove_file(
            run.output_root
                .join(&run.measurement_id)
                .join(format!("{}_progress.jsonl", run.attempt_id)),
        )
        .unwrap();
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let run = plugin.run.as_ref().unwrap();
        assert_eq!(run.index, 0);
        assert_eq!(run.resumed_points, BTreeSet::from([1]));
        assert_eq!(
            remaining_seconds(run, 0),
            point_seconds(&run.protocol.points[0])
        );
        assert!(run.resume_pause);
        save_completed_row(&mut plugin, 0);
        let run = plugin.run.take().unwrap();
        std::fs::remove_file(
            run.output_root
                .join(&run.measurement_id)
                .join(format!("{}_progress.jsonl", run.attempt_id)),
        )
        .unwrap();
        plugin.modulation = None;
        plugin.sensor = None;
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        assert!(plugin.run.is_none(), "{}", plugin.message);
        assert!(
            plugin.message.contains("already complete"),
            "{}",
            plugin.message
        );
        assert!(control.hosts.is_empty() && control.services.is_empty());
    }

    #[test]
    fn stop_during_camera_start_does_not_start_photodiode() {
        let (mut plugin, mut control) = waiting_camera("stop-opening");
        plugin.stop_pending = true;
        let run = plugin.run.as_ref().unwrap();
        let path = run
            .output_root
            .join(&run.measurement_id)
            .join(format!("{}.raw", run.run_id));
        let id = control.hosts.last().unwrap().request_id;
        let services_before = control.services.len();
        plugin.host_reply(
            &mut control,
            id,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: path.to_string_lossy().into_owned(),
                started_at: "now".into(),
            },
        );
        assert_eq!(control.services.len(), services_before);
        assert!(matches!(
            control.hosts.last().unwrap().command,
            HostCommand::StopRecording
        ));
        assert!(!plugin.run.as_ref().unwrap().evidence.acquisition_complete);
    }
    #[test]
    fn advancing_skips_completed_rows_and_keeps_crossed_manual_pause() {
        let mut plugin = ready_plugin("resume-advance");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        let run = plugin.run.as_mut().unwrap();
        run.protocol.points.push(run.protocol.points[1].clone());
        run.protocol.points[1].pause_before = true;
        run.protocol.points[2].pause_before = false;
        run.resumed_points = BTreeSet::from([1]);
        run.index = 0;
        run.pending = None;
        plugin.advance(&mut control);
        let run = plugin.run.as_ref().unwrap();
        assert_eq!(run.index, 2);
        assert_eq!(run.completed_points, 1);
        assert_eq!(run.phase, Phase::Paused);
        assert!(run.run_id.contains("_r003_"));
    }
    #[test]
    fn ui_mirror_keeps_continue_and_stop_accessible_without_worker_run_state() {
        let mut mirror = StageAA2Plugin::default();
        mirror.set_runtime_role(PluginRuntimeRole::UiMirror);
        assert!(mirror.run.is_none());
        let schema = mirror.settings_schema();
        for key in ["continue_run", "stop_protocol"] {
            let item = schema
                .sections
                .iter()
                .flat_map(|s| &s.items)
                .find(|item| item.key == key)
                .unwrap();
            assert!(
                matches!(item.kind, SettingKind::Button { enabled: true }),
                "{key} must be accessible in the UI mirror"
            );
        }
    }

    #[test]
    fn continue_click_outside_a_pause_cannot_acknowledge_a_later_pause() {
        let mut plugin = ready_plugin("continue-interlock");
        let mut control = MockControl::default();
        plugin.set_setting("continue_run", json!(1)).unwrap();
        plugin.drive(&mut control);
        assert!(!plugin.continue_pending);
        assert!(control.hosts.is_empty() && control.services.is_empty());
        plugin.begin(&mut control);
        let run = plugin.run.as_mut().unwrap();
        run.pending = None;
        run.next_renew_ms = u64::MAX;
        plugin.prepare(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Paused);
        plugin.set_setting("continue_run", json!(2)).unwrap();
        plugin.drive(&mut control);
        assert_ne!(plugin.run.as_ref().unwrap().phase, Phase::Paused);
        assert!(plugin.run.as_ref().unwrap().pause_acknowledged);
    }
}
