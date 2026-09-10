//! A5 acquisition adapter.
//!
//! A5 uses the proven A2 coordinated recorder for the transport layer. The
//! protocol and offline analysis define the A5 rate/load observables; no A2
//! latency conclusion is implied by reusing the camera/PD/modulation engine.

use augur_plugin_api::{
    EventStoreHandle, ExecutionContext, HostContext, HostOutput, Plugin, PluginControlContext,
    PluginControlSnapshot, PluginDiscontinuity, PluginFrame, PluginInput, PluginRuntimeRole,
    PluginServiceOutcome, PluginServiceReply, PluginServiceRequest, SettingsSchema, StatusEntry,
    export_plugin,
};
use augur_plugin_stage_a_a2::StageAA2Plugin;
use serde_json::{Value, json};
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
        let outcome = if request.service != SERVICE_EXECUTE_BLOCK_V1 {
            PluginServiceOutcome::Rejected {
                code: "unsupported_service".into(),
                message: "A5 does not support this service".into(),
            }
        } else if !execution.hardware_effects_allowed() {
            PluginServiceOutcome::Rejected {
                code: "effects_not_allowed".into(),
                message: "A5 block execution requires the live worker".into(),
            }
        } else {
            match serde_json::from_value::<ExecuteBlockRequest>(request.payload.clone()) {
                Ok(command) if command.experiment == Experiment::A5 => {
                    if command.camera.roi.is_some() {
                        return PluginServiceReply { request_id: request.request_id, source_plugin_id: request.source_plugin_id.clone(), target_plugin_id: request.target_plugin_id.clone(), service: request.service.clone(), outcome: PluginServiceOutcome::Rejected { code: "camera_roi_not_supported".into(), message: "A5 adapter requires a preselected qualified ROI; ROI switching is not yet supported by the delegated engine".into() } };
                    }
                    let protocol_path = if matches!(
                        command.protocol.as_str(),
                        "a5_complete_scientific" | "a5_final_load_recovery"
                    ) {
                        let path = std::env::temp_dir().join("stage-a-a5_complete_scientific.toml");
                        std::fs::write(
                            &path,
                            include_str!("../protocols/a5_complete_scientific.toml"),
                        )
                        .map(|_| path.to_string_lossy().into_owned())
                        .map_err(|error| error.to_string())
                    } else {
                        Ok(command.protocol)
                    };
                    let Ok(protocol_path) = protocol_path else {
                        return PluginServiceReply {
                            request_id: request.request_id,
                            source_plugin_id: request.source_plugin_id.clone(),
                            target_plugin_id: request.target_plugin_id.clone(),
                            service: request.service.clone(),
                            outcome: PluginServiceOutcome::Rejected {
                                code: "protocol_materialization_failed".into(),
                                message: "could not materialize the built-in A5 protocol".into(),
                            },
                        };
                    };
                    if let Err(error) = self
                        .inner
                        .set_setting("protocol_path", json!(protocol_path))
                    {
                        PluginServiceOutcome::Rejected {
                            code: "invalid_protocol".into(),
                            message: error,
                        }
                    } else if let Err(error) = self
                        .inner
                        .set_setting("measurement_id", json!(command.measurement_id))
                    {
                        PluginServiceOutcome::Rejected {
                            code: "invalid_measurement_id".into(),
                            message: error,
                        }
                    } else {
                        self.inner.set_camera_override(Some(command.camera.clone()));
                        let _ = self.inner.set_setting("run_protocol", json!(1));
                        PluginServiceOutcome::Accepted {
                            payload: json!({"state":"started"}),
                        }
                    }
                }
                Ok(command) => PluginServiceOutcome::Rejected {
                    code: "wrong_target".into(),
                    message: format!("A5 cannot execute {:?}", command.experiment),
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
