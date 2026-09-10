//! Shared declarative plan for a complete Stage-A measurement day.
//!
//! This crate owns no hardware. Modulation and photodiode access remain with
//! their existing owners; the future plugin runner uses this plan to select
//! the corresponding measurement module and to share IDs, resume keys,
//! pauses, and timing estimates.

use serde::{Deserialize, Serialize};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_plan_counts_repeats_and_overhead() {
        let plan = parse(include_str!("../protocols/stage-a-all.toml")).expect("fixture parses");
        assert_eq!(plan.blocks.len(), 6);
        assert_eq!(plan.total_points(), 24);
        assert_eq!(Plan::measurement_prefix(Experiment::A5), "A5");
        assert!(plan.total_seconds() > 1_000.0);
    }

    #[test]
    fn a6_is_explicitly_a_camera_lux_reference() {
        let plan = parse(include_str!("../protocols/stage-a-all.toml")).expect("fixture parses");
        let a6 = plan.blocks.iter().find(|b| b.experiment == Experiment::A6).expect("A6");
        assert_eq!(a6.protocol, "camera_lux_reference");
        assert!(a6.points.iter().all(|p| p.flux_id.as_deref() == Some("CAMERA_LUX")));
    }
}
