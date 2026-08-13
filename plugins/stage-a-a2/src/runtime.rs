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
    ModulationRequestV1, ModulationResponseV1, ModulationStateV1, PdqReceiptV1, PdqStartSpecV1,
    PdqTerminationV1, PhotodiodeCommandV1, PhotodiodePlacementV1, PhotodiodeRequestV1,
    PhotodiodeResponseV1, PhotodiodeSummaryV1, RequestId, RequestOutcomeV1, RunId,
    SemanticRevision, CTX_STAGE_A_MODULATION_STATE_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
    SERVICE_STAGE_A_MODULATION_CONTROL_V1, SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
};

use crate::protocol::{self, Acquisition, Point, Protocol};

const ID: &str = "stage-a.a2";
const MOD_ID: &str = "stage-a.modulation";
const PD_ID: &str = "stage-a.photodiode";
const TIMEOUT_MS: u64 = 20_000;
const LEASE_TTL_MS: u64 = 60_000;

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
    valid: bool,
    failure: Option<String>,
}

struct Run {
    protocol: Protocol,
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
            message: "Choose an output folder and a fully qualified A2 protocol".into(),
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
        if self.output_folder.trim().is_empty() {
            return Some("choose an output folder".into());
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
        if self.sensor.and_then(|s| s.pixel_dead_time_us).is_none() {
            return Some("sensor pixel-dead-time readout is missing".into());
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
        if let Some(dead) = self.sensor.and_then(|s| s.pixel_dead_time_us) {
            if f64::from(plan.controller.min_half_us) < 5.0 * f64::from(dead) {
                self.message = format!(
                    "A2 refused: min_half_us={} is below 5 x sensor dead time ({dead:.2} us)",
                    plan.controller.min_half_us
                );
                return;
            }
        }
        let measurement_id = if self.measurement_id.trim().is_empty() {
            format!("A2-{}", compact_time())
        } else {
            safe(self.measurement_id.trim())
        };
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
                configuration: CameraConfigurationSourceV1::NamedProfile {
                    name: self.run.as_ref().unwrap().protocol.camera.profile.clone(),
                },
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
        let (p, c) = {
            let r = self.run.as_ref().unwrap();
            (
                r.protocol.points[r.index].clone(),
                r.protocol.controller.clone(),
            )
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
                half_period_s,
                transitions_per_polarity,
                comparator_threshold_dac,
            } => {
                let hz = 1.0 / (2.0 * half_period_s);
                let cfg = A2AcquisitionConfigV1 {
                    mean_u_milli: (mean_u * 1000.0).round() as u32,
                    depth_a_milli: (depth_a * 1000.0).round() as u32,
                    frequency_millihz: (hz * 1000.0).round() as u64,
                    min_half_us: c.min_half_us,
                    v_null_dac: c.v_null_dac,
                    v_peak_dac: c.v_peak_dac,
                    comparator_threshold_dac,
                    comparator_hysteresis: c.comparator_hysteresis,
                    comparator_invert: c.comparator_invert,
                    sample_rate_hz: c.sample_rate_hz,
                    block_samples: c.block_samples,
                    emit_raw_samples: true,
                    emit_summary: true,
                };
                self.run
                    .as_mut()
                    .unwrap()
                    .evidence
                    .expected_triggers_per_polarity = u64::from(transitions_per_polarity);
                self.send_mod(
                    control,
                    ModulationCommandV1::PrepareA2 { configuration: cfg },
                    true,
                );
            }
        }
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
            ("transfer_scope", r.protocol.optical.transfer_scope.clone()),
            (
                "photodiode_placement",
                r.protocol.optical.photodiode_placement.clone(),
            ),
            (
                "splitter_fraction_to_pd",
                r.protocol.optical.splitter_fraction_to_pd.to_string(),
            ),
            (
                "optical_config_id",
                r.protocol.optical.optical_config_id.clone(),
            ),
        ] {
            m.insert(k.into(), v);
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
                        "comparator_threshold_dac",
                        comparator_threshold_dac.to_string(),
                    ),
                ] {
                    m.insert(key.into(), value);
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
                    expected_sample_rate_hz: Some(r.protocol.controller.sample_rate_hz),
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
                    let requested = self.run.as_ref().unwrap().protocol.camera.profile.clone();
                    let refusal = camera_configuration_refusal(
                        &snapshot,
                        &provenance,
                        &requested,
                        readback_age_s,
                    );
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
                    format!("camera profile was not applied and confirmed: {outcome:?}"),
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
            protocol_path: &'a str,
            protocol_sha256: &'a str,
            protocol_archive_path: &'a str,
            protocol_name: &'a str,
            protocol_row: usize,
            camera_profile: &'a str,
            camera_provenance: Option<&'a CameraConfigurationProvenanceV1>,
            camera_readback_age_s: Option<f64>,
            optical: &'a protocol::OpticalSetup,
            gates: &'a protocol::Gates,
            controller: &'a protocol::ControllerSetup,
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
            protocol_path: &r.protocol_path,
            protocol_sha256: &r.protocol_sha256,
            protocol_archive_path: &r.protocol_archive_path,
            protocol_name: &r.protocol.name,
            protocol_row: r.index + 1,
            camera_profile: &r.protocol.camera.profile,
            camera_provenance: r.camera_provenance.as_ref(),
            camera_readback_age_s: r.camera_readback_age_s,
            optical: &r.protocol.optical,
            gates: &r.protocol.gates,
            controller: &r.protocol.controller,
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
            if r.evidence.peak_events_per_us > r.protocol.gates.recorder_safety_limit_events_per_us
            {
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
        SettingsSchema{sections:vec![SettingsSection{label:"A2 protocol".into(),description:Some("The file is validated completely before any owner lease or camera recording starts.".into()),default_open:true,items:vec![SettingItem{key:"output_folder".into(),label:"Output folder".into(),tooltip:None,kind:SettingKind::Path{dialog:PathDialogKind::Directory,default:self.output_folder.clone()}},SettingItem{key:"measurement_id".into(),label:"Measurement id".into(),tooltip:None,kind:SettingKind::Text{default:self.measurement_id.clone()}},SettingItem{key:"protocol_path".into(),label:"Protocol".into(),tooltip:None,kind:SettingKind::Path{dialog:PathDialogKind::OpenFile,default:self.protocol_path.clone()}},SettingItem{key:"run_protocol".into(),label:"Run protocol".into(),tooltip:None,kind:SettingKind::Button{enabled:true}},SettingItem{key:"continue_run".into(),label:"Continue".into(),tooltip:None,kind:SettingKind::Button{enabled:true}},SettingItem{key:"stop_protocol".into(),label:"Stop".into(),tooltip:None,kind:SettingKind::Button{enabled:true}}]}]}
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
    provenance: &CameraConfigurationProvenanceV1,
    requested_profile: &str,
    readback_age_s: f64,
) -> Option<String> {
    if provenance.profile_name.as_deref() != Some(requested_profile) {
        return Some(format!(
            "host confirmed camera profile {:?}, expected {requested_profile:?}",
            provenance.profile_name
        ));
    }
    if snapshot.digital_filter.stc_enabled
        || snapshot.digital_filter.trail_enabled
        || snapshot.digital_filter.erc_enabled != Some(false)
    {
        return Some(
            "applied camera profile must explicitly confirm STC, Trail and ERC off".into(),
        );
    }
    if !snapshot.external_trigger.enabled {
        return Some("applied camera profile has EXT_TRIGGER disabled".into());
    }
    if !snapshot.global.record_sensor_telemetry {
        return Some("applied camera profile does not record sensor telemetry".into());
    }
    if !readback_age_s.is_finite() || readback_age_s < 0.0 {
        return Some("host returned an invalid sensor readback age".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
[camera]
profile="A2 qualified"
[optical]
transfer_scope="fluorescence_chain"
photodiode_placement="emission_path"
splitter_fraction_to_pd=0.5
optical_config_id="opt-1"
[gates]
firmware_a2_confirmed=true
comparator_self_test_passed=true
camera_external_trigger_confirmed=true
h4_loopback_id="h4-1"
h5_polarity_calibration_id="h5-1"
optical_edge_calibration_id="edge-1"
local_flux_calibration_id="flux-1"
recorder_safety_limit_events_per_us=1000
[controller]
v_null_dac=100
v_peak_dac=1000
comparator_hysteresis=1
comparator_invert=false
min_half_us=1000
sample_rate_hz=500000
block_samples=256
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
            comparator_threshold_dac: 500,
        };
        assert!(validate_trigger_counts(&acquisition, 100, 99).is_ok());
        assert!(validate_trigger_counts(&acquisition, 2, 2).is_err());
    }

    #[test]
    fn camera_profile_requires_explicit_erc_off() {
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
        let provenance = CameraConfigurationProvenanceV1 {
            source: "named_profile".into(),
            profile_name: Some("A2 qualified".into()),
            schema_version: 1,
            profile_revision: Some(1),
            sha256: "ab".repeat(32),
        };
        assert!(
            camera_configuration_refusal(&snapshot, &provenance, "A2 qualified", 0.1)
                .unwrap()
                .contains("ERC")
        );
        snapshot.digital_filter.erc_enabled = Some(false);
        assert!(
            camera_configuration_refusal(&snapshot, &provenance, "A2 qualified", 0.1).is_none()
        );
    }

    #[test]
    fn end_to_end_runs_paused_dark_then_stepped_and_restores_camera() {
        let mut plugin = ready_plugin("e2e-success");
        let mut control = MockControl::default();
        plugin.begin(&mut control);
        assert!(matches!(
            control.hosts.last().unwrap().command,
            HostCommand::ApplyCameraConfiguration { .. }
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
}

export_plugin!(StageAA2Plugin);
