//! Leased constant-light and photodiode capture for the matched A4 reference.
use augur_plugin_api::{PluginControlInbox, PluginServiceOutcome, PluginServiceRequest};
use serde_json::Value;
use stage_a_plugin_contract::*;
use std::collections::{BTreeMap, VecDeque};

const CLIENT: &str = "stage-a.a4";
const MOD: &str = "stage-a.modulation";
const PD: &str = "stage-a.photodiode";
#[derive(Clone, Debug)]
enum Command {
    Mod(ModulationCommandV1),
    Pd(PhotodiodeCommandV1),
}
#[derive(Debug)]
struct Pending {
    command: Command,
    request: PluginServiceRequest,
    sent: u64,
    poll: u64,
}
#[derive(Default, Debug)]
pub struct Devices {
    pub modulation: Option<ModulationStateV1>,
    pub photodiode: Option<PhotodiodeSummaryV1>,
    queue: VecDeque<Command>,
    pending: Option<Pending>,
    sequence: u64,
    revision: u64,
    run: String,
    mod_owner: Option<OwnerInstanceId>,
    pd_owner: Option<OwnerInstanceId>,
    mod_owned: bool,
    pd_owned: bool,
    renewed: u64,
    pub active: bool,
    closing: bool,
    pub error: Option<String>,
    pub receipts: Vec<Value>,
    expected_pd: Option<PdqStartSpecV1>,
    expected_seconds: f64,
}
impl Devices {
    pub fn ready(&self) -> bool {
        self.pending.is_none() && self.queue.is_empty() && self.error.is_none()
    }
    pub fn blocker(&self, now: u64) -> Option<String> {
        let Some(m) = &self.modulation else {
            return Some("Enable the modulation plugin and connect it".into());
        };
        let Some(p) = &self.photodiode else {
            return Some("Enable the photodiode plugin and connect it".into());
        };
        if !matches!(m.connection, ConnectionStateV1::Connected { .. })
            || !matches!(p.connection, ConnectionStateV1::Connected { .. })
        {
            return Some("Connect the modulation and photodiode plugins".into());
        }
        if m.freshness.is_stale_at(now) || p.freshness.is_stale_at(now) {
            return Some("Wait for fresh device status".into());
        }
        if m.lease.is_some() || p.lease.is_some() || p.active_recording.is_some() {
            return Some("Finish the previous experiment before starting A4".into());
        }
        if m.optical_lobe
            .as_ref()
            .is_none_or(|l| l.calibration_id.is_empty() || l.v_null_dac == l.v_peak_dac)
        {
            return Some("Apply the measured optical transfer calibration first".into());
        }
        None
    }
    pub fn prepare(&mut self, run: &str, now: u64) -> Result<(), String> {
        if let Some(reason) = self.blocker(now) {
            return Err(reason);
        }
        self.run = run.to_owned();
        self.active = true;
        self.closing = false;
        self.renewed = now;
        self.error = None;
        self.receipts.clear();
        let m = self.modulation.as_ref().unwrap();
        self.mod_owner = Some(m.owner_instance.clone());
        self.pd_owner = Some(self.photodiode.as_ref().unwrap().owner_instance.clone());
        let lobe = m.optical_lobe.as_ref().unwrap();
        let code = constant_code(lobe, 0.30)?;
        self.queue.extend([
            Command::Mod(ModulationCommandV1::AcquireLease { ttl_ms: 60_000 }),
            Command::Pd(PhotodiodeCommandV1::AcquireLease { ttl_ms: 60_000 }),
            Command::Mod(ModulationCommandV1::PrepareA1),
            Command::Mod(ModulationCommandV1::SetWaveform {
                waveform: WaveformV1::Constant { level_dac: code },
            }),
        ]);
        Ok(())
    }
    pub fn begin_pd(&mut self, root: String, folder: &str, stem: &str, seconds: f64) {
        self.expected_seconds = seconds;
        let mut metadata = BTreeMap::new();
        metadata.insert("experiment".into(), "A4".into());
        metadata.insert(
            "optical_condition".into(),
            "constant mean_u=0.30; unchanged external attenuation".into(),
        );
        metadata.insert(
            "brightness_axis".into(),
            "camera_reported_lux; absolute accuracy unmeasured".into(),
        );
        let specification = PdqStartSpecV1 {
            pdq_path: format!("{folder}/{stem}.pdq"),
            sidecar_path: format!("{folder}/{stem}.pd.json"),
            expected_sample_rate_hz: None,
            expected_stream_epoch: self.photodiode.as_ref().map(|p| p.stream.stream_epoch),
            metadata,
            root_dir: Some(root),
        };
        self.expected_pd = Some(specification.clone());
        self.queue
            .push_back(Command::Pd(PhotodiodeCommandV1::BeginRecording {
                specification,
            }));
    }
    pub fn finish_pd(&mut self, stopped: bool) {
        self.queue
            .push_back(Command::Pd(PhotodiodeCommandV1::FinalizeRecording {
                termination: if stopped {
                    PdqTerminationV1::OperatorStopped
                } else {
                    PdqTerminationV1::Completed
                },
            }));
    }
    pub fn release(&mut self) {
        // Keep a pending command until its outcome is known. The release commands
        // then finalize any PD capture and turn off the optical output.
        self.closing = true;
        self.queue.clear();
        self.pending = None;
        self.error = None;
        if self.pd_owned {
            self.queue
                .push_back(Command::Pd(PhotodiodeCommandV1::ReleaseLease {
                    finalize_recording: true,
                    reason: "A4 finished".into(),
                }));
        }
        if self.mod_owned {
            self.queue
                .push_back(Command::Mod(ModulationCommandV1::ReleaseLease {
                    safe_off: true,
                    reason: "A4 finished".into(),
                }));
        }
    }
    pub fn released(&self) -> bool {
        self.ready() && !self.mod_owned && !self.pd_owned
    }
    fn next_id(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    fn request(&mut self, command: Command, now: u64) -> PluginServiceRequest {
        let id = self.next_id();
        let mod_revision = self
            .modulation
            .as_ref()
            .and_then(|s| s.requested.as_ref())
            .map_or(0, |t| t.revision.0);
        let pd_revision = self
            .photodiode
            .as_ref()
            .and_then(|s| s.requested_revision)
            .map_or(0, |r| r.0);
        self.revision = self
            .revision
            .max(mod_revision)
            .max(pd_revision)
            .max(now)
            .saturating_add(1);
        let revision = SemanticRevision(self.revision);
        let lease = Some(LeaseId::new(format!("{}-devices", self.run)));
        let run = Some(RunId::new(&self.run));
        let (target, service, payload) = match command {
            Command::Mod(command) => {
                let query = matches!(command, ModulationCommandV1::QueryRequest { .. });
                let mut e = ModulationRequestV1::new(RequestId(id), ClientId::new(CLIENT), command);
                e.lease_id = lease;
                e.run_id = run;
                e.target_owner_instance = self.mod_owner.clone();
                e.issued_at_unix_ms = now;
                if !query {
                    e.requested_revision = Some(revision);
                }
                (
                    MOD,
                    SERVICE_STAGE_A_MODULATION_CONTROL_V1,
                    serde_json::to_value(e).unwrap(),
                )
            }
            Command::Pd(command) => {
                let mut e = PhotodiodeRequestV1::new(RequestId(id), ClientId::new(CLIENT), command);
                e.lease_id = lease;
                e.run_id = run;
                e.target_owner_instance = self.pd_owner.clone();
                e.issued_at_unix_ms = now;
                e.requested_revision = Some(revision);
                (
                    PD,
                    SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
                    serde_json::to_value(e).unwrap(),
                )
            }
        };
        PluginServiceRequest {
            request_id: id,
            source_plugin_id: CLIENT.into(),
            target_plugin_id: target.into(),
            service: service.into(),
            payload,
        }
    }
    pub fn update(&mut self, inbox: &PluginControlInbox) {
        for s in &inbox.snapshots {
            if s.plugin_id == MOD && s.topic == CTX_STAGE_A_MODULATION_STATE_V1 {
                if let Ok(value) = serde_json::from_value(s.payload.clone()) {
                    self.modulation = Some(value);
                }
            }
            if s.plugin_id == PD && s.topic == CTX_STAGE_A_PHOTODIODE_SUMMARY_V1 {
                if let Ok(value) = serde_json::from_value(s.payload.clone()) {
                    self.photodiode = Some(value);
                }
            }
        }
        for reply in &inbox.service_replies {
            if let Some(p) = &self.pending {
                if reply.request_id == p.request.request_id {
                    match &reply.outcome {
                        PluginServiceOutcome::Accepted { payload } => self.accept(payload.clone()),
                        PluginServiceOutcome::Rejected { code, message } => {
                            self.error = Some(format!("{code}: {message}"))
                        }
                    }
                } else if let PluginServiceOutcome::Accepted { payload } = &reply.outcome {
                    // QueryRequest replies retain the original operation ID.
                    self.accept(payload.clone());
                }
            }
        }
        if let Some(response) = self
            .modulation
            .as_ref()
            .and_then(|m| m.last_response.as_ref())
        {
            self.accept(serde_json::to_value(response).unwrap());
        }
        if let Some(response) = self
            .photodiode
            .as_ref()
            .and_then(|m| m.last_response.as_ref())
        {
            self.accept(serde_json::to_value(response).unwrap());
        }
    }
    fn accept(&mut self, payload: Value) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let Ok(common) = serde_json::from_value::<ResponseCommonV1>(payload.clone()) else {
            return;
        };
        let owner = match pending.command {
            Command::Mod(_) => &self.mod_owner,
            Command::Pd(_) => &self.pd_owner,
        };
        if common.request_id.0 != pending.request.request_id
            || owner.as_ref() != Some(&common.owner_instance)
            || common.run_id.as_ref().map(|r| r.as_str()) != Some(self.run.as_str())
        {
            return;
        }
        if common.outcome == RequestOutcomeV1::InProgress {
            return;
        }
        self.receipts.push(payload.clone());
        let needs_revision = matches!(
            pending.command,
            Command::Mod(ModulationCommandV1::PrepareA1 | ModulationCommandV1::SetWaveform { .. })
                | Command::Pd(
                    PhotodiodeCommandV1::BeginRecording { .. }
                        | PhotodiodeCommandV1::FinalizeRecording { .. }
                )
        );
        if common.outcome == RequestOutcomeV1::Applied
            && needs_revision
            && common.acknowledged_revision.map(|r| r.0)
                != pending.request.payload["requested_revision"].as_u64()
        {
            self.error =
                Some("Device did not acknowledge the requested configuration revision".into());
            return;
        }
        if common.outcome != RequestOutcomeV1::Applied {
            self.error = Some(format!("Device operation failed: {:?}", common.error));
            return;
        }
        match &pending.command {
            Command::Mod(ModulationCommandV1::AcquireLease { .. }) => self.mod_owned = true,
            Command::Pd(PhotodiodeCommandV1::AcquireLease { .. }) => self.pd_owned = true,
            Command::Mod(ModulationCommandV1::ReleaseLease { .. }) => self.mod_owned = false,
            Command::Pd(PhotodiodeCommandV1::ReleaseLease { .. }) => self.pd_owned = false,
            Command::Pd(PhotodiodeCommandV1::BeginRecording { specification }) => {
                let receipt = serde_json::from_value::<PhotodiodeResponseV1>(payload.clone())
                    .ok()
                    .and_then(|r| r.receipt);
                if !matches!(receipt, Some(PdqReceiptV1::Started(ref r)) if r.pdq_path == specification.pdq_path && r.sidecar_path == specification.sidecar_path)
                {
                    self.error = Some("PD start did not confirm the requested files".into());
                    return;
                }
            }
            Command::Pd(PhotodiodeCommandV1::FinalizeRecording { termination }) => {
                let receipt = serde_json::from_value::<PhotodiodeResponseV1>(payload.clone())
                    .ok()
                    .and_then(|r| r.receipt);
                if *termination == PdqTerminationV1::Completed
                    && !matches!(receipt, Some(PdqReceiptV1::Finalized(ref r)) if r.valid && r.integrity.is_clean() && r.sample_frames_written > 0 && r.file_size_bytes > 0
                    && self.expected_pd.as_ref().is_some_and(|s| s.pdq_path == r.pdq_path && s.sidecar_path == r.sidecar_path)
                    && r.sample_rate_hz.zip(r.sample_range).is_some_and(|(rate,range)| rate > 0 && range.sample_count as f64 / f64::from(rate) >= self.expected_seconds * 0.9))
                {
                    self.error = Some(
                        "PD recording is incomplete or has stream errors; retain it for review"
                            .into(),
                    );
                    return;
                }
            }
            _ => {}
        }
        self.pending = None;
    }
    pub fn tick(&mut self, now: u64) -> Option<PluginServiceRequest> {
        if self.error.is_some() {
            return None;
        }
        if let Some(p) = self.pending.as_mut() {
            if now.saturating_sub(p.sent) > 20_000 {
                self.error =
                    Some("Device command timed out; acquisition stopped at this point".into());
                return None;
            }
            if matches!(p.command, Command::Mod(_)) && now.saturating_sub(p.poll) >= 250 {
                p.poll = now;
                let original = p.request.request_id;
                return Some(self.request(
                    Command::Mod(ModulationCommandV1::QueryRequest {
                        request_id: RequestId(original),
                    }),
                    now,
                ));
            }
            return None;
        }
        if self.active && !self.closing && now.saturating_sub(self.renewed) >= 15_000 {
            self.renewed = now;
            if self.mod_owned {
                self.queue
                    .push_front(Command::Mod(ModulationCommandV1::RenewLease {
                        ttl_ms: 60_000,
                    }));
            }
            if self.pd_owned {
                self.queue
                    .push_front(Command::Pd(PhotodiodeCommandV1::RenewLease {
                        ttl_ms: 60_000,
                    }));
            }
        }
        let command = self.queue.pop_front()?;
        // An unanswered acquire may already own the lease. Cleanup must attempt
        // release even when its acknowledgement was lost.
        match command {
            Command::Mod(ModulationCommandV1::AcquireLease { .. }) => self.mod_owned = true,
            Command::Pd(PhotodiodeCommandV1::AcquireLease { .. }) => self.pd_owned = true,
            _ => {}
        }
        let request = self.request(command.clone(), now);
        self.pending = Some(Pending {
            command,
            request: request.clone(),
            sent: now,
            poll: now,
        });
        Some(request)
    }
}
fn constant_code(lobe: &OpticalLobeStateV1, mean: f64) -> Result<u16, String> {
    let span = f64::from(lobe.v_peak_dac) - f64::from(lobe.v_null_dac);
    let value = f64::from(lobe.v_null_dac) + 2.0 * span / std::f64::consts::PI * mean.sqrt().asin();
    if !value.is_finite() || !(0.0..=4095.0).contains(&value) {
        return Err("Invalid calibrated constant-light DAC code".into());
    }
    Ok(value.round() as u16)
}

#[cfg(test)]
impl Devices {
    pub(crate) fn simulated(now: u64) -> Self {
        use serde_json::json;
        let common = json!({"contract_version":1,"service_revision":1,"connection":{"state":"connected","port_label":"test","firmware_version":"test"},
            "lease":null,"active_run_id":null,"requested_revision":null,"acknowledged_revision":null,
            "synchronization":{"state":"unsynced","reason":"no_lease","detail":null},
            "last_response":null,"freshness":{"observed_at_unix_ms":now,"valid_for_ms":60000}});
        let mut m = common.clone();
        m["owner_instance"] = json!("mod-instance");
        m["capabilities"] = json!([]);
        m["controller_state"] = json!("configured");
        m["requested"] = Value::Null;
        m["acknowledged"] = Value::Null;
        m["optical_lobe"] = json!({"calibration_id":"test","v_null_dac":100,"v_peak_dac":1100});
        let mut p = common;
        p["owner_instance"] = json!("pd-instance");
        p["stream"] = json!({"stream_epoch":1,"sample_range":null,"sample_rate_hz":500000,"latest_adc_code":1000,"integrity":StreamIntegrityV1::default()});
        p["active_recording"] = Value::Null;
        p["last_finalized_recording"] = Value::Null;
        p["optical_summary"] = Value::Null;
        Self {
            modulation: Some(serde_json::from_value(m).unwrap()),
            photodiode: Some(serde_json::from_value(p).unwrap()),
            ..Self::default()
        }
    }
    pub(crate) fn simulate_reply(&mut self, request: &PluginServiceRequest) {
        use serde_json::json;
        let mut receipt = Value::Null;
        if request.target_plugin_id == PD {
            let e: PhotodiodeRequestV1 = serde_json::from_value(request.payload.clone()).unwrap();
            match e.command {
                PhotodiodeCommandV1::BeginRecording { specification: s } => {
                    let root = std::path::Path::new(s.root_dir.as_ref().unwrap());
                    std::fs::write(root.join(&s.pdq_path), b"simulated-pdq").unwrap();
                    std::fs::write(root.join(&s.sidecar_path), b"{}").unwrap();
                    receipt = json!({"kind":"started","run_id":self.run,"pdq_path":s.pdq_path,"sidecar_path":s.sidecar_path,
                        "opened_at_unix_ms":1000,"stream_epoch":1,"first_sample_index":0});
                }
                PhotodiodeCommandV1::FinalizeRecording { termination } => {
                    receipt = json!({"kind":"finalized","run_id":self.run,"pdq_path":self.expected_pd.as_ref().unwrap().pdq_path,"sidecar_path":self.expected_pd.as_ref().unwrap().sidecar_path,
                        "opened_at_unix_ms":1000,"finalized_at_unix_ms":121000,"file_size_bytes":13,"sha256":"a".repeat(64),
                        "frames_written":1,"sample_frames_written":1,"sample_range":{"first_sample_index":0,"end_sample_index_exclusive":60000000,"sample_count":60000000},"sample_rate_hz":500000,
                        "segment_count":1,"integrity":StreamIntegrityV1::default(),"termination":termination,"valid":true});
                }
                _ => {}
            }
        }
        let payload = json!({"contract_version":1,"request_id":request.request_id,
            "owner_instance":if request.target_plugin_id == MOD {"mod-instance"} else {"pd-instance"},
            "run_id":self.run,"requested_revision":request.payload["requested_revision"],"acknowledged_revision":request.payload["requested_revision"],
            "outcome":"applied","completed_at_unix_ms":1001,"error":null,"receipt":receipt});
        self.accept(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn device() -> Devices {
        Devices {
            active: true,
            run: "A4-test".into(),
            mod_owner: Some(OwnerInstanceId::new("mod-instance")),
            pd_owner: Some(OwnerInstanceId::new("pd-instance")),
            renewed: 1000,
            ..Devices::default()
        }
    }
    fn reply(request: &PluginServiceRequest, receipt: Value) -> Value {
        json!({"contract_version":1,"request_id":request.request_id,
            "owner_instance":if request.target_plugin_id == MOD {"mod-instance"} else {"pd-instance"},
            "run_id":"A4-test","requested_revision":request.payload["requested_revision"],"acknowledged_revision":request.payload["requested_revision"],
            "outcome":"applied","completed_at_unix_ms":1001,"error":null,"receipt":receipt})
    }
    #[test]
    fn a4_commands_are_newer_than_the_previous_experiments_state() {
        let mut d = Devices::simulated(1000);
        d.photodiode.as_mut().unwrap().requested_revision = Some(SemanticRevision(9_000_000));
        d.run = "A4-test".into();
        d.mod_owner = Some(OwnerInstanceId::new("mod-instance"));
        d.queue
            .push_back(Command::Mod(ModulationCommandV1::PrepareA1));
        let request = d.tick(1001).unwrap();
        let e: ModulationRequestV1 = serde_json::from_value(request.payload).unwrap();
        assert!(e.requested_revision.unwrap().0 > 9_000_000);
    }
    #[test]
    fn asynchronous_poll_uses_a_fresh_read_only_request() {
        let mut d = device();
        d.queue
            .push_back(Command::Mod(ModulationCommandV1::PrepareA1));
        let original = d.tick(1000).unwrap();
        let query = d.tick(1250).unwrap();
        assert_ne!(original.request_id, query.request_id);
        let e: ModulationRequestV1 = serde_json::from_value(query.payload).unwrap();
        assert_eq!(
            e.command,
            ModulationCommandV1::QueryRequest {
                request_id: RequestId(original.request_id)
            }
        );
        let mut stale = reply(&original, Value::Null);
        stale["owner_instance"] = json!("old-owner");
        d.accept(stale);
        assert!(!d.ready());
        d.accept(reply(&original, Value::Null));
        assert!(d.ready());
    }
    #[test]
    fn timeout_keeps_uncertain_lease_for_explicit_cleanup() {
        let mut d = device();
        d.queue
            .push_back(Command::Mod(ModulationCommandV1::AcquireLease {
                ttl_ms: 60000,
            }));
        d.tick(1000).unwrap();
        d.tick(22000);
        assert!(d.error.is_some());
        assert!(d.mod_owned);
        d.release();
        let request = d.tick(22001).unwrap();
        let e: ModulationRequestV1 = serde_json::from_value(request.payload.clone()).unwrap();
        assert!(matches!(
            e.command,
            ModulationCommandV1::ReleaseLease { safe_off: true, .. }
        ));
        d.accept(reply(&request, Value::Null));
        assert!(d.released());
    }
    #[test]
    fn leases_renew_during_long_recording_and_pause() {
        let mut d = device();
        d.mod_owned = true;
        d.pd_owned = true;
        let pd = d.tick(17000).unwrap();
        let e: PhotodiodeRequestV1 = serde_json::from_value(pd.payload.clone()).unwrap();
        assert!(matches!(e.command, PhotodiodeCommandV1::RenewLease { .. }));
        d.accept(reply(&pd, Value::Null));
        let m = d.tick(17001).unwrap();
        let e: ModulationRequestV1 = serde_json::from_value(m.payload).unwrap();
        assert!(matches!(e.command, ModulationCommandV1::RenewLease { .. }));
    }
    #[test]
    fn missing_pd_receipt_does_not_start_a_valid_capture() {
        let mut d = device();
        d.begin_pd("/tmp/data".into(), "A4-test", "p1", 120.0);
        let request = d.tick(1000).unwrap();
        d.accept(reply(&request, Value::Null));
        assert!(d.error.as_deref().unwrap().contains("requested files"));
        assert!(!d.ready());
    }
    #[test]
    fn incomplete_pd_finalization_is_not_success() {
        let mut d = device();
        d.finish_pd(false);
        let request = d.tick(1000).unwrap();
        d.accept(reply(&request, Value::Null));
        assert!(d.error.as_deref().unwrap().contains("incomplete"));
    }
    #[test]
    fn constant_light_uses_the_measured_lobe_in_both_directions() {
        for (start, end) in [(100, 1100), (1100, 100)] {
            let l = OpticalLobeStateV1 {
                calibration_id: "measured".into(),
                v_null_dac: start,
                v_peak_dac: end,
            };
            let code = constant_code(&l, 0.3).unwrap();
            let angle = std::f64::consts::PI * (f64::from(code) - f64::from(start))
                / (2.0 * (f64::from(end) - f64::from(start)));
            assert!((angle.sin().powi(2) - 0.3).abs() < 0.002);
        }
    }
}
