use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const MAX_POINTS: usize = 4_096;

/// An A2 protocol describes only the recordings to perform.
///
/// Camera, modulation, photodiode and comparator provenance belongs to the
/// components that own those values. The runner snapshots those owners and
/// writes their state into each measurement sidecar.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Protocol {
    pub name: String,
    #[serde(default)]
    pub trigger_validation: TriggerValidation,
    #[serde(default)]
    pub timing_reference: stage_a_plugin_contract::A2TimingReferenceV1,
    #[serde(rename = "point")]
    pub points: Vec<Point>,
}

/// Noisy captures can be retained for review without declaring timing valid.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TriggerValidation {
    #[default]
    Strict,
    OfflineReview,
}

/// A2 controller values that are owned by the A2 runner, not by the protocol.
/// They are recorded in the sidecar exactly as sent to the firmware.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ControllerSetup {
    pub comparator_hysteresis: u8,
    pub comparator_invert: bool,
    pub min_half_us: Option<u32>,
    pub sample_rate_hz: u32,
    pub block_samples: u32,
}

impl Default for ControllerSetup {
    fn default() -> Self {
        Self {
            comparator_hysteresis: 1,
            comparator_invert: false,
            min_half_us: None,
            sample_rate_hz: 500_000,
            block_samples: 256,
        }
    }
}

/// How a stepped point gets the comparator threshold `V_50`.
///
/// Protocols normally omit this field. `Auto` then measures both plateaus and
/// selects their midpoint. A frozen code remains an advanced diagnostic escape
/// hatch and is written into provenance as an operator assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ComparatorThreshold {
    #[default]
    Auto,
    Frozen(u16),
}

impl ComparatorThreshold {
    pub fn frozen_code(self) -> Option<u16> {
        match self {
            Self::Auto => None,
            Self::Frozen(code) => Some(code),
        }
    }

    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    pub fn mode(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Frozen(_) => "frozen",
        }
    }
}

impl Serialize for ComparatorThreshold {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            Self::Frozen(code) => serializer.serialize_u16(*code),
        }
    }
}

impl<'de> Deserialize<'de> for ComparatorThreshold {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Mode(String),
            Code(i64),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Mode(mode) => {
                if mode.trim().eq_ignore_ascii_case("auto") {
                    Ok(Self::Auto)
                } else {
                    Err(serde::de::Error::custom(format!(
                        "comparator_threshold_dac must be \"auto\" or a frozen code in 1..=4095, not {mode:?}"
                    )))
                }
            }
            Raw::Code(code) => u16::try_from(code).map(Self::Frozen).map_err(|_| {
                serde::de::Error::custom(format!(
                    "comparator_threshold_dac {code} is outside the frozen code range 1..=4095"
                ))
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub label: String,
    #[serde(default = "default_role")]
    pub role: String,
    #[serde(default = "default_settle_seconds")]
    pub settle_s: f64,
    #[serde(default)]
    pub pause_before: bool,
    #[serde(flatten)]
    pub acquisition: Acquisition,
}

fn default_role() -> String {
    "measurement".into()
}

fn default_settle_seconds() -> f64 {
    3.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "acquisition_mode",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Acquisition {
    Dark {
        duration_s: f64,
    },
    Stepped {
        mean_u: f64,
        depth_a: f64,
        half_period_s: f64,
        transitions_per_polarity: u32,
        #[serde(default)]
        comparator_threshold_dac: ComparatorThreshold,
    },
}

impl Point {
    pub fn acquisition_seconds(&self) -> f64 {
        match self.acquisition {
            Acquisition::Dark { duration_s } => duration_s,
            Acquisition::Stepped {
                half_period_s,
                transitions_per_polarity,
                ..
            } => 2.0 * half_period_s * f64::from(transitions_per_polarity),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

impl Protocol {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.name.trim().is_empty() {
            return Err(ProtocolError("protocol name must not be empty".into()));
        }
        if self.points.is_empty() || self.points.len() > MAX_POINTS {
            return Err(ProtocolError(format!(
                "protocol must contain 1..={MAX_POINTS} points"
            )));
        }
        for (index, point) in self.points.iter().enumerate() {
            if point.label.trim().is_empty() || point.role.trim().is_empty() {
                return Err(ProtocolError(format!(
                    "point {} needs label and role",
                    index + 1
                )));
            }
            if !point.settle_s.is_finite() || point.settle_s < 0.0 {
                return Err(ProtocolError(format!(
                    "point {} has invalid settle_s",
                    index + 1
                )));
            }
            match point.acquisition {
                Acquisition::Dark { duration_s } => {
                    if !duration_s.is_finite() || duration_s <= 0.0 {
                        return Err(ProtocolError(format!(
                            "point {} dark duration_s must be positive",
                            index + 1
                        )));
                    }
                }
                Acquisition::Stepped {
                    mean_u,
                    depth_a,
                    half_period_s,
                    transitions_per_polarity,
                    comparator_threshold_dac,
                } => {
                    if !(mean_u.is_finite()
                        && depth_a.is_finite()
                        && half_period_s.is_finite()
                        && 0.0 < mean_u
                        && mean_u <= 1.0
                        && depth_a > 0.0
                        && half_period_s > 0.0)
                        || transitions_per_polarity == 0
                    {
                        return Err(ProtocolError(format!(
                            "point {} has invalid stepped numeric values",
                            index + 1
                        )));
                    }
                    if let Some(code) = comparator_threshold_dac.frozen_code() {
                        if !(1..=4_095).contains(&code) {
                            return Err(ProtocolError(format!(
                                "point {} comparator_threshold_dac must be \"auto\" or a frozen code in 1..=4095",
                                index + 1
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn total_seconds(&self) -> f64 {
        self.points
            .iter()
            .map(|point| point.settle_s + point.acquisition_seconds())
            .sum()
    }
}

pub fn parse(text: &str) -> Result<Protocol, ProtocolError> {
    let protocol: Protocol =
        toml::from_str(text).map_err(|error| ProtocolError(error.to_string()))?;
    protocol.validate()?;
    Ok(protocol)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
name = "a2-test"

[[point]]
label = "core"
acquisition_mode = "stepped"
mean_u = 0.3
depth_a = 0.45
half_period_s = 0.5
transitions_per_polarity = 10
"#;

    #[test]
    fn parses_point_only_protocol() {
        let protocol = parse(BASE).unwrap();
        assert_eq!(protocol.points.len(), 1);
        assert_eq!(protocol.points[0].role, "measurement");
        assert_eq!(protocol.points[0].settle_s, 3.0);
    }

    #[test]
    fn dark_point_has_duration_but_no_step_parameters() {
        let dark = r#"
name = "dark"
[[point]]
label = "floor"
acquisition_mode = "dark"
duration_s = 30
"#;
        let protocol = parse(dark).unwrap();
        assert_eq!(protocol.points[0].acquisition_seconds(), 30.0);
    }

    #[test]
    fn comparator_threshold_defaults_to_auto() {
        let Acquisition::Stepped {
            comparator_threshold_dac,
            ..
        } = parse(BASE).unwrap().points[0].acquisition
        else {
            panic!("expected a stepped point");
        };
        assert_eq!(comparator_threshold_dac, ComparatorThreshold::Auto);
    }

    #[test]
    fn invalid_frozen_threshold_refuses() {
        let text = BASE.replace(
            "transitions_per_polarity = 10",
            "transitions_per_polarity = 10\ncomparator_threshold_dac = 0",
        );
        assert!(parse(&text).unwrap_err().0.contains("comparator_threshold"));
    }

    #[test]
    fn old_manual_hardware_sections_are_rejected() {
        let text = BASE.replacen(
            "[[point]]",
            "[gates]\nfirmware_a2_confirmed = true\n\n[[point]]",
            1,
        );
        let error = parse(&text).unwrap_err().0;
        assert!(error.contains("gates"), "unexpected error: {error}");
    }

    #[test]
    fn shipped_technical_smoke_is_a_short_point_only_protocol() {
        let text = include_str!(
            "../../plugins/stage-a-a2/protocols/a2_emission_path_technical_smoke.toml"
        );
        let protocol = parse(text).unwrap();
        assert_eq!(protocol.points.len(), 3);
        assert_eq!(protocol.total_seconds(), 47.0);
    }

    #[test]
    fn shipped_followup_is_point_only_and_parseable() {
        let text =
            include_str!("../../plugins/stage-a-a2/protocols/a2_fluorescence_chain_followup.toml");
        assert!(!parse(text).unwrap().points.is_empty());
    }
    #[test]
    fn production_drive_sync_schedule_is_valid_and_matches_cycle_mean_targets() {
        let plan = parse(include_str!(
            "../../plugins/stage-a-a2/protocols/a2_production_drive_sync.toml"
        ))
        .unwrap();
        assert_eq!(
            plan.timing_reference,
            stage_a_plugin_contract::A2TimingReferenceV1::DriveSync
        );
        assert_eq!(plan.trigger_validation, TriggerValidation::OfflineReview);
        assert_eq!(plan.points.len(), 43);
        assert_eq!(plan.total_seconds(), 7337.0);
        let mut seen = std::collections::BTreeMap::new();
        for p in &plan.points {
            if let Acquisition::Stepped {
                mean_u, depth_a, ..
            } = p.acquisition
            {
                assert!(mean_u * (0.5 * depth_a).exp() <= 1.0);
                if p.label.starts_with("grid_") {
                    let target: f64 = p
                        .label
                        .split('_')
                        .nth(2)
                        .unwrap()
                        .trim_start_matches("flux")
                        .parse::<f64>()
                        .unwrap()
                        / 1000.0;
                    assert!((mean_u * (0.5 * depth_a).cosh() - target).abs() < 0.0007);
                    *seen
                        .entry((
                            (target * 1000.0).round() as u32,
                            (depth_a * 1000.0).round() as u32,
                        ))
                        .or_insert(0) += 1;
                }
            }
        }
        assert_eq!(seen.len(), 15);
        assert!(seen.values().all(|n| *n == 2));
        let smoke = parse(include_str!(
            "../../plugins/stage-a-a2/protocols/a2_drive_sync_smoke.toml"
        ))
        .unwrap();
        assert_eq!(smoke.total_seconds(), 37.0);
    }
    #[test]
    fn short_core_keeps_three_fluxes_three_depths_and_physical_controls() {
        let plan = parse(include_str!(
            "../../plugins/stage-a-a2/protocols/a2_core_drive_sync.toml"
        ))
        .unwrap();
        assert_eq!(plan.points.len(), 19);
        assert_eq!(plan.total_seconds(), 1195.0);
        assert_eq!(plan.trigger_validation, TriggerValidation::OfflineReview);
        assert_eq!(
            plan.timing_reference,
            stage_a_plugin_contract::A2TimingReferenceV1::DriveSync
        );
        assert_eq!(
            plan.points
                .iter()
                .filter(|p| p.role == "identification_core")
                .count(),
            9
        );
        assert_eq!(plan.points.iter().filter(|p| p.pause_before).count(), 4);
        assert_eq!(
            plan.points
                .iter()
                .filter(|p| matches!(p.acquisition, Acquisition::Dark { .. }))
                .count(),
            2
        );
        assert!(plan.points.iter().any(|p| p.role == "blocked_drive_sham"));
    }
}
