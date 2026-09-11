//! Universal Stage-A block orchestrator.
//!
//! The runner owns sequencing only. A1-A6 experiment plugins own their
//! camera state machines and continue to use the modulation and photodiode
//! owner services directly.

use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostContext, HostOutput, Plugin, PluginControlContext,
    PluginControlInbox, PluginFrame, PluginInput, PluginServiceOutcome, PluginServiceReply,
    PluginServiceRequest, SensorMonitoringV1, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, CTX_SENSOR_MONITORING,
};
use serde_json::{json, Value};
use stage_a_universal_runner::{
    parse, ExecuteBlockRequest, Experiment, OpticalStateConfirmation, Plan,
    SERVICE_EXECUTE_BLOCK_V1,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

const ID: &str = "stage-a.universal-runner";

trait RunnerControl {
    fn request_service(&mut self, request: &PluginServiceRequest) -> Result<(), String>;
}
impl RunnerControl for PluginControlContext<'_> {
    fn request_service(&mut self, request: &PluginServiceRequest) -> Result<(), String> {
        PluginControlContext::request_service(self, request)
    }
}

#[derive(Debug, Clone)]
struct ActiveRun {
    plan: Plan,
    block_index: usize,
    measurement_sequence: u32,
    pending_request_id: Option<u64>,
    waiting_for_completion: bool,
    waiting_for_operator: bool,
    waiting_measurement_id: Option<String>,
    current_optical_state: Option<String>,
    block_started_at: Instant,
    started_at: Instant,
    started_unix_ms: u64,
    omitted: BTreeSet<usize>,
    completed: BTreeSet<usize>,
    attempts: BTreeMap<usize, u32>,
    preflight: BTreeMap<u64, String>,
    preflight_finished: bool,
    frozen_selected: Option<stage_a_universal_runner::CameraSettings>,
    frozen_finalists: Option<[stage_a_universal_runner::CameraSettings; 2]>,
    stop_sent: bool,
    user_stop: bool,
    error: Option<String>,
    reference_id: Option<String>,
    reference_request_id: Option<u64>,
    reference_ready: bool,
    reference_releasing: bool,
}

pub struct StageAUniversalRunnerPlugin {
    enabled: bool,
    protocol_path: String,
    resume_checkpoint: String,
    last_checkpoint_key: String,
    program: String,
    measurement_prefix: String,
    start_press: Press,
    continue_press: Press,
    stop_press: Press,
    start_pending: bool,
    continue_pending: bool,
    stop_pending: bool,
    selected_candidate: String,
    finalist_1: String,
    finalist_2: String,
    request_sequence: u64,
    run: Option<ActiveRun>,
    message: String,
    aod_setting: String,
    camera_lux: Option<f32>,
    photodiode_level: Option<f64>,
    confirmed_optical_state: Option<OpticalStateConfirmation>,
    output_folder: String,
    modulation_connected: bool,
    modulation_calibrated: bool,
    photodiode_connected: bool,
    photodiode_data_dir: Option<String>,
}

impl Default for StageAUniversalRunnerPlugin {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol_path: String::new(),
            resume_checkpoint: String::new(),
            last_checkpoint_key: String::new(),
            program: "selected_state_final".into(),
            measurement_prefix: String::new(),
            start_press: Press::default(),
            continue_press: Press::default(),
            stop_press: Press::default(),
            start_pending: false,
            continue_pending: false,
            stop_pending: false,
            selected_candidate: String::new(),
            finalist_1: String::new(),
            finalist_2: String::new(),
            request_sequence: 0,
            run: None,
            message: "Select a universal Stage-A protocol".into(),
            aod_setting: String::new(),
            camera_lux: None,
            photodiode_level: None,
            confirmed_optical_state: None,
            output_folder: String::new(),
            modulation_connected: false,
            modulation_calibrated: false,
            photodiode_connected: false,
            photodiode_data_dir: None,
        }
    }
}

impl StageAUniversalRunnerPlugin {
    fn drive_control(&mut self, inbox: &PluginControlInbox, context: &mut impl RunnerControl) {
        for snapshot in &inbox.snapshots {
            if snapshot.topic == "stage_a.modulation_state.v1" {
                self.modulation_connected = Self::connection_ready(&snapshot.payload);
                self.modulation_calibrated = snapshot
                    .payload
                    .get("optical_lobe")
                    .is_some_and(|v| !v.is_null());
            }
            if snapshot.topic == "stage_a.photodiode_summary.v1" {
                self.photodiode_connected = Self::connection_ready(&snapshot.payload);
                self.photodiode_data_dir = snapshot
                    .payload
                    .get("data_dir")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.photodiode_level = snapshot
                    .payload
                    .pointer("/stream/level/mean_volts")
                    .and_then(Value::as_f64);
            }
        }
        if self.start_pending || (self.continue_pending && self.run.is_none()) {
            self.start_pending = false;
            self.continue_pending = false;
            if self.run.is_none() {
                self.start(context);
            }
        }
        for reply in inbox.service_replies.clone() {
            self.service_reply(context, reply);
        }
        if let Some(run) = self.run.as_mut() {
            if !run.preflight_finished
                && !run.preflight.is_empty()
                && run.block_started_at.elapsed().as_secs() > 15
            {
                run.error = Some(format!(
                    "Owner readiness timed out: {}",
                    run.preflight
                        .values()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                self.message = run.error.clone().unwrap();
            }
        }
        let snapshots = inbox.snapshots.clone();
        for snapshot in snapshots {
            if snapshot.topic == "stage-a.universal.reference" && snapshot.plugin_id == "stage-a.a4"
            {
                if let Some(run) = self.run.as_mut() {
                    if run.reference_id.is_some()
                        && snapshot.payload.get("reference_id").and_then(Value::as_str)
                            == run.reference_id.as_deref()
                    {
                        match snapshot.payload.get("state").and_then(Value::as_str) {
                            Some("ready") => {
                                run.reference_ready = true;
                            }
                            Some("released") => {
                                run.reference_id = None;
                                run.reference_ready = false;
                                run.reference_releasing = false;
                                run.waiting_for_operator = false;
                            }
                            Some("failed") => {
                                run.error = Some(format!(
                                    "Optical reference cleanup needs attention: {}",
                                    snapshot.payload
                                ));
                                self.message = run.error.clone().unwrap();
                            }
                            _ => {}
                        }
                    }
                }
                continue;
            }
            let matches = self.run.as_ref().is_some_and(|run| {
                run.waiting_for_completion
                    && snapshot.topic == "stage-a.universal.block"
                    && snapshot.plugin_id
                        == Self::target(run.plan.blocks[run.block_index].experiment)
                    && snapshot
                        .payload
                        .get("attempt")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                        == u64::from(run.attempts.get(&run.block_index).copied().unwrap_or(0))
                    && snapshot
                        .payload
                        .get("measurement_id")
                        .and_then(Value::as_str)
                        == run.waiting_measurement_id.as_deref()
            });
            if !matches {
                continue;
            }
            let state = snapshot
                .payload
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("");
            if matches!(state, "completed" | "failed" | "aborted" | "rejected") {
                self.journal(state, snapshot.payload.clone());
                let run = self.run.as_mut().unwrap();
                run.waiting_for_completion = false;
                run.pending_request_id = None;
                run.waiting_measurement_id = None;
                let mut advance = true;
                if state == "completed" {
                    run.completed.insert(run.block_index);
                } else if !run.stop_sent {
                    let attempts = run.attempts.entry(run.block_index).or_default();
                    if *attempts < 2 && run.plan.blocks[run.block_index].retry_same_point {
                        *attempts += 1;
                        advance = false;
                        self.message = format!(
                            "Retrying unfinished block after owner cleanup, attempt {} of 3",
                            *attempts + 1
                        );
                    } else {
                        advance = false;
                        run.error = Some(format!(
                            "Block failed after bounded retries: {}",
                            snapshot.payload
                        ));
                        self.message = run.error.clone().unwrap();
                    }
                } else {
                    run.omitted.insert(run.block_index);
                }
                if advance {
                    run.block_index += 1;
                    run.measurement_sequence += 1;
                }
                if run.user_stop {
                    self.checkpoint();
                    self.message =
                        "Stopped after owner finalization; incomplete campaign and files retained"
                            .into();
                    self.run = None;
                    break;
                }
                run.stop_sent = false;
            }
        }
        if self.stop_pending {
            self.stop_pending = false;
            if let Some(run) = self.run.as_mut() {
                run.user_stop = true;
            }
        }
        let stop = self.run.as_ref().is_some_and(|run| {
            let deadline = run.plan.blocks.get(run.block_index).and_then(|b| {
                if b.closing {
                    run.plan.wall_clock_limit_s
                } else {
                    run.plan.acquisition_cutoff_s
                }
            });
            run.waiting_for_completion
                && !run.stop_sent
                && (run.user_stop
                    || deadline.is_some_and(|s| run.started_at.elapsed().as_secs() >= s))
        });
        if stop {
            let id = self.next_request_id();
            let run = self.run.as_mut().unwrap();
            run.stop_sent = true;
            let _ = context.request_service(&PluginServiceRequest {
                request_id: id,
                source_plugin_id: ID.into(),
                target_plugin_id: Self::target(run.plan.blocks[run.block_index].experiment).into(),
                service: stage_a_universal_runner::SERVICE_STOP_V1.into(),
                payload: json!({"measurement_id":run.waiting_measurement_id}),
            });
            self.message =
                "Stopping active owner; waiting for finalized files and restoration".into();
        }
        if self.run.as_ref().is_some_and(|r| {
            r.reference_id.is_some()
                && !r.reference_releasing
                && (r.user_stop
                    || r.plan
                        .blocks
                        .get(r.block_index)
                        .and_then(|b| {
                            if b.closing {
                                r.plan.wall_clock_limit_s
                            } else {
                                r.plan.acquisition_cutoff_s
                            }
                        })
                        .is_some_and(|s| r.started_at.elapsed().as_secs() >= s))
        }) {
            self.release_reference(context);
        }
        if self
            .run
            .as_ref()
            .is_some_and(|r| r.user_stop && !r.waiting_for_completion && r.reference_id.is_none())
        {
            self.journal("stopped", json!({}));
            self.checkpoint();
            self.run = None;
            self.message = "Campaign stopped; files retained".into();
        }
        if self.continue_pending {
            self.continue_pending = false;
            let mut release = false;
            if let Some(run) = self.run.as_mut() {
                if run.waiting_for_operator && run.preflight_finished && run.error.is_none() {
                    let selection = run.frozen_finalists.is_none()
                        && run.plan.blocks.get(run.block_index).is_some_and(|b| {
                            matches!(b.camera_from.as_deref(), Some("finalist_1" | "finalist_2"))
                        });
                    if selection {
                        match (
                            stage_a_universal_runner::candidate_camera(&self.finalist_1),
                            stage_a_universal_runner::candidate_camera(&self.finalist_2),
                        ) {
                            (Ok(a), Ok(b)) if self.finalist_1 != self.finalist_2 => {
                                run.frozen_finalists = Some([a, b]);
                                run.waiting_for_operator = false;
                            }
                            _ => {
                                self.message = "Enter two distinct measured finalists B0–B5".into()
                            }
                        }
                    } else if run.reference_ready && !run.reference_releasing {
                        if let (Some(lux), Some(pd)) = (self.camera_lux, self.photodiode_level) {
                            if lux.is_finite()
                                && pd.is_finite()
                                && !self.aod_setting.trim().is_empty()
                            {
                                let state = run.plan.blocks[run.block_index]
                                    .optical_state
                                    .clone()
                                    .unwrap_or_default();
                                self.confirmed_optical_state = Some(OpticalStateConfirmation {
                                    state_id: state.clone(),
                                    aod_setting: self.aod_setting.clone(),
                                    camera_lux: lux.to_string(),
                                    photodiode_level: pd.to_string(),
                                    confirmed_at_utc: (unix_ms() / 1000).to_string(),
                                });
                                run.current_optical_state = Some(state);
                                release = true;
                            } else {
                                self.message =
                                    "Wait for finite readbacks and enter the AOD setting".into();
                            }
                        } else {
                            self.message = "Camera lux or photodiode readback is missing".into();
                        }
                    } else {
                        self.message="Wait until the constant optical reference is ready before confirming the AOD".into();
                    }
                }
            }
            if release {
                self.release_reference(context);
            }
        }
        if self.run.as_ref().is_some_and(|run| {
            run.preflight_finished
                && !run.waiting_for_completion
                && !run.waiting_for_operator
                && run.error.is_none()
        }) {
            self.dispatch_next(context);
        }
        self.checkpoint();
    }
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
        format!(
            "{}-{}-{:02}",
            Plan::measurement_prefix(experiment),
            self.measurement_prefix,
            sequence
        )
    }

    fn selected_plan(&self) -> Result<Plan, String> {
        let text = if self.protocol_path.trim().is_empty() {
            match self.program.trim() {
                "smoke" => include_str!(
                    "../../../stage-a-universal-runner/protocols/stage-a-smoke-test.toml"
                ),
                "bright_reference" => {
                    include_str!("../../../stage-a-universal-runner/protocols/stage-a-all.toml")
                }
                "dim_bias_selection" => include_str!(
                    "../../../stage-a-universal-runner/protocols/stage-a-dim-bias-selection.toml"
                ),
                "selected_state_final" => include_str!(
                    "../../../stage-a-universal-runner/protocols/stage-a-grand-final.toml"
                ),
                other => return Err(format!("Unknown built-in program '{other}'")),
            }
            .to_owned()
        } else {
            std::fs::read_to_string(self.protocol_path.trim())
                .map_err(|error| format!("Cannot read universal protocol: {error}"))?
        };
        match parse(&text) {
            Ok(plan) if !plan.blocks.is_empty() => {
                plan.validate()?;
                Ok(plan)
            }
            Ok(_) => Err("Universal protocol has no blocks".into()),
            Err(error) => Err(format!("Universal protocol rejected: {error}")),
        }
    }

    fn format_duration(seconds: f64) -> String {
        let seconds = seconds.max(0.0).round() as u64;
        let hours = seconds / 3600;
        let minutes = (seconds % 3600) / 60;
        let seconds = seconds % 60;
        if hours > 0 {
            format!("{hours} h {minutes:02} min")
        } else if minutes > 0 {
            format!("{minutes} min {seconds:02} s")
        } else {
            format!("{seconds} s")
        }
    }

    fn connection_ready(payload: &Value) -> bool {
        match payload.get("connection") {
            Some(Value::String(value)) => value.eq_ignore_ascii_case("connected"),
            Some(Value::Object(value)) => {
                value
                    .get("state")
                    .and_then(Value::as_str)
                    .is_some_and(|state| state.eq_ignore_ascii_case("connected"))
                    || value.contains_key("connected")
            }
            _ => false,
        }
    }

    fn required_plugins(plan: &Plan) -> Vec<&'static str> {
        let mut plugins = Vec::new();
        for experiment in plan.blocks.iter().map(|block| block.experiment) {
            let plugin = Self::target(experiment);
            if !plugins.contains(&plugin) {
                plugins.push(plugin);
            }
        }
        if plan
            .blocks
            .iter()
            .any(|block| block.optical_state.is_some())
            && !plugins.contains(&"stage-a.a4")
        {
            plugins.push("stage-a.a4");
        }
        for plugin in ["stage-a.modulation", "stage-a.photodiode"] {
            if !plugins.contains(&plugin) {
                plugins.push(plugin);
            }
        }
        plugins
    }

    fn start(&mut self, context: &mut impl RunnerControl) {
        let checkpoint = if self.resume_checkpoint.trim().is_empty() {
            None
        } else {
            match std::fs::read_to_string(self.resume_checkpoint.trim())
                .map_err(|e| e.to_string())
                .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| e.to_string()))
            {
                Ok(value)
                    if value["schema"] == "stage-a.universal.checkpoint.v1"
                        && value["finished"] != true =>
                {
                    self.output_folder = value["output_folder"].as_str().unwrap_or("").into();
                    self.measurement_prefix =
                        value["measurement_prefix"].as_str().unwrap_or("").into();
                    self.selected_candidate =
                        value["selected_candidate"].as_str().unwrap_or("").into();
                    self.finalist_1 = value["finalist_1"].as_str().unwrap_or("").into();
                    self.finalist_2 = value["finalist_2"].as_str().unwrap_or("").into();
                    Some(value)
                }
                Ok(_) => {
                    self.message = "Checkpoint is complete or has an unsupported schema".into();
                    return;
                }
                Err(error) => {
                    self.message = format!("Cannot read checkpoint: {error}");
                    return;
                }
            }
        };
        if self.output_folder.trim().is_empty() {
            self.message = "Choose a common output folder before starting; each measurement gets its own subfolder".into();
            return;
        }
        if !Path::new(self.output_folder.trim()).is_absolute() {
            self.message = "Choose an absolute common output folder".into();
            return;
        }
        if let Err(error) = std::fs::create_dir_all(self.output_folder.trim()) {
            self.message = format!("Cannot create the common output folder: {error}");
            return;
        }
        let saved_plan = checkpoint
            .as_ref()
            .map(|c| serde_json::from_value::<Plan>(c["plan"].clone()).map_err(|e| e.to_string()));
        let plan = match saved_plan.unwrap_or_else(|| self.selected_plan()) {
            Ok(plan) => plan,
            Err(error) => {
                self.message = error;
                return;
            }
        };
        if let Err(error) = plan.validate() {
            self.message = format!("Invalid campaign: {error}");
            return;
        }
        let mut missing = Vec::new();
        if !self.photodiode_connected {
            missing.push("photodiode connection");
        }
        if self.photodiode_data_dir.is_none() {
            missing.push("photodiode data path");
        }
        if !self.modulation_connected {
            missing.push("modulation connection");
        }
        if !self.modulation_calibrated {
            missing.push("modulation calibration");
        }
        if !missing.is_empty() {
            self.message = format!(
                "Universal run blocked before first block — missing {}",
                missing.join(", ")
            );
            return;
        }
        let frozen_selected = if plan
            .blocks
            .iter()
            .any(|b| b.camera_from.as_deref() == Some("selected"))
        {
            match stage_a_universal_runner::candidate_camera(&self.selected_candidate) {
                Ok(camera) => Some(camera),
                Err(error) => {
                    self.message = format!("Choose the measured final bias first: {error}");
                    return;
                }
            }
        } else {
            None
        };
        if self.measurement_prefix.trim().is_empty() {
            self.measurement_prefix = unix_ms().to_string();
        }
        if !self
            .measurement_prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            self.message = "Measurement prefix may contain only letters, digits, - and _".into();
            return;
        }
        let campaign = Path::new(self.output_folder.trim())
            .join(format!("universal-{}", self.measurement_prefix));
        if campaign.exists() && checkpoint.is_none() {
            self.message =
                "This campaign prefix already exists. Choose a new prefix to preserve its records"
                    .into();
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&campaign).and_then(|_| {
            std::fs::write(
                campaign.join("plan.json"),
                serde_json::to_vec_pretty(&plan).expect("plan serializes"),
            )
        }) {
            self.message = format!("Cannot freeze campaign: {error}");
            return;
        }
        self.run = Some(ActiveRun {
            plan,
            started_at: Instant::now(),
            started_unix_ms: unix_ms(),
            omitted: BTreeSet::new(),
            completed: BTreeSet::new(),
            attempts: BTreeMap::new(),
            preflight: BTreeMap::new(),
            preflight_finished: false,
            frozen_selected,
            frozen_finalists: None,
            stop_sent: false,
            user_stop: false,
            error: None,
            reference_id: None,
            reference_request_id: None,
            reference_ready: false,
            reference_releasing: false,
            block_index: 0,
            measurement_sequence: 1,
            pending_request_id: None,
            waiting_for_completion: false,
            waiting_for_operator: false,
            waiting_measurement_id: None,
            current_optical_state: None,
            block_started_at: Instant::now(),
        });
        if let Some(saved) = checkpoint {
            let restored = (|| -> Result<(), String> {
                let run = self.run.as_mut().unwrap();
                run.block_index = saved["block_index"]
                    .as_u64()
                    .ok_or("Missing checkpoint index")? as usize;
                if run.block_index >= run.plan.blocks.len() {
                    return Err("No unfinished blocks remain".into());
                }
                run.completed = serde_json::from_value(saved["completed"].clone())
                    .map_err(|e| e.to_string())?;
                run.omitted =
                    serde_json::from_value(saved["omitted"].clone()).map_err(|e| e.to_string())?;
                run.attempts =
                    serde_json::from_value(saved["attempts"].clone()).map_err(|e| e.to_string())?;
                run.started_unix_ms = saved["started_unix_ms"]
                    .as_u64()
                    .ok_or("Missing original campaign start")?;
                if run.started_unix_ms > unix_ms() {
                    return Err("Clock predates campaign start".into());
                }
                let elapsed = unix_ms() - run.started_unix_ms;
                run.started_at = Instant::now()
                    .checked_sub(std::time::Duration::from_millis(elapsed))
                    .ok_or("Invalid campaign elapsed time")?;
                run.measurement_sequence = saved["measurement_sequence"]
                    .as_u64()
                    .ok_or("Missing sequence")? as u32;
                run.frozen_finalists = serde_json::from_value(saved["frozen_finalists"].clone())
                    .map_err(|e| e.to_string())?;
                if saved["waiting_for_completion"] == true || saved["error"].is_string() {
                    *run.attempts.entry(run.block_index).or_default() += 1;
                }
                if run
                    .plan
                    .wall_clock_limit_s
                    .is_some_and(|s| elapsed >= s * 1000)
                {
                    return Err(
                        "Original campaign deadline has expired; resume cannot reset it".into(),
                    );
                }
                Ok(())
            })();
            if let Err(error) = restored {
                self.message = format!("Cannot resume: {error}");
                self.run = None;
                return;
            }
        }
        self.last_checkpoint_key.clear();
        self.confirmed_optical_state = None;
        let owners = Self::required_plugins(&self.run.as_ref().unwrap().plan);
        for owner in owners
            .into_iter()
            .filter(|p| !matches!(*p, "stage-a.modulation" | "stage-a.photodiode"))
        {
            let request_id = self.next_request_id();
            self.run
                .as_mut()
                .unwrap()
                .preflight
                .insert(request_id, owner.to_owned());
            let _ = context.request_service(&PluginServiceRequest {
                request_id,
                source_plugin_id: ID.into(),
                target_plugin_id: owner.into(),
                service: stage_a_universal_runner::SERVICE_READY_V1.into(),
                payload: json!({}),
            });
        }
        self.message = format!(
            "Started: {} blocks, {} points, estimated {}",
            self.run.as_ref().map_or(0, |run| run.plan.blocks.len()),
            self.run.as_ref().map_or(0, |run| run.plan.display_points()),
            Self::format_duration(
                self.run
                    .as_ref()
                    .map_or(0.0, |run| run.plan.display_seconds())
            )
        );
        self.message = "Checking that every required acquisition plugin is loaded and idle".into();
    }

    fn dispatch_next(&mut self, context: &mut impl RunnerControl) {
        let Some(run) = self.run.as_mut() else { return };
        if !run.preflight_finished || run.waiting_for_completion || run.error.is_some() {
            return;
        }
        let elapsed = run.started_at.elapsed().as_secs_f64();
        if let Some(cutoff) = run.plan.acquisition_cutoff_s {
            let available = (cutoff as f64 - elapsed).max(0.0);
            let mut remaining: f64 = run
                .plan
                .blocks
                .iter()
                .enumerate()
                .skip(run.block_index)
                .filter(|(i, b)| !b.closing && !run.omitted.contains(i))
                .map(|(_, b)| b.seconds(run.plan.overhead_s_per_point) + 20.0)
                .sum();
            for priority in [2, 1] {
                for (i, block) in run.plan.blocks.iter().enumerate().skip(run.block_index) {
                    if remaining <= available {
                        break;
                    }
                    if block.optional_priority == priority && run.omitted.insert(i) {
                        remaining -= block.seconds(run.plan.overhead_s_per_point) + 20.0;
                    }
                }
            }
            while let Some(block) = run.plan.blocks.get(run.block_index) {
                if run.omitted.contains(&run.block_index)
                    || (!block.closing
                        && elapsed + block.seconds(run.plan.overhead_s_per_point) + 20.0
                            > cutoff as f64)
                {
                    run.omitted.insert(run.block_index);
                    run.block_index += 1;
                } else {
                    break;
                }
            }
        }
        if run
            .plan
            .wall_clock_limit_s
            .is_some_and(|limit| elapsed >= limit as f64)
        {
            self.message = "Wall-clock limit reached; campaign incomplete. Verify closing files and restoration".into();
            self.journal("deadline_expired", json!({}));
            self.checkpoint();
            self.run = None;
            return;
        }
        if run
            .plan
            .blocks
            .get(run.block_index)
            .is_some_and(|b| matches!(b.camera_from.as_deref(), Some("finalist_1" | "finalist_2")))
            && run.frozen_finalists.is_none()
        {
            match (
                stage_a_universal_runner::candidate_camera(&self.finalist_1),
                stage_a_universal_runner::candidate_camera(&self.finalist_2),
            ) {
                (Ok(a), Ok(b)) if self.finalist_1 != self.finalist_2 => {
                    run.frozen_finalists = Some([a, b])
                }
                _ => {
                    self.message = "Screen complete. Review response and background, then enter two distinct finalists and press Continue".into();
                    run.waiting_for_operator = true;
                    return;
                }
            }
        }
        self.checkpoint();
        if self
            .run
            .as_ref()
            .is_some_and(|run| run.error.is_some() || run.user_stop)
        {
            return;
        }
        let (experiment, block_name, protocol, camera, plan_name, measurement_sequence) = {
            let Some(run) = self.run.as_ref() else { return };
            let Some(block) = run.plan.blocks.get(run.block_index) else {
                self.message = if run.omitted.is_empty() {
                    "Universal Stage-A run complete; verify backup".into()
                } else {
                    format!(
                        "Campaign ended with {} omitted blocks; see campaign journal",
                        run.omitted.len()
                    )
                };
                self.run = None;
                return;
            };
            (
                block.experiment,
                block.name.clone(),
                block.protocol.clone(),
                {
                    let mut camera = match block.camera_from.as_deref() {
                        Some("selected") => {
                            run.frozen_selected.clone().expect("selected state frozen")
                        }
                        Some("finalist_1") => {
                            run.frozen_finalists.as_ref().expect("finalists frozen")[0].clone()
                        }
                        Some("finalist_2") => {
                            run.frozen_finalists.as_ref().expect("finalists frozen")[1].clone()
                        }
                        _ => block.camera.clone(),
                    };
                    if let Some(roi) = &block.roi_mode {
                        camera.roi = Some(roi.clone());
                    }
                    camera
                },
                run.plan.name.clone(),
                run.measurement_sequence,
            )
        };
        let required_state = self
            .run
            .as_ref()
            .and_then(|run| run.plan.blocks.get(run.block_index))
            .and_then(|block| block.optical_state.clone());
        if let Some(required_state) = required_state {
            let current_state = self
                .run
                .as_ref()
                .and_then(|run| run.current_optical_state.as_deref());
            if current_state != Some(required_state.as_str()) {
                if let Some(run) = self.run.as_mut() {
                    run.waiting_for_operator = true;
                }
                self.confirmed_optical_state = None;
                if self.run.as_ref().unwrap().reference_id.is_none() {
                    let request_id = self.next_request_id();
                    let run = self.run.as_mut().unwrap();
                    let reference_id = format!(
                        "REF-{}-{}-{}",
                        self.measurement_prefix,
                        run.block_index,
                        run.attempts.get(&run.block_index).copied().unwrap_or(0)
                    );
                    let block = &run.plan.blocks[run.block_index];
                    let seconds = if block.closing {
                        run.plan.wall_clock_limit_s
                    } else {
                        run.plan.acquisition_cutoff_s
                    };
                    let deadline = seconds
                        .map_or_else(|| unix_ms() + 900_000, |s| run.started_unix_ms + s * 1000);
                    run.reference_request_id = Some(request_id);
                    run.reference_id = Some(reference_id.clone());
                    run.reference_ready = false;
                    run.reference_releasing = false;
                    let _=context.request_service(&PluginServiceRequest {request_id,source_plugin_id:ID.into(),target_plugin_id:"stage-a.a4".into(),
                        service:stage_a_universal_runner::SERVICE_REFERENCE_V1.into(),
                        payload:json!({"action":"start","reference_id":reference_id,"deadline_unix_ms":deadline})});
                }
                let instruction = self
                    .run
                    .as_ref()
                    .and_then(|run| {
                        run.plan
                            .optical_states
                            .iter()
                            .find(|state| state.id == required_state)
                    })
                    .map(|state| state.operator_action.clone())
                    .unwrap_or_else(|| format!("Set AOD for optical state '{required_state}'."));
                self.message =
                    format!("{instruction} Press Continue after the readback is stable.");
                return;
            }
        }
        if self.confirmed_optical_state.is_none() {
            if let Some(run) = self.run.as_mut() {
                run.waiting_for_operator = true;
            }
            self.message =
                "Set the AOD, enter its control value and press Continue; camera lux and PD are read automatically".into();
            return;
        }
        let id = self.next_request_id();
        let attempt = self
            .run
            .as_ref()
            .and_then(|r| r.attempts.get(&r.block_index))
            .copied()
            .unwrap_or(0);
        let mut measurement_id = self.measurement_id(experiment, measurement_sequence);
        if experiment == Experiment::A4 && attempt > 0 {
            measurement_id.push_str(&format!("-retry{attempt}"));
        }
        let payload = ExecuteBlockRequest {
            plan_name,
            block_name: block_name.clone(),
            experiment,
            protocol,
            measurement_id,
            output_folder: self.output_folder.trim().to_owned(),
            attempt,
            acquisition_deadline_unix_ms: self.run.as_ref().and_then(|run| {
                let block = &run.plan.blocks[run.block_index];
                let limit = if block.closing {
                    run.plan.wall_clock_limit_s
                } else {
                    run.plan.acquisition_cutoff_s
                };
                limit.map(|seconds| run.started_unix_ms + seconds * 1000)
            }),
            camera,
            optical_state: self.confirmed_optical_state.clone(),
            required_artifacts: self
                .run
                .as_ref()
                .and_then(|run| run.plan.blocks.get(run.block_index))
                .map(|block| block.required_artifacts.clone())
                .unwrap_or_default(),
            retry_same_point: self
                .run
                .as_ref()
                .and_then(|run| run.plan.blocks.get(run.block_index))
                .is_some_and(|block| block.retry_same_point),
        };
        let receipt_dir = Path::new(self.output_folder.trim()).join(&payload.measurement_id);
        if let Err(error) = std::fs::create_dir_all(&receipt_dir).and_then(|_| {
            std::fs::write(
                receipt_dir.join(if attempt == 0 {
                    "universal-request.json".into()
                } else {
                    format!("universal-request-retry{attempt}.json")
                }),
                serde_json::to_vec_pretty(&payload).expect("request serializes"),
            )
        }) {
            self.message = format!("Cannot save block handoff: {error}");
            self.run.as_mut().unwrap().error = Some(self.message.clone());
            return;
        }
        if !self.journal("dispatch", json!({"request": payload})) {
            return;
        }
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
            run.waiting_for_operator = false;
            run.waiting_for_completion = true;
            run.waiting_measurement_id = Some(waiting_measurement_id);
            run.block_started_at = Instant::now();
        }
        self.message = format!(
            "Block {}/{}: {} '{}' accepted; waiting for owner completion",
            self.run.as_ref().map_or(0, |run| run.block_index + 1),
            self.run.as_ref().map_or(0, |run| run.plan.blocks.len()),
            Plan::measurement_prefix(experiment),
            block_name
        );
    }

    fn service_reply(&mut self, _context: &mut impl RunnerControl, reply: PluginServiceReply) {
        let mut rejected = None;
        {
            let Some(run) = self.run.as_mut() else { return };
            if reply.service == stage_a_universal_runner::SERVICE_REFERENCE_V1 {
                if run.reference_request_id != Some(reply.request_id)
                    || reply.target_plugin_id != "stage-a.a4"
                {
                    return;
                }
                if let PluginServiceOutcome::Rejected { code, message } = &reply.outcome {
                    run.error = Some(format!("Optical reference failed ({code}): {message}"));
                    self.message = run.error.clone().unwrap();
                }
                return;
            }
            if let Some(owner) = run.preflight.remove(&reply.request_id) {
                match &reply.outcome {
                    PluginServiceOutcome::Accepted { payload }
                        if reply.target_plugin_id == owner
                            && payload.get("ready").and_then(Value::as_bool) == Some(true) => {}
                    _ => {
                        run.error = Some(format!(
                            "Required owner {owner} is unavailable or busy: {:?}",
                            reply.outcome
                        ));
                        self.message = run.error.clone().unwrap();
                    }
                }
                if run.preflight.is_empty() && run.error.is_none() {
                    run.preflight_finished = true;
                }
                return;
            }
            if run.pending_request_id != Some(reply.request_id) {
                return;
            }
            match reply.outcome {
                PluginServiceOutcome::Accepted { .. } => {
                    self.message = "Block accepted; waiting for explicit completion".into()
                }
                PluginServiceOutcome::Rejected { code, message } => {
                    rejected = Some(format!("Block refused ({code}): {message}"));
                }
            }
        }
        if let Some(message) = rejected {
            self.message = message.clone();
            if let Some(run) = self.run.as_mut() {
                run.error = Some(message);
                run.waiting_for_completion = false;
            }
        }
    }

    fn release_reference(&mut self, context: &mut impl RunnerControl) {
        let request_id = self.next_request_id();
        let Some(run) = self.run.as_mut() else { return };
        let Some(id) = run.reference_id.clone() else {
            return;
        };
        run.reference_request_id = Some(request_id);
        run.reference_releasing = true;
        let _ = context.request_service(&PluginServiceRequest {
            request_id,
            source_plugin_id: ID.into(),
            target_plugin_id: "stage-a.a4".into(),
            service: stage_a_universal_runner::SERVICE_REFERENCE_V1.into(),
            payload: json!({"action":"release","reference_id":id}),
        });
        self.message =
            "Optical state confirmed; releasing reference before the next measurement".into();
    }

    fn checkpoint(&mut self) {
        let Some(run) = self.run.as_ref() else { return };
        let key = format!(
            "{}:{:?}:{:?}:{:?}:{}:{}:{:?}:{:?}",
            run.block_index,
            run.completed,
            run.omitted,
            run.attempts,
            run.waiting_for_completion,
            run.waiting_for_operator,
            run.error,
            run.frozen_finalists
        );
        if key == self.last_checkpoint_key {
            return;
        }
        let data = json!({"schema":"stage-a.universal.checkpoint.v1","plan":run.plan,
            "output_folder":self.output_folder,"measurement_prefix":self.measurement_prefix,
            "selected_candidate":self.selected_candidate,"finalist_1":self.finalist_1,"finalist_2":self.finalist_2,
            "frozen_finalists":run.frozen_finalists,"block_index":run.block_index,
            "started_unix_ms":run.started_unix_ms,"measurement_sequence":run.measurement_sequence,
            "completed":run.completed,"omitted":run.omitted,"attempts":run.attempts,
            "waiting_for_completion":run.waiting_for_completion,"error":run.error,
            "finished":run.block_index>=run.plan.blocks.len()});
        let dir = Path::new(self.output_folder.trim())
            .join(format!("universal-{}", self.measurement_prefix));
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(dir.join("checkpoint.pending.json"))?;
            file.write_all(serde_json::to_string_pretty(&data)?.as_bytes())?;
            file.sync_all()?;
            // Windows cannot rename over an existing file. Preserve the previous checkpoint.
            let path = dir.join("checkpoint.json");
            if path.exists() {
                std::fs::copy(&path, dir.join("checkpoint.previous.json"))?;
                std::fs::remove_file(&path)?;
            }
            std::fs::rename(dir.join("checkpoint.pending.json"), path)
        })();
        if let Err(error) = result {
            self.message = format!("Checkpoint write failed; campaign needs attention: {error}");
            if let Some(run) = self.run.as_mut() {
                run.error = Some(self.message.clone());
                run.user_stop = true;
            }
        } else {
            self.last_checkpoint_key = key;
        }
    }

    fn journal(&mut self, event: &str, detail: Value) -> bool {
        let path = Path::new(self.output_folder.trim())
            .join(format!("universal-{}", self.measurement_prefix))
            .join("events.jsonl");
        let entry = json!({"unix_ms":unix_ms(),"event":event,"detail":detail,
            "completed":self.run.as_ref().map(|r|&r.completed),"omitted":self.run.as_ref().map(|r|&r.omitted)});
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(path)?;
            writeln!(file, "{entry}")?;
            file.sync_data()
        })();
        if let Err(error) = result {
            self.message = format!("Cannot preserve campaign journal: {error}");
            if let Some(run) = self.run.as_mut() {
                run.error = Some(self.message.clone());
                run.user_stop = true;
            }
            false
        } else {
            true
        }
    }
}

impl Plugin for StageAUniversalRunnerPlugin {
    fn name(&self) -> &'static str {
        "Stage-A Universal Runner"
    }
    fn description(&self) -> &'static str {
        "Sequences A1-A6 blocks while experiment plugins own hardware."
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled && self.run.is_some() {
            self.stop_pending = true;
        }
    }
    fn reset(&mut self) {
        self.stop_pending = self.run.is_some();
    }
    fn input_kind(&self) -> PluginInput {
        PluginInput::RawEvents
    }
    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        if let Ok(Some(sensor)) = context.get::<SensorMonitoringV1>(CTX_SENSOR_MONITORING) {
            self.camera_lux = sensor.illumination_lux;
        }
    }
    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        if context.execution().hardware_effects_allowed() {
            let inbox = context.inbox().clone();
            self.drive_control(&inbox, context);
        }
    }
    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema { sections: vec![SettingsSection { label: "Universal Stage-A".into(), description: Some("Sequence existing A1-A6 experiment owners from one protocol.".into()), default_open: true, items: vec![
            SettingItem { key: "program".into(), label: "Built-in program".into(), tooltip: Some("Use smoke, bright_reference, dim_bias_selection, or selected_state_final.".into()), kind: SettingKind::Text { default: self.program.clone() } },
            SettingItem { key: "protocol_path".into(), label: "Protocol file (blank = built-in program)".into(), tooltip: None, kind: SettingKind::Path { dialog: augur_plugin_api::PathDialogKind::OpenFile, default: self.protocol_path.clone() } },
            SettingItem { key: "resume_checkpoint".into(), label: "Resume checkpoint (optional)".into(), tooltip: Some("Load checkpoint.json to resume unfinished work. The original final deadline is retained.".into()), kind: SettingKind::Path { dialog: augur_plugin_api::PathDialogKind::OpenFile, default: self.resume_checkpoint.clone() } },
            SettingItem { key: "measurement_prefix".into(), label: "Measurement prefix".into(), tooltip: None, kind: SettingKind::Text { default: self.measurement_prefix.clone() } },
            SettingItem { key: "output_folder".into(), label: "Common output folder".into(), tooltip: Some("Absolute campaign folder. Each measurement is saved in its own subfolder; all A1-A5 owners receive this path.".into()), kind: SettingKind::Path { dialog: augur_plugin_api::PathDialogKind::Directory, default: self.output_folder.clone() } },
            SettingItem { key: "aod_setting".into(), label: "AOD setting".into(), tooltip: Some("Control value only; not a flux measurement.".into()), kind: SettingKind::Text { default: self.aod_setting.clone() } },
            SettingItem { key: "confirm_optical_state".into(), label: "Continue: confirm optical state".into(), tooltip: Some("Required after every AOD change.".into()), kind: SettingKind::Button { enabled: true } },
            SettingItem { key: "selected_candidate".into(), label: "Selected final bias (B0–B5)".into(), tooltip: Some("Choose after reviewing the bias screen; no automatic winner.".into()), kind: SettingKind::Text { default: self.selected_candidate.clone() } },
            SettingItem { key: "finalist_1".into(), label: "First screen finalist (B0–B5)".into(), tooltip: None, kind: SettingKind::Text { default: self.finalist_1.clone() } },
            SettingItem { key: "finalist_2".into(), label: "Second screen finalist (B0–B5)".into(), tooltip: None, kind: SettingKind::Text { default: self.finalist_2.clone() } },
            SettingItem { key: "stop".into(), label: "Stop and save".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
            SettingItem { key: "start".into(), label: "Run universal protocol".into(), tooltip: None, kind: SettingKind::Button { enabled: true } },
        ] }] }
    }
    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "resume_checkpoint" => Some(json!(self.resume_checkpoint)),
            "program" => Some(json!(self.program)),
            "protocol_path" => Some(json!(self.protocol_path)),
            "measurement_prefix" => Some(json!(self.measurement_prefix)),
            "output_folder" => Some(json!(self.output_folder)),
            "aod_setting" => Some(json!(self.aod_setting)),
            "confirm_optical_state" => Some(json!(self.continue_press.counter)),
            "start" => Some(json!(self.start_press.counter)),
            "stop" => Some(json!(self.stop_press.counter)),
            "selected_candidate" => Some(json!(self.selected_candidate)),
            "finalist_1" => Some(json!(self.finalist_1)),
            "finalist_2" => Some(json!(self.finalist_2)),
            _ => None,
        }
    }
    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        if self.run.is_some()
            && matches!(
                key,
                "program" | "protocol_path" | "measurement_prefix" | "resume_checkpoint"
            )
        {
            return Err("Stop the campaign before changing its identity or protocol".into());
        }
        match key {
            "resume_checkpoint" => {
                self.resume_checkpoint =
                    value.as_str().ok_or("checkpoint path must be text")?.into();
                Ok(())
            }
            "program" => {
                self.program = value.as_str().ok_or("program must be text")?.into();
                Ok(())
            }
            "protocol_path" => {
                self.protocol_path = value
                    .as_str()
                    .ok_or("protocol_path must be a string")?
                    .into();
                Ok(())
            }
            "measurement_prefix" => {
                self.measurement_prefix = value
                    .as_str()
                    .ok_or("measurement_prefix must be a string")?
                    .into();
                Ok(())
            }
            "output_folder" => {
                if self.run.is_some() {
                    return Err("Stop the universal run before changing its output folder".into());
                }
                self.output_folder = value
                    .as_str()
                    .ok_or("output_folder must be a string")?
                    .into();
                Ok(())
            }
            "aod_setting" => {
                if self.run.as_ref().is_some_and(|r| r.waiting_for_completion) {
                    return Err("Do not change attenuation during capture".into());
                }
                self.aod_setting = value.as_str().ok_or("aod_setting must be text")?.into();
                self.confirmed_optical_state = None;
                Ok(())
            }
            "confirm_optical_state" => {
                if self.continue_press.accept(&value) {
                    self.continue_pending = true;
                }
                Ok(())
            }
            "start" => {
                if self.start_press.accept(&value) {
                    self.start_pending = true;
                }
                Ok(())
            }
            "stop" => {
                if self.stop_press.accept(&value) {
                    self.stop_pending = true;
                }
                Ok(())
            }
            "selected_candidate" => {
                if self.run.is_some() {
                    return Err("Final bias is frozen while running".into());
                }
                self.selected_candidate = value.as_str().ok_or("candidate must be text")?.into();
                Ok(())
            }
            "finalist_1" | "finalist_2" => {
                if self
                    .run
                    .as_ref()
                    .is_some_and(|r| r.frozen_finalists.is_some())
                {
                    return Err("Finalists are frozen for these checks".into());
                }
                let text = value.as_str().ok_or("candidate must be text")?.to_owned();
                if key == "finalist_1" {
                    self.finalist_1 = text;
                } else {
                    self.finalist_2 = text;
                }
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }
    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut lines = vec![StatusEntry::Text(format!("Status: {}", self.message))];
        let plan = match self
            .run
            .as_ref()
            .map(|run| Ok(run.plan.clone()))
            .unwrap_or_else(|| self.selected_plan())
        {
            Ok(plan) => plan,
            Err(error) => {
                lines.push(StatusEntry::Text(format!("Protocol: NOT READY — {error}")));
                lines.push(StatusEntry::Text(
                    "Start checklist: choose an absolute output folder; select a built-in program or readable protocol file.".into(),
                ));
                return lines;
            }
        };

        lines.push(StatusEntry::Text(format!(
            "Protocol: {}{} | {} blocks | {} expanded owner points | owner-time estimate {}",
            plan.name,
            if plan.version.is_empty() {
                String::new()
            } else {
                format!(" v{}", plan.version)
            },
            plan.blocks.len(),
            plan.display_points(),
            Self::format_duration(plan.display_seconds())
        )));
        lines.push(StatusEntry::Text(format!(
            "Output: {}",
            if self.output_folder.trim().is_empty() {
                "NOT SET — required before start".into()
            } else {
                format!(
                    "{} (each measurement gets its own subfolder)",
                    self.output_folder
                )
            }
        )));
        let owners = Self::required_plugins(&plan).join(", ");
        lines.push(StatusEntry::Text(format!("Required plugins: {owners}")));
        lines.push(StatusEntry::Text(format!(
            "Readbacks: camera lux {} | photodiode {}{} | PD data path {} | modulation {} / calibration {}",
            if self.camera_lux.is_some() { "OK" } else { "MISSING" },
            if self.photodiode_connected { "connected" } else { "MISSING" },
            self.photodiode_level
                .map(|level| format!(" ({level:.6} V)"))
                .unwrap_or_default(),
            self.photodiode_data_dir.as_deref().unwrap_or("MISSING"),
            if self.modulation_connected { "connected" } else { "MISSING" },
            if self.modulation_calibrated { "OK" } else { "MISSING" },
        )));
        let mut missing = Vec::new();
        if self.output_folder.trim().is_empty() {
            missing.push("common output folder");
        }
        if self.camera_lux.is_none() {
            missing.push("camera-lux readback");
        }
        if !self.photodiode_connected {
            missing.push("photodiode connection");
        }
        if self.photodiode_data_dir.is_none() {
            missing.push("photodiode data path");
        }
        if !self.modulation_connected {
            missing.push("modulation connection");
        }
        if !self.modulation_calibrated {
            missing.push("modulation calibration");
        }
        if missing.is_empty() {
            lines.push(StatusEntry::Text(
                "READBACKS READY — Run will verify every acquisition owner before dispatch.".into(),
            ));
        } else {
            lines.push(StatusEntry::Text(format!(
                "START GATE: BLOCKED — missing {}. Resolve these before Run/Continue.",
                missing.join(", ")
            )));
        }

        if let Some(run) = &self.run {
            if run.reference_id.is_some() {
                lines.push(StatusEntry::Text(format!(
                    "AOD reference: {}",
                    if run.reference_releasing {
                        "releasing"
                    } else if run.reference_ready {
                        "constant output ready; set AOD and Continue"
                    } else {
                        "preparing constant output; wait"
                    }
                )));
            }
            if let Some(limit) = run.plan.wall_clock_limit_s {
                lines.push(StatusEntry::Text(format!(
                    "Campaign elapsed {} / limit {}; {} completed blocks, {} omitted",
                    Self::format_duration(run.started_at.elapsed().as_secs_f64()),
                    Self::format_duration(limit as f64),
                    run.completed.len(),
                    run.omitted.len()
                )));
            }
            if let Some(block) = run.plan.blocks.get(run.block_index) {
                let block_points = block.point_count();
                let block_seconds = block.seconds(run.plan.overhead_s_per_point);
                let elapsed = run.block_started_at.elapsed().as_secs_f64();
                let remaining = (block_seconds - elapsed).max(0.0);
                let measurement = run
                    .waiting_measurement_id
                    .as_deref()
                    .unwrap_or("not dispatched yet");
                lines.push(StatusEntry::Text(format!(
                    "Current: block {}/{} — {} / {} — measurement {}",
                    run.block_index + 1,
                    run.plan.blocks.len(),
                    Plan::measurement_prefix(block.experiment),
                    block.name,
                    measurement
                )));
                if run.waiting_for_operator {
                    lines.push(StatusEntry::Text(format!(
                        "Current block: {} owner points | estimated {} | waiting for operator; timed acquisition has not started",
                        block_points,
                        Self::format_duration(block_seconds)
                    )));
                } else {
                    lines.push(StatusEntry::Text(format!(
                        "Current block: {} owner points | estimated {} | elapsed {} | next action in about {}",
                        block_points,
                        Self::format_duration(block_seconds),
                        Self::format_duration(elapsed),
                        Self::format_duration(remaining)
                    )));
                }
                lines.push(StatusEntry::Text(format!(
                    "Progress: {} of {} owner points completed; {} blocks remain after this one",
                    run.completed
                        .iter()
                        .filter_map(|index| run.plan.blocks.get(*index))
                        .map(|item| item.point_count())
                        .sum::<usize>(),
                    run.plan.display_points(),
                    run.plan.blocks.len().saturating_sub(run.block_index + 1)
                )));
                if run.waiting_for_operator {
                    let state = block.optical_state.as_deref().unwrap_or("unspecified");
                    lines.push(StatusEntry::Text(format!(
                        "NEXT OPERATOR ACTION: set/read back optical state {state}; enter AOD setting; press Continue. Confirmed camera lux: {:?}; PD: {:?}",
                        self.camera_lux, self.photodiode_level
                    )));
                } else {
                    lines.push(StatusEntry::Text(
                        "NEXT OPERATOR ACTION: wait for the owner completion message; do not change the AOD unless requested.".into(),
                    ));
                }
            }
        } else {
            lines.push(StatusEntry::Text(
                "Next action: verify every required plugin is loaded, then press Continue for the first optical state or Run universal protocol.".into(),
            ));
        }
        lines
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Default)]
struct Press {
    counter: u64,
    seen: Option<u64>,
}
impl Press {
    fn accept(&mut self, value: &Value) -> bool {
        if value.as_bool() == Some(true) {
            self.counter += 1;
            self.seen = Some(self.counter);
            return true;
        }
        let Some(incoming) = value.as_u64() else {
            return false;
        };
        let previous = self.seen.replace(incoming);
        self.counter = self.counter.max(incoming);
        previous.is_some_and(|old| incoming > old)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use augur_plugin_api::PluginControlSnapshot;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Control {
        requests: Vec<PluginServiceRequest>,
    }
    impl RunnerControl for Control {
        fn request_service(&mut self, request: &PluginServiceRequest) -> Result<(), String> {
            self.requests.push(request.clone());
            Ok(())
        }
    }
    fn plugin(program: &str) -> StageAUniversalRunnerPlugin {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        StageAUniversalRunnerPlugin {
            program: program.into(),
            output_folder: std::env::temp_dir()
                .join(format!(
                    "universal-tests-{}-{}",
                    unix_ms(),
                    SERIAL.fetch_add(1, Ordering::Relaxed)
                ))
                .to_string_lossy()
                .into_owned(),
            camera_lux: Some(0.08),
            photodiode_level: Some(0.1),
            aod_setting: "saved dim".into(),
            modulation_connected: true,
            modulation_calibrated: true,
            photodiode_connected: true,
            photodiode_data_dir: Some("/tmp".into()),
            selected_candidate: "B2".into(),
            ..Default::default()
        }
    }
    fn preflight(p: &mut StageAUniversalRunnerPlugin, c: &mut Control) {
        p.set_setting("start", json!(true)).unwrap();
        p.drive_control(&PluginControlInbox::default(), c);
        let replies = std::mem::take(&mut c.requests)
            .into_iter()
            .map(|r| PluginServiceReply {
                request_id: r.request_id,
                source_plugin_id: r.source_plugin_id,
                target_plugin_id: r.target_plugin_id,
                service: r.service,
                outcome: PluginServiceOutcome::Accepted {
                    payload: json!({"ready":true}),
                },
            })
            .collect();
        p.drive_control(
            &PluginControlInbox {
                service_replies: replies,
                ..Default::default()
            },
            c,
        );
        assert!(p.run.as_ref().unwrap().preflight_finished);
        assert!(p.run.as_ref().unwrap().waiting_for_operator);
    }
    fn reference_snapshot(p: &mut StageAUniversalRunnerPlugin, c: &mut Control, state: &str) {
        let id = p.run.as_ref().unwrap().reference_id.clone();
        p.drive_control(
            &PluginControlInbox {
                snapshots: vec![PluginControlSnapshot {
                    plugin_id: "stage-a.a4".into(),
                    topic: "stage-a.universal.reference".into(),
                    revision: 1,
                    payload: json!({"state":state,"reference_id":id}),
                }],
                ..Default::default()
            },
            c,
        );
    }
    fn confirm(p: &mut StageAUniversalRunnerPlugin, c: &mut Control) {
        assert_eq!(c.requests.last().unwrap().payload["action"], "start");
        c.requests.clear();
        reference_snapshot(p, c, "ready");
        p.set_setting("confirm_optical_state", json!(true)).unwrap();
        p.drive_control(&PluginControlInbox::default(), c);
        assert_eq!(c.requests.last().unwrap().payload["action"], "release");
        assert!(!p.run.as_ref().unwrap().waiting_for_completion);
        c.requests.clear();
        reference_snapshot(p, c, "released");
    }
    #[test]
    fn continue_waits_for_real_reference_ready_and_release() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        p.set_setting("confirm_optical_state", json!(true)).unwrap();
        p.drive_control(&PluginControlInbox::default(), &mut c);
        assert!(c
            .requests
            .iter()
            .all(|r| r.service != SERVICE_EXECUTE_BLOCK_V1));
        assert!(p.run.as_ref().unwrap().waiting_for_operator);
        confirm(&mut p, &mut c);
        assert_eq!(c.requests[0].service, SERVICE_EXECUTE_BLOCK_V1);
    }
    #[test]
    fn continue_dispatches_once_and_ignores_repeat_during_capture() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        confirm(&mut p, &mut c);
        assert_eq!(c.requests.len(), 1);
        let request: ExecuteBlockRequest =
            serde_json::from_value(c.requests[0].payload.clone()).unwrap();
        assert_eq!(request.protocol, "sine_smoke");
        assert_eq!(request.optical_state.unwrap().state_id, "F2");
        p.set_setting("confirm_optical_state", json!(true)).unwrap();
        p.drive_control(&PluginControlInbox::default(), &mut c);
        assert_eq!(c.requests.len(), 1);
    }
    #[test]
    fn final_uses_selected_bias_and_absolute_cutoff() {
        let mut p = plugin("selected_state_final");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        confirm(&mut p, &mut c);
        let r: ExecuteBlockRequest = serde_json::from_value(c.requests[0].payload.clone()).unwrap();
        assert_eq!(
            r.camera,
            stage_a_universal_runner::candidate_camera("B2").unwrap()
        );
        assert_eq!(
            r.acquisition_deadline_unix_ms,
            Some(p.run.as_ref().unwrap().started_unix_ms + 9_900_000)
        );
        assert!(Path::new(&r.output_folder)
            .join(&r.measurement_id)
            .join("universal-request.json")
            .exists());
    }
    #[test]
    fn missing_owner_prevents_any_acquisition() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        p.set_setting("start", json!(true)).unwrap();
        p.drive_control(&PluginControlInbox::default(), &mut c);
        let r = c
            .requests
            .iter()
            .find(|r| r.target_plugin_id == "stage-a.a2")
            .unwrap()
            .clone();
        p.drive_control(
            &PluginControlInbox {
                service_replies: vec![PluginServiceReply {
                    request_id: r.request_id,
                    source_plugin_id: r.source_plugin_id,
                    target_plugin_id: r.target_plugin_id,
                    service: r.service,
                    outcome: PluginServiceOutcome::Rejected {
                        code: "not_loaded".into(),
                        message: "DLL missing".into(),
                    },
                }],
                ..Default::default()
            },
            &mut c,
        );
        assert!(p.message.contains("stage-a.a2"));
        assert!(c
            .requests
            .iter()
            .all(|r| r.service != SERVICE_EXECUTE_BLOCK_V1));
    }
    #[test]
    fn tagged_owner_connection_states_are_recognized() {
        assert!(StageAUniversalRunnerPlugin::connection_ready(
            &json!({"connection":{"state":"connected","port_label":"COM5"}})
        ));
        assert!(!StageAUniversalRunnerPlugin::connection_ready(
            &json!({"connection":{"state":"disconnected"}})
        ));
    }
    fn complete_current(
        p: &mut StageAUniversalRunnerPlugin,
        c: &mut Control,
        state: &str,
        attempt: u64,
    ) {
        let run = p.run.as_ref().unwrap();
        let snapshot = PluginControlSnapshot {
            plugin_id: StageAUniversalRunnerPlugin::target(
                run.plan.blocks[run.block_index].experiment,
            )
            .into(),
            topic: "stage-a.universal.block".into(),
            revision: 1,
            payload: json!({"state":state,"attempt":attempt,"measurement_id":run.waiting_measurement_id}),
        };
        c.requests.clear();
        p.drive_control(
            &PluginControlInbox {
                snapshots: vec![snapshot],
                ..Default::default()
            },
            c,
        );
    }
    #[test]
    fn completed_campaign_preserves_all_main_rows_and_final_checkpoint() {
        let mut p = plugin("selected_state_final");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        let count = p.run.as_ref().unwrap().plan.blocks.len();
        let mut recorded = 0;
        while p.run.is_some() {
            if p.run.as_ref().unwrap().waiting_for_operator {
                confirm(&mut p, &mut c);
            }
            assert!(
                p.run.as_ref().unwrap().waiting_for_completion,
                "{}",
                p.message
            );
            recorded += 1;
            complete_current(&mut p, &mut c, "completed", 0);
            assert!(recorded <= count);
        }
        assert_eq!(recorded, count);
        let path = Path::new(&p.output_folder)
            .join(format!("universal-{}", p.measurement_prefix))
            .join("checkpoint.json");
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(saved["finished"], true);
        assert_eq!(saved["omitted"], json!([]));
    }
    #[test]
    fn failed_attempt_retries_same_block_and_ignores_its_stale_result() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        confirm(&mut p, &mut c);
        complete_current(&mut p, &mut c, "failed", 0);
        assert_eq!(p.run.as_ref().unwrap().block_index, 0);
        assert_eq!(p.run.as_ref().unwrap().attempts[&0], 1);
        assert_eq!(c.requests[0].payload["attempt"], 1);
        complete_current(&mut p, &mut c, "failed", 0);
        assert_eq!(p.run.as_ref().unwrap().attempts[&0], 1);
    }
    #[test]
    fn resume_preserves_original_clock_and_skips_completed_blocks() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        confirm(&mut p, &mut c);
        complete_current(&mut p, &mut c, "completed", 0);
        let start = p.run.as_ref().unwrap().started_unix_ms;
        let path = Path::new(&p.output_folder)
            .join(format!("universal-{}", p.measurement_prefix))
            .join("checkpoint.json");
        let mut resumed = plugin("smoke");
        resumed.resume_checkpoint = path.to_string_lossy().into_owned();
        resumed.set_setting("start", json!(true)).unwrap();
        resumed.drive_control(&PluginControlInbox::default(), &mut Control::default());
        assert_eq!(resumed.run.as_ref().unwrap().block_index, 1);
        assert!(resumed.run.as_ref().unwrap().completed.contains(&0));
        assert_eq!(resumed.run.as_ref().unwrap().started_unix_ms, start);
    }
    #[test]
    fn checkpoint_failure_does_not_dispatch_a_measurement() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        let dir = Path::new(&p.output_folder).join(format!("universal-{}", p.measurement_prefix));
        std::fs::create_dir(dir.join("checkpoint.pending.json")).unwrap();
        c.requests.clear();
        reference_snapshot(&mut p, &mut c, "ready");
        p.set_setting("confirm_optical_state", json!(true)).unwrap();
        p.drive_control(&PluginControlInbox::default(), &mut c);
        c.requests.clear();
        reference_snapshot(&mut p, &mut c, "released");
        assert!(!c
            .requests
            .iter()
            .any(|r| r.service == SERVICE_EXECUTE_BLOCK_V1));
    }
    #[test]
    fn mirrored_button_edges_do_not_repeat_or_replay() {
        let mut press = Press::default();
        assert!(!press.accept(&json!(4)));
        assert!(press.accept(&json!(5)));
        assert!(!press.accept(&json!(5)));
        assert!(press.accept(&json!(true)));
        assert_eq!(press.counter, 6);
    }
    #[test]
    fn wrong_owner_completion_cannot_advance_campaign() {
        let mut p = plugin("smoke");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        confirm(&mut p, &mut c);
        let id = p.run.as_ref().unwrap().waiting_measurement_id.clone();
        p.drive_control(
            &PluginControlInbox {
                snapshots: vec![PluginControlSnapshot {
                    plugin_id: "stage-a.a4".into(),
                    topic: "stage-a.universal.block".into(),
                    revision: 1,
                    payload: json!({"state":"completed","measurement_id":id}),
                }],
                ..Default::default()
            },
            &mut c,
        );
        assert_eq!(p.run.as_ref().unwrap().block_index, 0);
    }
    #[test]
    fn cutoff_stops_active_owner_once_and_waits_for_finalization() {
        let mut p = plugin("selected_state_final");
        let mut c = Control::default();
        preflight(&mut p, &mut c);
        confirm(&mut p, &mut c);
        p.run.as_mut().unwrap().started_at = Instant::now() - std::time::Duration::from_secs(9901);
        p.drive_control(&PluginControlInbox::default(), &mut c);
        p.drive_control(&PluginControlInbox::default(), &mut c);
        assert_eq!(
            c.requests
                .iter()
                .filter(|r| r.service == stage_a_universal_runner::SERVICE_STOP_V1)
                .count(),
            1
        );
        assert!(p.run.as_ref().unwrap().waiting_for_completion);
    }
}

export_plugin!(StageAUniversalRunnerPlugin);
