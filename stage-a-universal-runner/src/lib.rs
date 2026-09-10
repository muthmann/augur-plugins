//! Shared declarative plan for a complete Stage-A measurement day.
//!
//! This crate owns no hardware. Modulation and photodiode access remain with
//! their existing owners; the future plugin runner uses this plan to select
//! the corresponding measurement module and to share IDs, resume keys,
//! pauses, and timing estimates.

use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub overhead_s_per_point: f64,
    #[serde(rename = "block")]
    pub blocks: Vec<Block>,
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

fn one() -> u32 { 1 }

impl Plan {
    pub fn total_points(&self) -> usize {
        self.blocks.iter().map(|b| b.points.iter().map(|p| p.repeats as usize).sum::<usize>()).sum()
    }

    pub fn total_seconds(&self) -> f64 {
        self.blocks.iter().flat_map(|b| &b.points).map(|p| {
            (p.duration_s + p.settle_s + self.overhead_s_per_point) * f64::from(p.repeats)
        }).sum()
    }

    pub fn measurement_prefix(experiment: Experiment) -> &'static str {
        match experiment { Experiment::A1 => "A1", Experiment::A2 => "A2", Experiment::A3 => "A3", Experiment::A4 => "A4", Experiment::A5 => "A5", Experiment::A6 => "A6" }
    }
}

pub fn parse(text: &str) -> Result<Plan, toml::de::Error> { toml::from_str(text) }

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
    pub camera: CameraSettings,
    #[serde(default)]
    pub optical_state: Option<OpticalStateConfirmation>,
}

/// Operator-confirmed optical state. AOD is a control value; camera lux and
/// photodiode level are recorded observations, not calibration constants.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpticalStateConfirmation {
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
    fn complete_plan_counts_repeats_and_overhead() {
        let plan = parse(include_str!("../protocols/stage-a-all.toml")).expect("fixture parses");
        assert_eq!(plan.blocks.len(), 5);
        assert_eq!(plan.total_points(), 23);
        assert_eq!(Plan::measurement_prefix(Experiment::A5), "A5");
        assert!(plan.total_seconds() > 1_000.0);
    }

    #[test]
    fn a6_is_explicitly_a_camera_lux_reference() {
        let plan = parse(include_str!("../protocols/a6_camera_lux_reference.toml")).expect("fixture parses");
        let a6 = plan.blocks.iter().find(|b| b.experiment == Experiment::A6).expect("A6");
        assert_eq!(a6.protocol, "camera_lux_reference");
        assert!(a6.points.iter().all(|p| p.flux_id.as_deref() == Some("CAMERA_LUX")));
    }

    #[test]
    fn smoke_plan_covers_all_runtime_stages() {
        let plan = parse(include_str!("../protocols/stage-a-smoke-test.toml")).expect("fixture parses");
        assert_eq!(plan.blocks.len(), 5);
        assert_eq!(plan.total_points(), 5);
        assert!(plan.blocks.iter().all(|block| block.camera.refr == Some(235)));
    }

    #[test]
    fn block_handoff_is_versioned_and_keeps_experiment_ownership() {
        let request = ExecuteBlockRequest {
            plan_name: "day".into(),
            block_name: "a5".into(),
            experiment: Experiment::A5,
            protocol: "a5_complete_scientific".into(),
            measurement_id: "A5-20260910-01".into(),
            camera: CameraSettings::default(),
            optical_state: None,
        };
        let encoded = toml::to_string(&request).expect("request serializes");
        assert!(encoded.contains("experiment = \"a5\""));
        assert_eq!(SERVICE_EXECUTE_BLOCK_V1, "stage-a.universal.execute-block.v1");
    }
}
