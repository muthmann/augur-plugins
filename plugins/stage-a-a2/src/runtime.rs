use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use augur_plugin_api::{
    export_plugin, CameraConfigurationProvenanceV1, CameraConfigurationSnapshotV1,
    CameraConfigurationSourceV1, EventStoreHandle, GlobalSettings, HostCommand, HostCommandOutcome,
    HostCommandRequest, HostContext, HostOutput, PathDialogKind, Plugin, PluginCapabilities,
    PluginControlContext, PluginControlInbox, PluginDiscontinuity, PluginFrame, PluginInput,
    PluginRuntimeRole, PluginServiceOutcome, PluginServiceRequest, SensorMonitoringV1, SettingItem,
    SettingKind, SettingsSchema, SettingsSection, StatusEntry, CTX_GLOBAL_SETTINGS,
    CTX_SENSOR_MONITORING,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use stage_a_plugin_contract::{
    A2AcquisitionConfigV1, ClientId, ConnectionStateV1, LeaseId, ModulationCommandV1,
    ModulationRequestV1, ModulationResponseV1, ModulationStateV1, OpticalTargetV1, PdqReceiptV1,
    PdqStartSpecV1, PdqTerminationV1, PhotodiodeCommandV1, PhotodiodeDarkReferenceV1,
    PhotodiodeLevelV1, PhotodiodePlacementV1, PhotodiodeRequestV1, PhotodiodeResponseV1,
    PhotodiodeSummaryV1, RequestId, RequestOutcomeV1, RunId, SemanticRevision, StreamIntegrityV1,
    WaveformV1, CTX_STAGE_A_MODULATION_STATE_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
    SERVICE_STAGE_A_MODULATION_CONTROL_V1, SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
};

use crate::protocol::{self, Acquisition, ControllerSetup, Point, Protocol};

const ID: &str = "stage-a.a2";
const MOD_ID: &str = "stage-a.modulation";
const PD_ID: &str = "stage-a.photodiode";
const TIMEOUT_MS: u64 = 20_000;
const LEASE_TTL_MS: u64 = 60_000;
/// Conservative fail-closed limit for the first small-ROI A2 runs. The actual
/// peak is written per point; H21 can later lower this bound without changing
/// every protocol file.
const RECORDER_SAFETY_LIMIT_EVENTS_PER_US: u64 = 6;

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
/// Smallest plateau-to-plateau span a threshold may be placed inside, in volts.
/// 20 mV, the same floor the bring-up program warns at.
const MIN_PLATEAU_SPAN_VOLTS: f64 = 0.020;
/// Largest share of the plateau span either settled window may itself wander
/// over. A settled `CONST` hold has a small peak-to-peak; a drifting or still
/// slewing one does not, and its mean is not a plateau.
const MAX_PLATEAU_WINDOW_SPREAD_FRACTION: f64 = 0.5;

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
    Settle,
    StartCamera,
    StartPd,
    StartMod,
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
    pdq_sha256: Option<String>,
    /// `"auto"` or `"frozen"`: whether this point's threshold was measured by
    /// the runner or asserted by the protocol.
    comparator_threshold_mode: Option<&'static str>,
    /// The code actually sent on the `CMP thr=` path for this point.
    comparator_threshold_dac: Option<u16>,
    /// Full plateau evidence when the threshold was measured. Shared by every
    /// point of the same pedestal, and recorded on each of them so a single
    /// sidecar is self-contained.
    threshold_measurement: Option<MeasuredThreshold>,
    valid: bool,
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
    optical_target: OpticalTargetV1,
    requested_mean_u_milli: u32,
    resolved_mean_u_milli: u32,
    internal_u_milli: u32,
    armed_depth_a_milli: u32,
    /// Which measured Pockels transfer inversion produced the lobe. `None`
    /// means the modulation operator entered the endpoints by hand, and the run
    /// says so rather than implying a calibration that does not exist.
    modulation_calibration_id: Option<String>,
    modulation_owner_instance: String,
    min_half_us: u32,
    /// `"protocol"` or `"sensor_telemetry"`.
    min_half_us_source: &'static str,
    pixel_dead_time_us: f32,
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
}

impl PlateauStep {
    fn new(level_dac: u16) -> Self {
        Self {
            level_dac,
            acknowledged_sample_index: 0,
            stream_epoch: 0,
            level: None,
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
    index: usize,
    phase: Phase,
    pending: Option<(PendingKind, u64, u64)>,
    lease: LeaseId,
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
    camera_provenance: Option<CameraConfigurationProvenanceV1>,
    camera_readback_age_s: Option<f64>,
    pause_acknowledged: bool,
    last_event_bin_us: Option<u64>,
    last_event_bin_count: u64,
    evidence: PointEvidence,
}

pub struct StageAA2Plugin {
    enabled: bool,
    role: PluginRuntimeRole,
    output_folder: String,
    measurement_id: String,
    protocol_path: String,
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
            enabled: true,
            role: PluginRuntimeRole::LiveWorker,
            output_folder: String::new(),
            measurement_id: String::new(),
            protocol_path: String::new(),
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
        if self.sensor.and_then(|s| s.pixel_dead_time_us).is_none() {
            return Some("sensor pixel-dead-time readout is missing".into());
        }
        if self
            .modulation
            .as_ref()
            .is_none_or(|state| state.optical_drive.is_none())
        {
            return Some(
                "arm a calibrated optical drive on the modulation owner so the A2 lobe resolves"
                    .into(),
            );
        }
        None
    }

    fn begin(&mut self, control: &mut impl Control) {
        if let Some(blocker) = self.blocker() {
            self.message = format!("A2 refused: {blocker}");
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
        // Everything below happens before the camera configuration is applied and
        // before either owner lease is acquired: an unresolvable lobe, an
        // unresolvable step floor or an unreachable plateau must refuse while
        // the bench is still untouched.
        let controller = ControllerSetup::default();
        let photodiode_setup = resolve_photodiode(
            self.photodiode
                .as_ref()
                .expect("blocker confirmed the photodiode owner"),
        );
        self.output_folder = self
            .photodiode
            .as_ref()
            .and_then(|summary| summary.data_dir.clone())
            .expect("blocker confirmed the photodiode data folder");
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
        let measurement_id = format!("A2-{}", compact_time());
        self.measurement_id = measurement_id.clone();
        let hash = hex_hash(text.as_bytes());
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
            measurement_id,
            index: 0,
            phase: Phase::ApplyCamera,
            pending: None,
            lease: LeaseId::new(format!("a2-{}", now_ms())),
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
            camera_provenance: None,
            camera_readback_age_s: None,
            pause_acknowledged: false,
            last_event_bin_us: None,
            last_event_bin_count: 0,
            evidence: PointEvidence::default(),
        });
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
        let request_id = self.next_id();
        let (lease, run_id, owner) = {
            let run = self.run.as_ref().unwrap();
            (
                run.lease.clone(),
                run.run_id.clone(),
                self.modulation.as_ref().map(|s| s.owner_instance.clone()),
            )
        };
        let mut e = ModulationRequestV1::new(RequestId(request_id), ClientId::new(ID), command);
        e.lease_id = Some(lease);
        e.target_owner_instance = owner;
        e.issued_at_unix_ms = now_ms();
        if !run_id.is_empty() {
            e.run_id = Some(RunId::new(run_id));
        }
        if revision {
            e.requested_revision = Some(self.next_revision());
        }
        control.service(&PluginServiceRequest {
            request_id,
            source_plugin_id: ID.into(),
            target_plugin_id: MOD_ID.into(),
            service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
            payload: serde_json::to_value(e).unwrap(),
        });
        self.run.as_mut().unwrap().pending = Some((PendingKind::Mod, request_id, now_ms()));
    }

    fn send_pd(
        &mut self,
        control: &mut impl Control,
        command: PhotodiodeCommandV1,
        revision: bool,
    ) {
        let request_id = self.next_id();
        let (lease, run_id, owner) = {
            let run = self.run.as_ref().unwrap();
            (
                run.lease.clone(),
                run.run_id.clone(),
                self.photodiode.as_ref().map(|s| s.owner_instance.clone()),
            )
        };
        let mut e = PhotodiodeRequestV1::new(RequestId(request_id), ClientId::new(ID), command);
        e.lease_id = Some(lease);
        e.target_owner_instance = owner;
        e.issued_at_unix_ms = now_ms();
        if !run_id.is_empty() {
            e.run_id = Some(RunId::new(run_id));
        }
        if revision {
            e.requested_revision = Some(self.next_revision());
        }
        control.service(&PluginServiceRequest {
            request_id,
            source_plugin_id: ID.into(),
            target_plugin_id: PD_ID.into(),
            service: SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1.into(),
            payload: serde_json::to_value(e).unwrap(),
        });
        self.run.as_mut().unwrap().pending = Some((PendingKind::Pd, request_id, now_ms()));
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
        if should_pause(&p, self.run.as_ref().unwrap().pause_acknowledged) {
            self.run.as_mut().unwrap().phase = Phase::Paused;
            self.message = format!("Paused before {}", p.label);
            return;
        }
        let run = self.run.as_mut().unwrap();
        run.phase = Phase::Prepare;
        run.run_id = format!(
            "{}_r{:03}_{}",
            run.measurement_id,
            run.index + 1,
            safe(&p.label)
        );
        run.evidence = PointEvidence::default();
        run.last_event_bin_us = None;
        run.last_event_bin_count = 0;
        run.camera_stop_attempts = 0;
        run.probe = None;
        match p.acquisition {
            Acquisition::Dark { .. } => self.send_mod(
                control,
                ModulationCommandV1::StopAcquisition {
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
                // `V_50` moves with the operating flux, so it is measured once
                // per pedestal and reused by every point that shares one.
                let measured = self
                    .run
                    .as_ref()
                    .and_then(|run| run.thresholds.get(&key).cloned());
                if let Some(measured) = measured {
                    let code = measured.threshold_dac;
                    self.run.as_mut().unwrap().evidence.threshold_measurement = Some(measured);
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
            run.probe = None;
            run.evidence.expected_triggers_per_polarity = u64::from(transitions_per_polarity);
            run.evidence.comparator_threshold_dac = Some(threshold_dac);
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
        let now = now_ms();
        let run = self.run.as_mut().unwrap();
        if let Some(probe) = run.probe.as_mut() {
            let step = if low { &mut probe.low } else { &mut probe.high };
            step.acknowledged_sample_index = acknowledged_sample_index;
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
        let qualified = level.filter(|level| {
            level.sample_count > 0
                && level.end_sample_index >= level.sample_count
                && level.end_sample_index - level.sample_count >= step.acknowledged_sample_index
        });
        let Some(level) = qualified else {
            if now >= timeout {
                self.fail(
                    control,
                    format!(
                        "no photodiode level window began after the acknowledged plateau drive \
                         within {PLATEAU_LEVEL_TIMEOUT_MS} ms"
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
                r.resolved
                    .modulation_calibration_id
                    .clone()
                    .unwrap_or_else(|| "operator_entered_lobe".into()),
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
            r.index += 1;
            r.pause_acknowledged = false;
            r.index >= r.protocol.points.len() || r.stop
        };
        if done {
            self.release_next(control);
        } else {
            self.prepare(control);
        }
    }

    fn release_next(&mut self, control: &mut impl Control) {
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
        let failed = self
            .run
            .as_ref()
            .and_then(|run| run.abort_reason.as_ref())
            .is_some();
        if !failed {
            self.message = "A2 protocol finished; inspect point sidecars and offline first-event distributions".into();
        }
        self.run = None;
    }

    fn fail(&mut self, control: &mut impl Control, reason: String) {
        self.message = format!("A2 failed closed: {reason}");
        let Some(run) = self.run.as_mut() else { return };
        run.stop = true;
        run.evidence.failure = Some(reason.clone());
        run.abort_reason = Some(reason);
        run.pending = None;
        if run.modulation_active {
            run.phase = Phase::StopMod;
            self.send_mod(
                control,
                ModulationCommandV1::StopAcquisition {
                    reason: "A2 abort".into(),
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
        if self.continue_pending {
            self.continue_pending = false;
            if self.run.as_ref().is_some_and(|r| r.phase == Phase::Paused) {
                self.run.as_mut().unwrap().pause_acknowledged = true;
                self.prepare(control);
            }
        }
        let Some(run) = self.run.as_ref() else { return };
        if let Some((_, _, sent)) = run.pending {
            if now_ms().saturating_sub(sent) > TIMEOUT_MS {
                match run.phase {
                    Phase::ReleasePd => {
                        self.run.as_mut().unwrap().pd_leased = false;
                        self.run.as_mut().unwrap().pending = None;
                        self.release_next(control);
                    }
                    Phase::ReleaseMod => {
                        self.run.as_mut().unwrap().mod_leased = false;
                        self.run.as_mut().unwrap().pending = None;
                        self.release_next(control);
                    }
                    Phase::FinalizePd => {
                        let run = self.run.as_mut().unwrap();
                        run.pending = None;
                        run.pd_recording = false;
                        run.evidence.failure = Some("photodiode finalize timed out".into());
                        self.stop_camera(control);
                    }
                    Phase::StopCamera => {
                        self.run.as_mut().unwrap().pending = None;
                        if self.run.as_ref().unwrap().camera_stop_attempts < 3 {
                            self.stop_camera(control);
                        } else {
                            let run = self.run.as_mut().unwrap();
                            run.camera_recording = false;
                            run.evidence.failure =
                                Some("camera stop timed out after 3 attempts".into());
                            let _ = self.write_sidecar();
                            self.release_next(control);
                        }
                    }
                    _ => self.fail(control, "owner/host reply timed out".into()),
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
                .unwrap_or_else(|| "A2 stopped".into());
            self.fail(control, reason);
            return;
        }
        if !matches!(
            run.phase,
            Phase::AcquireMod | Phase::AcquirePd | Phase::ReleasePd | Phase::ReleaseMod
        ) && now_ms() >= run.next_renew_ms
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
                        ModulationCommandV1::StopAcquisition {
                            reason: "A2 point complete".into(),
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
        if kind == PendingKind::Mod {
            if let Ok(response) = serde_json::from_value::<ModulationResponseV1>(payload.clone()) {
                if response.common.outcome == RequestOutcomeV1::InProgress {
                    self.run.as_mut().unwrap().pending =
                        Some((PendingKind::Mod, response.common.request_id.0, now_ms()));
                    return;
                }
            }
        }
        self.run.as_mut().unwrap().pending = None;
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
            (PendingKind::Mod, Phase::Prepare) => {
                let settle = (self.point().unwrap().settle_s * 1000.0) as u64;
                let r = self.run.as_mut().unwrap();
                r.phase = Phase::Settle;
                r.deadline_ms = now_ms() + settle;
            }
            (PendingKind::Host, Phase::StartCamera) => {
                let r = self.run.as_ref().unwrap();
                let spec = PdqStartSpecV1 {
                    pdq_path: format!("{}/{}.pdq", r.measurement_id, r.run_id),
                    sidecar_path: format!("{}/{}.pd.json", r.measurement_id, r.run_id),
                    expected_sample_rate_hz: Some(r.controller.sample_rate_hz),
                    expected_stream_epoch: self.photodiode.as_ref().map(|p| p.stream.stream_epoch),
                    metadata: self.metadata(),
                    root_dir: Some(self.output_folder.clone()),
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
            (PendingKind::Pd, Phase::StartPd) => {
                if !starts_modulation(&self.point().unwrap().acquisition) {
                    let seconds = self.point().unwrap().acquisition_seconds();
                    let r = self.run.as_mut().unwrap();
                    r.phase = Phase::Recording;
                    r.deadline_ms = now_ms() + (seconds * 1000.0) as u64;
                } else {
                    let run = self.run.as_mut().unwrap();
                    run.phase = Phase::StartMod;
                    run.modulation_active = true;
                    self.send_mod(control, ModulationCommandV1::StartAcquisition, true);
                }
            }
            (PendingKind::Mod, Phase::StartMod) => {
                let seconds = self.point().unwrap().acquisition_seconds();
                let r = self.run.as_mut().unwrap();
                r.phase = Phase::Recording;
                r.deadline_ms = now_ms() + (seconds * 1000.0) as u64;
            }
            (PendingKind::Mod, Phase::StopMod) => {
                let run = self.run.as_mut().unwrap();
                run.modulation_active = false;
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
                let e = &mut self.run.as_mut().unwrap().evidence;
                if let Some(receipt) = finalized {
                    e.pdq_path = Some(receipt.pdq_path);
                    e.pdq_sha256 = Some(receipt.sha256.to_string());
                    if !receipt.valid {
                        e.failure = Some("PDQ receipt invalid".into());
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
            (PendingKind::Mod, _) => self.send_pd(
                control,
                PhotodiodeCommandV1::RenewLease {
                    ttl_ms: LEASE_TTL_MS,
                },
                false,
            ),
            (PendingKind::Pd, _) => self.run.as_mut().unwrap().next_renew_ms = now_ms() + 30_000,
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

    fn finish_async_mod(&mut self, control: &mut impl Control) {
        let Some((PendingKind::Mod, request_id, _)) = self.run.as_ref().and_then(|r| r.pending)
        else {
            return;
        };
        let Some(response) = self
            .modulation
            .as_ref()
            .and_then(|s| s.last_response.as_ref())
        else {
            return;
        };
        if response.common.request_id.0 != request_id
            || response.common.outcome == RequestOutcomeV1::InProgress
        {
            return;
        }
        if response.common.outcome == RequestOutcomeV1::Rejected {
            let phase = self.run.as_ref().unwrap().phase;
            if phase == Phase::ReleaseMod {
                self.run.as_mut().unwrap().mod_leased = false;
                self.release_next(control);
            } else if phase == Phase::StopMod {
                let run = self.run.as_mut().unwrap();
                run.modulation_active = false;
                run.phase = Phase::FinalizePd;
                self.send_pd(
                    control,
                    PhotodiodeCommandV1::FinalizeRecording {
                        termination: PdqTerminationV1::Aborted,
                    },
                    true,
                );
            } else {
                self.fail(
                    control,
                    response
                        .common
                        .error
                        .as_ref()
                        .map(|e| e.message.clone())
                        .unwrap_or_else(|| "modulation owner rejected A2".into()),
                );
            }
            return;
        }
        let payload = serde_json::to_value(response).unwrap_or(Value::Null);
        self.accepted(control, PendingKind::Mod, &payload);
    }

    fn finish_point(&mut self, control: &mut impl Control, outcome: &HostCommandOutcome) {
        if let HostCommandOutcome::RecordingFinalized {
            actual_raw_path,
            size: _,
            sha256,
            duration_us: _,
        } = outcome
        {
            let acquisition = {
                let run = self.run.as_ref().unwrap();
                run.protocol.points[run.index].acquisition.clone()
            };
            let e = &mut self.run.as_mut().unwrap().evidence;
            e.raw_path = Some(actual_raw_path.clone());
            e.raw_sha256 = Some(sha256.clone());
            let trigger_problem =
                validate_trigger_counts(&acquisition, e.rising_triggers, e.falling_triggers).err();
            if e.failure.is_none() {
                e.failure = trigger_problem;
            }
            e.valid = e.failure.is_none();
            self.run.as_mut().unwrap().camera_recording = false;
        } else if let HostCommandOutcome::RecordingPartial { reason, .. } = outcome {
            let run = self.run.as_mut().unwrap();
            run.camera_recording = false;
            run.evidence.failure = Some(format!("RAW finalized partially: {reason}"));
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
        let _ = self.write_sidecar();
        if self
            .run
            .as_ref()
            .is_some_and(|run| run.abort_reason.is_some())
        {
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
                    snapshot,
                    provenance,
                    readback: _,
                    readback_age_s,
                } => {
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
                self.message = format!(
                    "{}; camera restore was not confirmed after 3 attempts: {:?}",
                    self.message, outcome
                );
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
                    evidence.raw_path = Some(actual_raw_path);
                    self.accepted(control, PendingKind::Host, &Value::Null)
                }
                outcome => self.fail(control, format!("camera start failed: {outcome:?}")),
            }
        } else if phase == Phase::StopCamera {
            self.finish_point(control, &outcome)
        }
    }

    fn write_sidecar(&self) -> Result<(), String> {
        let r = self.run.as_ref().unwrap();
        let dir = Path::new(&self.output_folder).join(&r.measurement_id);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        #[derive(Serialize)]
        struct Side<'a> {
            schema_version: u32,
            experiment: &'static str,
            scientific_status: &'static str,
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
            schema_version: 1,
            experiment: "A2",
            scientific_status: "requires_offline_h4_h5_review",
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
        std::fs::write(dir.join(format!("{}.a2.json", r.run_id)), bytes).map_err(|e| e.to_string())
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
        self.run = None;
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
                if t.is_rising() {
                    r.evidence.rising_triggers += 1
                } else {
                    r.evidence.falling_triggers += 1
                }
            }
            if r.evidence.peak_events_per_us > RECORDER_SAFETY_LIMIT_EVENTS_PER_US {
                r.stop = true;
                r.evidence.failure = Some(format!(
                    "pre-qualified recorder safety limit exceeded: {} events/us",
                    r.evidence.peak_events_per_us
                ));
            }
        }
    }
    fn process_control(&mut self, c: &mut PluginControlContext<'_>) {
        let inbox = c.inbox().clone();
        self.snapshots(&inbox);
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
                    let phase = self.run.as_ref().unwrap().phase;
                    if phase == Phase::ReleasePd {
                        self.run.as_mut().unwrap().pd_leased = false;
                        self.release_next(c);
                    } else if phase == Phase::ReleaseMod {
                        self.run.as_mut().unwrap().mod_leased = false;
                        self.release_next(c);
                    } else if phase == Phase::FinalizePd {
                        let run = self.run.as_mut().unwrap();
                        run.pd_recording = false;
                        run.evidence.failure = Some(format!("{code}: {message}"));
                        self.stop_camera(c);
                    } else if phase == Phase::StopMod {
                        let run = self.run.as_mut().unwrap();
                        run.modulation_active = false;
                        run.evidence.failure = Some(format!("{code}: {message}"));
                        run.phase = Phase::FinalizePd;
                        self.send_pd(
                            c,
                            PhotodiodeCommandV1::FinalizeRecording {
                                termination: PdqTerminationV1::Aborted,
                            },
                            true,
                        );
                    } else {
                        self.fail(c, format!("{code}: {message}"))
                    }
                }
            }
        }
        for reply in inbox.host_replies {
            self.host_reply(c, reply.request_id, reply.outcome);
        }
        self.drive(c);
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
                        SettingItem { key: "protocol_path".into(), label: "Protocol".into(), tooltip: None, kind: SettingKind::Path { dialog: PathDialogKind::OpenFile, default: self.protocol_path.clone() } },
                        SettingItem { key: "run_protocol".into(), label: "Run protocol".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
                        SettingItem { key: "continue_run".into(), label: "Continue".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
                        SettingItem { key: "stop_protocol".into(), label: "Stop".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
                    ],
                },
                SettingsSection {
                    label: "Before the first A2 run".into(),
                    description: Some(
                        "1. Record the generic dark, crosstalk and static-light data in Guided PD references. A2 reads the selected reference set automatically.\n\n2. Keep H4 loopback and H5 polarity/invert as comparator-specific A2 evidence.\n\n3. Run the optical-edge points. A2 sets LOG_SQUARE, measures both plateaus, selects V50, records camera RAW plus PDQ, and restores the camera configuration.\n\nDo not infer invert from the drawing. H5 fixes whether a physical rising optical edge is reported as rising or falling."
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
            "run_protocol" => Some(json!(self.start.value)),
            "continue_run" => Some(json!(self.continue_press.value)),
            "stop_protocol" => Some(json!(self.stop.value)),
            _ => None,
        }
    }
    fn set_setting(&mut self, k: &str, v: Value) -> Result<(), String> {
        match k {
            "output_folder" => self.output_folder = v.as_str().ok_or("string required")?.into(),
            "measurement_id" => self.measurement_id = v.as_str().ok_or("string required")?.into(),
            "protocol_path" => self.protocol_path = v.as_str().ok_or("string required")?.into(),
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
        } else if let Some(b) = self.blocker() {
            v.push(StatusEntry::Text(format!("Not ready: {b}")));
        }
        v
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
    let Some(drive) = state.optical_drive.as_ref() else {
        return Err(
            "the modulation owner publishes no optical_drive; arm a calibrated optical drive so \
             V_null/V_peak resolve"
                .into(),
        );
    };
    if drive.v_peak_dac <= drive.v_null_dac {
        return Err(format!(
            "resolved lobe is not usable: v_peak_dac={} must exceed v_null_dac={}",
            drive.v_peak_dac, drive.v_null_dac
        ));
    }
    if drive.v_peak_dac > STIMULUS_MAX_CODE {
        return Err(format!(
            "resolved lobe maximum {} is outside the stimulus DAC range 0..={STIMULUS_MAX_CODE}",
            drive.v_peak_dac
        ));
    }
    if controller.comparator_hysteresis > 3 {
        return Err(format!(
            "comparator_hysteresis={} is outside the comparator's 0..=3 range",
            controller.comparator_hysteresis
        ));
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
        lobe_source: "modulation_owner.optical_drive",
        v_null_dac: drive.v_null_dac,
        v_peak_dac: drive.v_peak_dac,
        optical_target: drive.target,
        requested_mean_u_milli: drive.requested_mean_u_milli,
        resolved_mean_u_milli: drive.resolved_mean_u_milli,
        internal_u_milli: drive.internal_u_milli,
        armed_depth_a_milli: drive.depth_a_milli,
        modulation_calibration_id: state.calibration_id.clone(),
        modulation_owner_instance: state.owner_instance.to_string(),
        min_half_us,
        min_half_us_source,
        pixel_dead_time_us,
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
        if !comparator_threshold_dac.is_auto() {
            continue;
        }
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
    if !(1.0..=f64::from(THRESHOLD_MAX_CODE)).contains(&code) {
        return Err(format!(
            "plateau midpoint {millivolt:.1} mV is outside the 0..2500 mV comparator threshold \
             DAC range"
        ));
    }
    Ok(code as u16)
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
            "plateau span {:.1} mV is under the {:.0} mV floor at mean_u={} m, a={} m; a threshold \
             placed in here would be sitting in noise",
            span_volts * 1000.0,
            MIN_PLATEAU_SPAN_VOLTS * 1000.0,
            probe.mean_u_milli,
            probe.depth_a_milli
        ));
    }
    // A settled hold has a small peak-to-peak. A window that wanders over a
    // large share of the very step it is meant to define was still slewing, and
    // its mean is not a plateau.
    let spread_ceiling = MAX_PLATEAU_WINDOW_SPREAD_FRACTION * span_volts;
    for (label, level) in [("dim", low), ("bright", high)] {
        if !level.peak_to_peak_volts.is_finite() || level.peak_to_peak_volts > spread_ceiling {
            return Err(format!(
                "{label} plateau window wanders {:.1} mV over a {:.1} mV step; it had not settled",
                level.peak_to_peak_volts * 1000.0,
                span_volts * 1000.0
            ));
        }
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
            "external-trigger count implausible: expected {expected} +/- 1 per polarity, observed rising={rising}, falling={falling}"
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
    use crate::protocol::ComparatorThreshold;

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

    fn armed_optical_drive() -> stage_a_plugin_contract::OpticalDriveStateV1 {
        stage_a_plugin_contract::OpticalDriveStateV1 {
            target: OpticalTargetV1::LogSine,
            requested_mean_u_milli: 300,
            resolved_mean_u_milli: 300,
            internal_u_milli: 290,
            depth_a_milli: 450,
            v_null_dac: 100,
            v_peak_dac: 1_000,
        }
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
            optical_drive: Some(armed_optical_drive()),
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
                run_id: Some(RunId::new(run_id)),
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
                sample_range: None,
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
    fn a2_ui_does_not_ask_for_owner_data_or_manual_ids() {
        let plugin = StageAA2Plugin::default();
        let keys = plugin
            .settings_schema()
            .sections
            .into_iter()
            .flat_map(|section| section.items)
            .map(|item| item.key)
            .collect::<Vec<_>>();
        assert!(!keys.iter().any(|key| key == "output_folder"));
        assert!(!keys.iter().any(|key| key == "measurement_id"));
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
        assert_eq!(plugin.output_folder, expected_output_folder);
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

        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Paused);
        plugin.continue_pending = true;
        plugin.drive(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Prepare);
        let dark_prepare: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        assert!(matches!(
            dark_prepare.command,
            ModulationCommandV1::StopAcquisition { .. }
        ));
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
        let camera_start = control.hosts.last().unwrap().request_id;
        plugin.host_reply(
            &mut control,
            camera_start,
            HostCommandOutcome::RecordingStarted {
                actual_raw_path: "/tmp/a2-dark.raw".into(),
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
                actual_raw_path: "/tmp/a2-dark.raw".into(),
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
                actual_raw_path: "/tmp/a2-step.raw".into(),
                started_at: "now".into(),
            },
        );
        plugin.accepted(&mut control, PendingKind::Pd, &Value::Null);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::StartMod);
        plugin.accepted(&mut control, PendingKind::Mod, &Value::Null);
        {
            let run = plugin.run.as_mut().unwrap();
            run.evidence.rising_triggers = 2;
            run.evidence.falling_triggers = 2;
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
                actual_raw_path: "/tmp/a2-step.raw".into(),
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
        assert!(plugin.message.contains("failed closed"));
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

    fn modulation_state(
        drive: Option<stage_a_plugin_contract::OpticalDriveStateV1>,
        calibration_id: Option<&str>,
    ) -> ModulationStateV1 {
        let mut plugin = ready_plugin("resolve-helper");
        let mut state = plugin.modulation.take().unwrap();
        state.optical_drive = drive;
        state.calibration_id = calibration_id.map(str::to_owned);
        state
    }

    #[test]
    fn a_missing_optical_drive_refuses_instead_of_defaulting_a_lobe() {
        let controller = ControllerSetup::default();
        let state = modulation_state(None, Some("lobe-1"));
        let error = resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0))))
            .unwrap_err();
        assert!(error.contains("optical_drive"), "{error}");

        let error =
            resolve_controller(&controller, None, Some(test_sensor(Some(10.0)))).unwrap_err();
        assert!(error.contains("modulation owner"), "{error}");
    }

    #[test]
    fn a_degenerate_lobe_refuses_against_the_resolved_values() {
        let controller = ControllerSetup::default();
        let mut drive = armed_optical_drive();
        drive.v_peak_dac = drive.v_null_dac;
        let state = modulation_state(Some(drive), Some("lobe-1"));
        let error = resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0))))
            .unwrap_err();
        assert!(error.contains("v_peak_dac"), "{error}");
    }

    #[test]
    fn the_resolved_lobe_and_its_calibration_id_reach_the_run_provenance() {
        let controller = ControllerSetup::default();
        let state = modulation_state(Some(armed_optical_drive()), Some("pockels-2026-08-01"));
        let resolved =
            resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0)))).unwrap();
        assert_eq!(resolved.v_null_dac, 100);
        assert_eq!(resolved.v_peak_dac, 1_000);
        assert_eq!(resolved.lobe_source, "modulation_owner.optical_drive");
        assert_eq!(
            resolved.modulation_calibration_id.as_deref(),
            Some("pockels-2026-08-01")
        );

        // A hand-entered lobe is recorded as such, never as a calibration.
        let state = modulation_state(Some(armed_optical_drive()), None);
        let resolved =
            resolve_controller(&controller, Some(&state), Some(test_sensor(Some(10.0)))).unwrap();
        assert_eq!(resolved.modulation_calibration_id, None);
    }

    #[test]
    fn an_absent_min_half_us_resolves_to_the_larger_of_refractory_and_settling_floors() {
        let controller = ControllerSetup::default();
        let state = modulation_state(Some(armed_optical_drive()), Some("lobe-1"));

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
        let state = modulation_state(Some(armed_optical_drive()), Some("lobe-1"));
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
        let state = modulation_state(Some(armed_optical_drive()), Some("lobe-1"));
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
    fn plan_pedestals_covers_each_distinct_pair_once_and_skips_frozen_points() {
        let plan = parsed(auto_protocol());
        let state = modulation_state(Some(armed_optical_drive()), Some("lobe-1"));
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
        assert!(plan_pedestals(&frozen_only, &resolved).unwrap().is_empty());
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
            },
            high: PlateauStep {
                level_dac: 478,
                acknowledged_sample_index: 2_000,
                stream_epoch: 1,
                level: Some(high),
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
        let high = level(1.005, 0.0005, 2_500, 400, false);
        let error = measured_threshold(&probe(low, high), low, high).unwrap_err();
        assert!(error.contains("under the 20 mV floor"), "{error}");
    }

    #[test]
    fn an_inverted_plateau_pair_refuses() {
        let low = level(1.2, 0.001, 1_500, 400, false);
        let high = level(1.0, 0.001, 2_500, 400, false);
        assert!(measured_threshold(&probe(low, high), low, high).is_err());
    }

    #[test]
    fn a_plateau_window_that_had_not_settled_refuses() {
        let low = level(1.0, 0.15, 1_500, 400, false);
        let high = level(1.2, 0.001, 2_500, 400, false);
        let error = measured_threshold(&probe(low, high), low, high).unwrap_err();
        assert!(error.contains("had not settled"), "{error}");
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
        // A window ending at 1500 over 400 samples starts at 1100, after the
        // drive was acknowledged at 1000.
        publish_stream(
            &mut plugin,
            1,
            1_500,
            Some(level(1.0, 0.001, 1_500, 400, false)),
        );
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);
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
        publish_stream(
            &mut plugin,
            1,
            3_000,
            Some(level(1.2, 0.001, 3_000, 400, false)),
        );
        plugin.run.as_mut().unwrap().deadline_ms = 0;
        plugin.drive(&mut control);

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
        assert_eq!(measurement.high_drive_acknowledged_sample_index, 1_500);

        // A second point on the same pedestal reuses the measurement instead of
        // driving the plateaus again.
        let services_before = control.services.len();
        {
            let run = plugin.run.as_mut().unwrap();
            run.pending = None;
            run.index = 1;
        }
        plugin.prepare(&mut control);
        assert_eq!(plugin.run.as_ref().unwrap().phase, Phase::Prepare);
        assert_eq!(control.services.len(), services_before + 1);
        let request: ModulationRequestV1 =
            serde_json::from_value(control.services.last().unwrap().payload.clone()).unwrap();
        let ModulationCommandV1::PrepareA2 { configuration } = request.command else {
            panic!("expected the cached threshold to go straight to PrepareA2");
        };
        assert_eq!(configuration.comparator_threshold_dac, 1_802);

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
            1_500,
            Some(level(3.29, 0.001, 1_500, 400, true)),
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
        plugin.modulation.as_mut().unwrap().optical_drive = None;
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        assert!(plugin.run.is_none());
        assert!(control.hosts.is_empty(), "no camera command may be issued");
        assert!(control.services.is_empty(), "no owner lease may be taken");
        assert!(plugin.message.contains("refused"), "{}", plugin.message);
    }
}

export_plugin!(StageAA2Plugin);
