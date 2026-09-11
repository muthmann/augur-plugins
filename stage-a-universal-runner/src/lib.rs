//! Shared declarative plan for a complete Stage-A measurement day.
//!
//! This crate owns no hardware. Modulation and photodiode access remain with
//! their existing owners; the future plugin runner uses this plan to select
//! the corresponding measurement module and to share IDs, resume keys,
//! pauses, and timing estimates.

use serde::{Deserialize, Serialize};

mod protocol_catalog;

#[derive(Debug, Clone, Copy)]
pub struct BuiltinProtocol {
    pub name: &'static str,
    pub experiment: Experiment,
    pub extension: &'static str,
    pub contents: &'static str,
    pub points: usize,
    pub seconds: f64,
}

pub fn builtin_protocols() -> &'static [BuiltinProtocol] {
    protocol_catalog::PROTOCOLS
}

pub fn builtin_protocol(name: &str) -> Option<&'static BuiltinProtocol> {
    let name = match name {
        "a1_bright_overlap_bridge" => "bright_bridge",
        "a1_a3_low_light_final" => "main_complete",
        "a2_low_light_final" => "latency_core",
        "a2_heldout_blink_validation" => "heldout_temporal",
        "a5_complete_scientific" => "load_recovery",
        _ => name,
    };
    builtin_protocols().iter().find(|p| p.name == name)
}

pub fn materialize_protocol(name: &str) -> Result<Option<String>, String> {
    let Some(protocol) = builtin_protocol(name) else {
        return Ok(None);
    };
    let root = std::env::temp_dir().join("stage-a-universal-20260910-v4");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let path = root.join(format!("{}.{}", protocol.name, protocol.extension));
    std::fs::write(&path, protocol.contents).map_err(|e| e.to_string())?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

pub fn candidate_camera(name: &str) -> Result<CameraSettings, String> {
    let (on, off, fo) = match name.trim() {
        "B0" => (0, 0, 0),
        "B1" => (-15, -2, 0),
        "B2" => (-30, -5, 0),
        "B3" => (0, 0, 55),
        "B4" => (-15, -2, 55),
        "B5" => (-30, -5, 55),
        _ => return Err("Select a measured candidate B0 through B5".into()),
    };
    Ok(CameraSettings {
        diff_on: Some(on),
        diff_off: Some(off),
        fo: Some(fo),
        hpf: Some(0),
        refr: Some(235),
        filters_off: Some(true),
        roi: None,
    })
}

pub const SERVICE_REFERENCE_V1: &str = "stage-a.universal.reference.v1";

pub const SERVICE_READY_V1: &str = "stage-a.universal.ready.v1";
pub const SERVICE_STOP_V1: &str = "stage-a.universal.stop.v1";

pub const SERVICE_EXECUTE_BLOCK_V1: &str = "stage-a.universal.execute-block.v1";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Experiment {
    #[serde(alias = "A1")]
    A1,
    #[serde(alias = "A2")]
    A2,
    #[serde(alias = "A3")]
    A3,
    #[serde(alias = "A4")]
    A4,
    #[serde(alias = "A5")]
    A5,
    #[serde(alias = "A6")]
    A6,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub name: String,
    /// Human-readable campaign version recorded in every block hand-off.
    #[serde(default)]
    pub version: String,
    /// One of `smoke`, `bright_reference`, `dim_bias_selection`, or
    /// `selected_state_final`.  The runner does not infer scientific intent
    /// from file names.
    #[serde(default)]
    pub program: String,
    #[serde(default)]
    pub overhead_s_per_point: f64,
    #[serde(default)]
    pub wall_clock_limit_s: Option<u64>,
    #[serde(default)]
    pub acquisition_cutoff_s: Option<u64>,
    /// The operator may change only the AOD between these optical states.
    /// Each state requires a stable camera-lux and photodiode observation
    /// before the next block is dispatched.
    #[serde(default)]
    pub optical_states: Vec<OpticalState>,
    #[serde(rename = "block")]
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OpticalState {
    pub id: String,
    pub label: String,
    pub operator_action: String,
    #[serde(default)]
    pub target_camera_lux: Option<String>,
    #[serde(default)]
    pub pause_before: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub experiment: Experiment,
    pub name: String,
    pub protocol: String,
    #[serde(default)]
    pub points: Vec<Point>,
    #[serde(default)]
    pub pause_before: bool,
    #[serde(default)]
    pub camera: CameraSettings,
    #[serde(default)]
    pub camera_from: Option<String>,
    #[serde(default)]
    pub roi_mode: Option<String>,
    /// Higher values are omitted first when the remaining time is insufficient.
    #[serde(default)]
    pub optional_priority: u8,
    #[serde(default)]
    pub closing: bool,
    #[serde(default)]
    pub optical_state: Option<String>,
    #[serde(default)]
    pub required_artifacts: Vec<String>,
    #[serde(default)]
    pub retry_same_point: bool,
    /// Optional complete estimate for the delegated owner protocol. The
    /// universal point list is an orchestration description and may not
    /// contain the owner's expanded acquisition points.
    #[serde(default)]
    pub estimated_seconds: Option<f64>,
    #[serde(default)]
    pub estimated_points: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct CameraSettings {
    /// Offsets are relative to the confirmed factory trim, as in A1-A4.
    #[serde(default)]
    pub diff_on: Option<i32>,
    #[serde(default)]
    pub diff_off: Option<i32>,
    #[serde(default)]
    pub fo: Option<i32>,
    #[serde(default)]
    pub hpf: Option<i32>,
    #[serde(default)]
    pub refr: Option<i32>,
    #[serde(default)]
    pub roi: Option<String>,
    #[serde(default)]
    pub filters_off: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Point {
    pub label: String,
    pub duration_s: f64,
    #[serde(default)]
    pub settle_s: f64,
    #[serde(default = "one")]
    pub repeats: u32,
    #[serde(default)]
    pub pause_before: bool,
    #[serde(default)]
    pub roi: Option<String>,
    #[serde(default)]
    pub flux_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub frequency_hz: Option<f64>,
    #[serde(default)]
    pub depth_a: Option<f64>,
    #[serde(default)]
    pub transitions_per_polarity: Option<u32>,
}

fn one() -> u32 {
    1
}

impl Plan {
    pub fn total_points(&self) -> usize {
        self.blocks.iter().map(Block::point_count).sum()
    }

    pub fn total_seconds(&self) -> f64 {
        self.blocks
            .iter()
            .map(|b| b.seconds(self.overhead_s_per_point))
            .sum()
    }

    pub fn display_points(&self) -> usize {
        self.total_points()
    }
    pub fn display_seconds(&self) -> f64 {
        self.total_seconds()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.blocks.is_empty() {
            return Err("Protocol has no blocks".into());
        }
        for block in &self.blocks {
            if block.protocol.trim().is_empty() {
                return Err(format!("{} has no owner protocol", block.name));
            }
            if let Some(p) = builtin_protocol(&block.protocol) {
                if p.experiment != block.experiment
                    && !(p.experiment == Experiment::A1 && block.experiment == Experiment::A3)
                {
                    return Err(format!("Wrong owner for {}", block.name));
                }
                if block.estimated_points.is_some_and(|n| n != p.points)
                    || block
                        .estimated_seconds
                        .is_some_and(|s| !s.is_finite() || (s - p.seconds).abs() > 0.01)
                {
                    return Err(format!("Stale point count or duration for {}", block.name));
                }
            } else if self.wall_clock_limit_s.is_some() {
                return Err(format!(
                    "Timed campaign needs a verified bundled protocol: {}",
                    block.protocol
                ));
            }
            if let Some(state) = &block.optical_state {
                if !self.optical_states.iter().any(|s| &s.id == state) {
                    return Err(format!("Unknown optical state {state}"));
                }
            }
            if block
                .camera_from
                .as_deref()
                .is_some_and(|s| !matches!(s, "selected" | "finalist_1" | "finalist_2"))
            {
                return Err(format!("Unknown camera selection in {}", block.name));
            }
            if block
                .roi_mode
                .as_deref()
                .is_some_and(|s| s != "center_half")
            {
                return Err(format!("Unsupported ROI mode in {}", block.name));
            }
        }
        if let Some(limit) = self.wall_clock_limit_s {
            let cutoff = self
                .acquisition_cutoff_s
                .ok_or("Missing acquisition cutoff")?;
            if cutoff == 0 || cutoff >= limit || self.total_seconds() > cutoff as f64 {
                return Err("Acquisition and closing work do not fit the wall-clock budget".into());
            }
        }
        Ok(())
    }

    pub fn measurement_prefix(experiment: Experiment) -> &'static str {
        match experiment {
            Experiment::A1 => "A1",
            Experiment::A2 => "A2",
            Experiment::A3 => "A3",
            Experiment::A4 => "A4",
            Experiment::A5 => "A5",
            Experiment::A6 => "A6",
        }
    }
}

impl Block {
    pub fn point_count(&self) -> usize {
        builtin_protocol(&self.protocol)
            .map(|p| p.points)
            .or(self.estimated_points)
            .unwrap_or_else(|| self.points.iter().map(|p| p.repeats as usize).sum())
    }
    pub fn seconds(&self, overhead: f64) -> f64 {
        builtin_protocol(&self.protocol)
            .map(|p| p.seconds)
            .or(self.estimated_seconds)
            .unwrap_or_else(|| {
                self.points
                    .iter()
                    .map(|p| (p.duration_s + p.settle_s + overhead) * f64::from(p.repeats))
                    .sum()
            })
    }
}

pub fn parse(text: &str) -> Result<Plan, toml::de::Error> {
    toml::from_str(text)
}

/// Versioned hand-off between the universal runner and an experiment module.
/// The target module remains responsible for its own camera, modulation and
/// photodiode state machine; the universal runner only sequences these blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecuteBlockRequest {
    pub plan_name: String,
    pub block_name: String,
    pub experiment: Experiment,
    pub protocol: String,
    pub measurement_id: String,
    /// Absolute campaign root. Owners create `<root>/<measurement_id>`.
    #[serde(default)]
    pub output_folder: String,
    #[serde(default)]
    pub acquisition_deadline_unix_ms: Option<u64>,
    #[serde(default)]
    pub attempt: u32,
    pub camera: CameraSettings,
    #[serde(default)]
    pub optical_state: Option<OpticalStateConfirmation>,
    #[serde(default)]
    pub required_artifacts: Vec<String>,
    #[serde(default)]
    pub retry_same_point: bool,
}

/// Operator-confirmed optical state. AOD is a control value; camera lux and
/// photodiode level are recorded observations, not calibration constants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpticalStateConfirmation {
    pub state_id: String,
    pub aod_setting: String,
    pub camera_lux: String,
    pub photodiode_level: String,
    pub confirmed_at_utc: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecuteBlockReply {
    pub accepted: bool,
    pub measurement_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_active_campaigns_use_real_owner_rows() {
        for text in [
            include_str!("../protocols/stage-a-grand-final.toml"),
            include_str!("../protocols/stage-a-dim-bias-selection.toml"),
            include_str!("../protocols/stage-a-smoke-test.toml"),
            include_str!("../protocols/stage-a-all.toml"),
            include_str!("../protocols/a6_camera_lux_reference.toml"),
        ] {
            let plan = parse(text).unwrap();
            plan.validate().unwrap();
            assert!(plan
                .blocks
                .iter()
                .all(|b| builtin_protocol(&b.protocol).is_some()));
        }
    }

    #[test]
    fn final_retains_both_main_levels_and_six_hour_limit() {
        let plan = parse(include_str!("../protocols/stage-a-grand-final.toml")).unwrap();
        assert_eq!(plan.wall_clock_limit_s, Some(21600));
        assert_eq!(plan.acquisition_cutoff_s, Some(19800));
        assert!(plan.total_seconds() < 18000.0);
        for state in ["F1", "F2"] {
            let points: usize = plan
                .blocks
                .iter()
                .filter(|b| {
                    b.optical_state.as_deref() == Some(state) && b.protocol.starts_with("main_")
                })
                .map(Block::point_count)
                .sum();
            assert_eq!(points, 106);
        }
        assert!(plan
            .blocks
            .iter()
            .all(|b| b.camera_from.as_deref() == Some("selected")));
    }

    #[test]
    fn screen_pairs_signal_and_background_for_every_candidate() {
        let plan = parse(include_str!("../protocols/stage-a-dim-bias-selection.toml")).unwrap();
        for i in 0..6 {
            let camera = candidate_camera(&format!("B{i}")).unwrap();
            for experiment in [Experiment::A1, Experiment::A4] {
                assert!(plan.blocks.iter().any(|b| b.camera == camera
                    && b.experiment == experiment
                    && b.optical_state.as_deref() == Some("F2")));
            }
        }
    }

    #[test]
    fn stale_display_estimates_are_rejected() {
        let mut plan = parse(include_str!("../protocols/stage-a-grand-final.toml")).unwrap();
        plan.blocks[0].estimated_seconds = Some(1.0);
        assert!(plan.validate().is_err());
    }

    #[test]
    fn block_handoff_is_versioned_and_keeps_experiment_ownership() {
        let request = ExecuteBlockRequest {
            plan_name: "day".into(),
            block_name: "a5".into(),
            experiment: Experiment::A5,
            protocol: "a5_complete_scientific".into(),
            measurement_id: "A5-20260910-01".into(),
            output_folder: "/tmp/stage-a".into(),
            acquisition_deadline_unix_ms: None,
            attempt: 0,
            camera: CameraSettings::default(),
            optical_state: None,
            required_artifacts: Vec::new(),
            retry_same_point: true,
        };
        let encoded = toml::to_string(&request).expect("request serializes");
        assert!(encoded.contains("experiment = \"a5\""));
        assert_eq!(
            SERVICE_EXECUTE_BLOCK_V1,
            "stage-a.universal.execute-block.v1"
        );
    }
}
