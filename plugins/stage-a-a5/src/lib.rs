//! A5 acquisition adapter.
//!
//! A5 uses the proven A2 coordinated recorder for the transport layer. The
//! protocol and offline analysis define the A5 rate/load observables; no A2
//! latency conclusion is implied by reusing the camera/PD/modulation engine.

use augur_plugin_api::{
    export_plugin, EventStoreHandle, ExecutionContext, HostContext, HostOutput, Plugin,
    PluginControlContext, PluginControlSnapshot, PluginDiscontinuity, PluginFrame, PluginInput,
    PluginRuntimeRole, PluginServiceOutcome, PluginServiceReply, PluginServiceRequest,
    SettingsSchema, StatusEntry,
};
use augur_plugin_stage_a_a2::StageAA2Plugin;
use serde_json::{json, Value};
use stage_a_universal_runner::{ExecuteBlockRequest, Experiment, SERVICE_EXECUTE_BLOCK_V1};

pub struct StageAA5Plugin {
    inner: StageAA2Plugin,
}

impl Default for StageAA5Plugin {
    fn default() -> Self {
        let mut inner = StageAA2Plugin::default();
        inner.set_requester_plugin_id("stage-a.a5");
        Self { inner }
    }
}

impl Plugin for StageAA5Plugin {
    fn name(&self) -> &'static str {
        "Stage-A A5 Refractory and Load"
    }
    fn description(&self) -> &'static str {
        "Runs A5 rate, polarity and load protocols with coordinated camera RAW and PDQ acquisition."
    }
    fn enabled(&self) -> bool {
        self.inner.enabled()
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.inner.set_enabled(enabled);
    }
    fn set_runtime_role(&mut self, role: PluginRuntimeRole) {
        self.inner.set_runtime_role(role);
    }
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn on_discontinuity(&mut self, reason: PluginDiscontinuity) {
        self.inner.on_discontinuity(reason);
    }
    fn input_kind(&self) -> PluginInput {
        self.inner.input_kind()
    }
    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        store: &EventStoreHandle<'_>,
    ) {
        self.inner.process_frame(frame, output, context, store);
    }
    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        self.inner.process_control(context);
    }
    fn control_snapshots(&self) -> Vec<PluginControlSnapshot> {
        self.inner
            .control_snapshots()
            .into_iter()
            .map(|mut snapshot| {
                snapshot.plugin_id = "stage-a.a5".into();
                snapshot.payload["experiment"] = json!("A5");
                snapshot
            })
            .collect()
    }
    fn handle_service_request(
        &mut self,
        request: &PluginServiceRequest,
        execution: &ExecutionContext,
    ) -> PluginServiceReply {
        if request.service != SERVICE_EXECUTE_BLOCK_V1 {
            return self.inner.handle_service_request(request, execution);
        }
        let command = serde_json::from_value::<ExecuteBlockRequest>(request.payload.clone());
        let Ok(mut command) = command else {
            return PluginServiceReply {
                request_id: request.request_id,
                source_plugin_id: request.source_plugin_id.clone(),
                target_plugin_id: request.target_plugin_id.clone(),
                service: request.service.clone(),
                outcome: PluginServiceOutcome::Rejected {
                    code: "invalid_payload".into(),
                    message: "Invalid universal A5 request".into(),
                },
            };
        };
        if command.experiment != Experiment::A5 {
            return PluginServiceReply {
                request_id: request.request_id,
                source_plugin_id: request.source_plugin_id.clone(),
                target_plugin_id: request.target_plugin_id.clone(),
                service: request.service.clone(),
                outcome: PluginServiceOutcome::Rejected {
                    code: "wrong_target".into(),
                    message: "A5 requires an A5 block".into(),
                },
            };
        }
        command.experiment = Experiment::A2;
        let mut delegated = request.clone();
        delegated.payload = serde_json::to_value(command).expect("request serializes");
        self.inner.handle_service_request(&delegated, execution)
    }

    fn settings_schema(&self) -> SettingsSchema {
        self.inner.settings_schema()
    }
    fn get_setting(&self, key: &str) -> Option<Value> {
        self.inner.get_setting(key)
    }
    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        self.inner.set_setting(key, value)
    }
    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut status = self.inner.status_entries();
        status.insert(
            0,
            StatusEntry::Text(
                "A5 analysis mode: refractory/load; timing conclusions remain offline".into(),
            ),
        );
        status
    }
}

export_plugin!(StageAA5Plugin);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn universal_handoff_reaches_recorder_and_retains_attempt_identity() {
        let mut plugin = StageAA5Plugin::default();
        let request = PluginServiceRequest {
            request_id: 7,
            source_plugin_id: "stage-a.universal-runner".into(),
            target_plugin_id: "stage-a.a5".into(),
            service: SERVICE_EXECUTE_BLOCK_V1.into(),
            payload: json!({"plan_name":"smoke","block_name":"load","experiment":"A5",
                "protocol":"load_smoke","measurement_id":"A5-test","output_folder":std::env::temp_dir(),
                "attempt":2,"camera":{},"required_artifacts":[]}),
        };
        assert!(matches!(
            plugin
                .handle_service_request(&request, &ExecutionContext::default())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));
        let execution = ExecutionContext {
            mode: augur_plugin_api::ExecutionMode::LiveCapture,
            effects_allowed: true,
            session_id: None,
        };
        assert!(matches!(
            plugin.handle_service_request(&request, &execution).outcome,
            PluginServiceOutcome::Accepted { .. }
        ));
        let snapshot = plugin
            .control_snapshots()
            .into_iter()
            .find(|s| s.topic == "stage-a.universal.block")
            .unwrap();
        assert_eq!(snapshot.plugin_id, "stage-a.a5");
        assert_eq!(snapshot.payload["attempt"], 2);
        assert_eq!(snapshot.payload["measurement_id"], "A5-test");
        assert!(
            matches!(plugin.handle_service_request(&request,&execution).outcome,PluginServiceOutcome::Rejected {code,..} if code=="owner_busy")
        );
    }
}
