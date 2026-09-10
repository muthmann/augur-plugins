//! Universal Stage-A block orchestrator.
//!
//! The runner owns sequencing only. A1-A6 experiment plugins own their
//! camera state machines and continue to use the modulation and photodiode
//! owner services directly.

use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostContext, HostOutput, Plugin, PluginControlContext,
    PluginFrame, PluginInput, PluginServiceOutcome, PluginServiceReply, PluginServiceRequest,
    SettingItem, SettingKind, SettingsSchema, SettingsSection, StatusEntry,
};
use serde_json::{json, Value};
use stage_a_universal_runner::{parse, ExecuteBlockRequest, Experiment, OpticalStateConfirmation, Plan, SERVICE_EXECUTE_BLOCK_V1};

const ID: &str = "stage-a.universal-runner";

#[derive(Debug, Clone)]
struct ActiveRun {
    plan: Plan,
    block_index: usize,
    measurement_sequence: u32,
    pending_request_id: Option<u64>,
    waiting_for_completion: bool,
    waiting_for_operator: bool,
    waiting_measurement_id: Option<String>,
}

pub struct StageAUniversalRunnerPlugin {
    enabled: bool,
    protocol_path: String,
    measurement_prefix: String,
    start_counter: u64,
    seen_start_counter: Option<u64>,
    request_sequence: u64,
    run: Option<ActiveRun>,
    message: String,
    aod_setting: String,
    camera_lux: String,
    photodiode_level: String,
    confirmed_optical_state: Option<OpticalStateConfirmation>,
}

impl Default for StageAUniversalRunnerPlugin {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol_path: String::new(),
            measurement_prefix: String::new(),
            start_counter: 0,
            seen_start_counter: None,
            request_sequence: 0,
            run: None,
            message: "Select a universal Stage-A protocol".into(),
            aod_setting: String::new(),
            camera_lux: String::new(),
            photodiode_level: String::new(),
            confirmed_optical_state: None,
        }
    }
}

impl StageAUniversalRunnerPlugin {
    fn next_request_id(&mut self) -> u64 {
        self.request_sequence = self.request_sequence.wrapping_add(1);
        self.request_sequence
    }

    fn target(experiment: Experiment) -> &'static str {
        match experiment {
            Experiment::A1 => "stage-a.a1",
            Experiment::A2 => "stage-a.a2",
            Experiment::A3 => "stage-a.a3",
            Experiment::A4 => "stage-a.a4",
            Experiment::A5 => "stage-a.a5",
            Experiment::A6 => "stage-a.a6",
        }
    }

    fn measurement_id(&self, experiment: Experiment, sequence: u32) -> String {
        format!("{}-{}-{:02}", Plan::measurement_prefix(experiment), self.measurement_prefix, sequence)
    }

    fn start(&mut self, context: &mut PluginControlContext<'_>) {
        if self.confirmed_optical_state.is_none() {
            self.message = "Enter AOD, camera lux and photodiode level, then Continue".into();
            return;
        }
        let text = if self.protocol_path.trim().is_empty() {
            include_str!("../../../stage-a-universal-runner/protocols/stage-a-all.toml").to_owned()
        } else {
            match std::fs::read_to_string(self.protocol_path.trim()) {
                Ok(text) => text,
                Err(error) => {
                    self.message = format!("Cannot read universal protocol: {error}");
                    return;
                }
            }
        };
        let plan = match parse(&text) {
            Ok(plan) if !plan.blocks.is_empty() => plan,
            Ok(_) => {
                self.message = "Universal protocol has no blocks".into();
                return;
            }
            Err(error) => {
                self.message = format!("Universal protocol rejected: {error}");
                return;
            }
        };
        self.run = Some(ActiveRun {
            plan,
            block_index: 0,
            measurement_sequence: 1,
            pending_request_id: None,
            waiting_for_completion: false,
            waiting_for_operator: false,
            waiting_measurement_id: None,
        });
        self.message = "Universal Stage-A run started".into();
        self.dispatch_next(context);
    }

    fn dispatch_next(&mut self, context: &mut PluginControlContext<'_>) {
        if self.confirmed_optical_state.is_none() {
            if let Some(run) = self.run.as_mut() { run.waiting_for_operator = true; }
            self.message = "AOD changed; enter readbacks and press Continue before the next block".into();
            return;
        }
        let (experiment, block_name, protocol, camera, plan_name, measurement_sequence) = {
            let Some(run) = self.run.as_ref() else { return };
            let Some(block) = run.plan.blocks.get(run.block_index) else {
                self.message = "Universal Stage-A run complete".into();
                self.run = None;
                return;
            };
            (block.experiment, block.name.clone(), block.protocol.clone(), block.camera.clone(), run.plan.name.clone(), run.measurement_sequence)
        };
        let id = self.next_request_id();
        let measurement_id = self.measurement_id(experiment, measurement_sequence);
        let payload = ExecuteBlockRequest {
            plan_name,
            block_name: block_name.clone(),
            experiment,
            protocol,
            measurement_id,
            camera,
            optical_state: self.confirmed_optical_state.clone(),
        };
        let waiting_measurement_id = payload.measurement_id.clone();
        let _ = context.request_service(&PluginServiceRequest {
            request_id: id,
            source_plugin_id: ID.into(),
            target_plugin_id: Self::target(experiment).into(),
            service: SERVICE_EXECUTE_BLOCK_V1.into(),
            payload: serde_json::to_value(payload).expect("request is serializable"),
        });
        if let Some(run) = self.run.as_mut() {
            run.pending_request_id = Some(id);
            run.waiting_for_completion = true;
            run.waiting_measurement_id = Some(waiting_measurement_id);
        }
        self.message = format!("Waiting for {} block '{}'", Plan::measurement_prefix(experiment), block_name);
    }

    fn service_reply(&mut self, _context: &mut PluginControlContext<'_>, reply: PluginServiceReply) {
        let Some(run) = self.run.as_mut() else { return };
        if run.pending_request_id != Some(reply.request_id) { return; }
        match reply.outcome {
            PluginServiceOutcome::Accepted { .. } => self.message = "Block accepted; waiting for explicit completion".into(),
            PluginServiceOutcome::Rejected { code, message } => {
                self.message = format!("Block refused ({code}): {message}");
                run.pending_request_id = None;
            }
        }
    }
}

impl Plugin for StageAUniversalRunnerPlugin {
    fn name(&self) -> &'static str { "Stage-A Universal Runner" }
    fn description(&self) -> &'static str { "Sequences A1-A6 blocks while experiment plugins own hardware." }
    fn enabled(&self) -> bool { self.enabled }
    fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }
    fn reset(&mut self) { self.run = None; }
    fn input_kind(&self) -> PluginInput { PluginInput::RawEvents }
    fn process_frame(&mut self, _frame: &PluginFrame<'_>, _output: &mut HostOutput<'_>, _context: &mut HostContext<'_>, _event_store: &EventStoreHandle<'_>) {}
    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        for reply in context.inbox().service_replies.clone() { self.service_reply(context, reply); }
        if self.confirmed_optical_state.is_some() && self.run.as_ref().is_some_and(|run| run.waiting_for_operator) {
            self.dispatch_next(context);
        }
        let completed = context.inbox().snapshots.iter().any(|snapshot| {
            snapshot.topic == "stage-a.universal.block"
                && snapshot.payload.get("state").and_then(Value::as_str) == Some("completed")
                && self.run.as_ref().and_then(|run| run.waiting_measurement_id.as_deref())
                    == snapshot.payload.get("measurement_id").and_then(Value::as_str)
        });
        if completed {
            if let Some(run) = self.run.as_mut() {
                if run.waiting_for_completion {
                    run.block_index += 1;
                    run.measurement_sequence += 1;
                    run.pending_request_id = None;
                    run.waiting_for_completion = false;
                    run.waiting_for_operator = false;
                    run.waiting_measurement_id = None;
                    self.dispatch_next(context);
                }
            }
        }
        if self.start_counter != self.seen_start_counter.unwrap_or(self.start_counter) {
            self.seen_start_counter = Some(self.start_counter);
            if self.run.is_none() { self.start(context); }
        }
    }
    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema { sections: vec![SettingsSection { label: "Universal Stage-A".into(), description: Some("Sequence existing A1-A6 experiment owners from one protocol.".into()), default_open: true, items: vec![
            SettingItem { key: "protocol_path".into(), label: "Protocol file (blank = built-in A1-A5)".into(), tooltip: None, kind: SettingKind::Path { dialog: augur_plugin_api::PathDialogKind::OpenFile, default: self.protocol_path.clone() } },
            SettingItem { key: "measurement_prefix".into(), label: "Measurement prefix".into(), tooltip: None, kind: SettingKind::Text { default: self.measurement_prefix.clone() } },
            SettingItem { key: "aod_setting".into(), label: "AOD setting".into(), tooltip: Some("Control value only; not a flux measurement.".into()), kind: SettingKind::Text { default: self.aod_setting.clone() } },
            SettingItem { key: "camera_lux".into(), label: "Camera lux (readback)".into(), tooltip: None, kind: SettingKind::Text { default: self.camera_lux.clone() } },
            SettingItem { key: "photodiode_level".into(), label: "Photodiode level (readback)".into(), tooltip: None, kind: SettingKind::Text { default: self.photodiode_level.clone() } },
            SettingItem { key: "confirm_optical_state".into(), label: "Continue: confirm optical state".into(), tooltip: Some("Required after every AOD change.".into()), kind: SettingKind::Button { enabled: true } },
            SettingItem { key: "start".into(), label: "Run universal protocol".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
        ] }] }
    }
    fn get_setting(&self, key: &str) -> Option<Value> { match key { "protocol_path" => Some(json!(self.protocol_path)), "measurement_prefix" => Some(json!(self.measurement_prefix)), "aod_setting" => Some(json!(self.aod_setting)), "camera_lux" => Some(json!(self.camera_lux)), "photodiode_level" => Some(json!(self.photodiode_level)), "confirm_optical_state" => Some(json!(0)), "start" => Some(json!(self.start_counter)), _ => None } }
    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "protocol_path" => { self.protocol_path = value.as_str().ok_or("protocol_path must be a string")?.into(); Ok(()) }
            "measurement_prefix" => { self.measurement_prefix = value.as_str().ok_or("measurement_prefix must be a string")?.into(); Ok(()) }
            "aod_setting" => { self.aod_setting = value.as_str().ok_or("aod_setting must be text")?.into(); self.confirmed_optical_state = None; Ok(()) }
            "camera_lux" => { self.camera_lux = value.as_str().ok_or("camera_lux must be text")?.into(); self.confirmed_optical_state = None; Ok(()) }
            "photodiode_level" => { self.photodiode_level = value.as_str().ok_or("photodiode_level must be text")?.into(); self.confirmed_optical_state = None; Ok(()) }
            "confirm_optical_state" => {
                if self.aod_setting.trim().is_empty() || self.camera_lux.trim().is_empty() || self.photodiode_level.trim().is_empty() { return Err("enter AOD, camera lux and photodiode level before Continue".into()); }
                self.confirmed_optical_state = Some(OpticalStateConfirmation { aod_setting: self.aod_setting.clone(), camera_lux: self.camera_lux.clone(), photodiode_level: self.photodiode_level.clone(), confirmed_at_utc: format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|_| "system clock before epoch")?.as_secs()) });
                self.message = "Optical state confirmed; press Run universal protocol".into(); Ok(())
            }
            "start" => { if let Some(counter) = value.as_u64() { self.start_counter = counter; } else if value.as_bool() == Some(true) { self.start_counter = self.start_counter.wrapping_add(1); } else { return Err("start must be a button counter".into()); } Ok(()) }
            _ => Err(format!("unknown setting: {key}")),
        }
    }
    fn status_entries(&self) -> Vec<StatusEntry> { vec![StatusEntry::Text(self.message.clone())] }
}

export_plugin!(StageAUniversalRunnerPlugin);
