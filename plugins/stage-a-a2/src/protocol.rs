use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const MAX_POINTS: usize = 4_096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Protocol {
    pub name: String,
    pub camera: CameraSetup,
    pub optical: OpticalSetup,
    pub gates: Gates,
    pub controller: ControllerSetup,
    #[serde(rename = "point")]
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CameraSetup {
    pub profile: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpticalSetup {
    pub transfer_scope: String,
    pub photodiode_placement: String,
    pub splitter_fraction_to_pd: f64,
    pub optical_config_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Gates {
    pub firmware_a2_confirmed: bool,
    pub comparator_self_test_passed: bool,
    pub camera_external_trigger_confirmed: bool,
    pub h4_loopback_id: String,
    pub h5_polarity_calibration_id: String,
    /// Links this A2 run to the generic raw PD noise/reference captures.
    pub photodiode_reference_set_id: String,
    pub optical_edge_calibration_id: String,
    pub local_flux_calibration_id: String,
    pub recorder_safety_limit_events_per_us: u64,
}

/// Controller settings a protocol still states for itself.
///
/// The lobe endpoints `v_null_dac`/`v_peak_dac` deliberately do **not** live
/// here any more: the modulation owner already publishes the lobe it resolved,
/// together with the calibration ID of the transfer inversion that produced it
/// (ADR 040). Retyping them into the protocol duplicated an owner's state and
/// could not be checked against anything. The runner reads them from
/// `ModulationStateV1::optical_drive` at preflight and refuses when they are
/// absent.
///
/// Unknown keys are rejected here rather than ignored: a file that still lists
/// `v_null_dac`/`v_peak_dac` is a stale file, and silently dropping them would
/// let an operator believe a lobe they typed is the lobe that ran.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControllerSetup {
    pub comparator_hysteresis: u8,
    pub comparator_invert: bool,
    /// Host floor on the interval between optical steps, in microseconds.
    ///
    /// Optional. When absent the runner resolves
    /// `max(5 * pixel_dead_time_us, settling guard)` from sensor telemetry and
    /// records which floor it used. When present the value is kept verbatim and
    /// still has to clear `5 * pixel_dead_time_us`, so freezing a number can
    /// only ever tighten the floor, never loosen it.
    #[serde(default)]
    pub min_half_us: Option<u32>,
    #[serde(default = "default_sample_rate")]
    pub sample_rate_hz: u32,
    #[serde(default = "default_block_samples")]
    pub block_samples: u32,
}

/// How a stepped point gets the comparator threshold `V_50`.
///
/// `V_50` sits midway between the two optical plateaus, and those move with the
/// operating flux — it is not a set-once calibration. [`Self::Auto`] is
/// therefore the default: the runner drives both plateaus, reads the settled
/// photodiode level at each and places the threshold at their midpoint, once
/// per distinct `(mean_u, depth_a)` pedestal. A frozen code stays legal for a
/// point whose threshold was established some other way, and is recorded as the
/// operator's claim rather than as a measurement.
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

    /// Stable token for sidecars and recorder metadata.
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
                        "comparator_threshold_dac must be \"auto\" or a frozen code in 1..=4095, \
                         not {mode:?}"
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
    pub role: String,
    pub settle_s: f64,
    #[serde(default)]
    pub pause_before: bool,
    #[serde(flatten)]
    pub acquisition: Acquisition,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "acquisition_mode", rename_all = "snake_case")]
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

fn default_sample_rate() -> u32 {
    500_000
}
fn default_block_samples() -> u32 {
    256
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError(pub String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProtocolError {}

fn real_id(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.eq_ignore_ascii_case("tbd") && !value.contains("REPLACE")
}

impl Protocol {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.points.is_empty() || self.points.len() > MAX_POINTS {
            return Err(ProtocolError(format!(
                "protocol must contain 1..={MAX_POINTS} points"
            )));
        }
        if !real_id(&self.camera.profile) {
            return Err(ProtocolError(
                "camera.profile is missing/TBD; freeze a complete host camera profile".into(),
            ));
        }
        if self.optical.transfer_scope != "fluorescence_chain" {
            return Err(ProtocolError(
                "transfer_scope must be fluorescence_chain for this protocol".into(),
            ));
        }
        if self.optical.photodiode_placement != "emission_path" {
            return Err(ProtocolError(
                "photodiode_placement must be emission_path".into(),
            ));
        }
        if !(0.0..=1.0).contains(&self.optical.splitter_fraction_to_pd)
            || self.optical.splitter_fraction_to_pd == 0.0
        {
            return Err(ProtocolError(
                "splitter_fraction_to_pd must be in (0,1]".into(),
            ));
        }
        if !real_id(&self.optical.optical_config_id) {
            return Err(ProtocolError(
                "optical_config_id is missing/TBD; freeze the bench first".into(),
            ));
        }
        if !self.gates.firmware_a2_confirmed
            || !self.gates.comparator_self_test_passed
            || !self.gates.camera_external_trigger_confirmed
        {
            return Err(ProtocolError(
                "A2 firmware, comparator self-test and camera external trigger must be confirmed"
                    .into(),
            ));
        }
        for (name, value) in [
            ("h4_loopback_id", self.gates.h4_loopback_id.as_str()),
            (
                "h5_polarity_calibration_id",
                self.gates.h5_polarity_calibration_id.as_str(),
            ),
            (
                "photodiode_reference_set_id",
                self.gates.photodiode_reference_set_id.as_str(),
            ),
            (
                "optical_edge_calibration_id",
                self.gates.optical_edge_calibration_id.as_str(),
            ),
            (
                "local_flux_calibration_id",
                self.gates.local_flux_calibration_id.as_str(),
            ),
        ] {
            if !real_id(value) {
                return Err(ProtocolError(format!("{name} is missing/TBD")));
            }
        }
        if self.gates.recorder_safety_limit_events_per_us == 0 {
            return Err(ProtocolError(
                "recorder_safety_limit_events_per_us must be pre-qualified before this protocol"
                    .into(),
            ));
        }
        let c = &self.controller;
        if c.comparator_hysteresis > 3 {
            return Err(ProtocolError(
                "comparator_hysteresis must be a frozen level in 0..=3".into(),
            ));
        }
        // A present-but-zero floor is an unfinished edit, not "resolve it for
        // me": omitting the key is how a protocol asks for owner resolution.
        if c.min_half_us == Some(0) {
            return Err(ProtocolError(
                "min_half_us is present but zero; omit the key to resolve the floor from sensor \
                 telemetry, or freeze a positive value"
                    .into(),
            ));
        }
        for (index, p) in self.points.iter().enumerate() {
            if p.label.trim().is_empty() || p.role.trim().is_empty() {
                return Err(ProtocolError(format!(
                    "point {} needs label and role",
                    index + 1
                )));
            }
            if p.settle_s < 0.0 {
                return Err(ProtocolError(format!(
                    "point {} has invalid numeric values",
                    index + 1
                )));
            }
            match p.acquisition {
                Acquisition::Dark { duration_s } => {
                    if duration_s <= 0.0 {
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
                    if !(0.0 < mean_u && mean_u <= 1.0)
                        || depth_a <= 0.0
                        || half_period_s <= 0.0
                        || transitions_per_polarity == 0
                    {
                        return Err(ProtocolError(format!(
                            "point {} has invalid stepped numeric values",
                            index + 1
                        )));
                    }
                    // Only checkable here when the protocol froze the floor. A
                    // protocol that leaves it to owner resolution is checked
                    // against the resolved floor at preflight, before any
                    // camera apply or owner lease.
                    if let Some(min_half_us) = c.min_half_us {
                        if half_period_s * 1e6 < f64::from(min_half_us) {
                            return Err(ProtocolError(format!(
                                "point {} half-period violates min_half_us",
                                index + 1
                            )));
                        }
                    }
                    if let Some(code) = comparator_threshold_dac.frozen_code() {
                        if !(1..=4_095).contains(&code) {
                            return Err(ProtocolError(format!(
                                "point {} comparator_threshold_dac must be \"auto\" or a frozen \
                                 code in 1..=4095",
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
            .map(|p| p.settle_s + p.acquisition_seconds())
            .sum()
    }
}

pub fn parse(text: &str) -> Result<Protocol, ProtocolError> {
    let protocol: Protocol = toml::from_str(text).map_err(|e| ProtocolError(e.to_string()))?;
    protocol.validate()?;
    Ok(protocol)
}

#[cfg(test)]
mod tests {
    use super::*;
    const BASE: &str = r#"
name="a2-test"
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
photodiode_reference_set_id="pdref-1"
optical_edge_calibration_id="edge-1"
local_flux_calibration_id="flux-1"
recorder_safety_limit_events_per_us=1000
[controller]
comparator_hysteresis=1
comparator_invert=false
min_half_us=100
[[point]]
label="core"
role="core"
acquisition_mode="stepped"
mean_u=0.3
depth_a=0.45
half_period_s=0.5
transitions_per_polarity=500
settle_s=2
comparator_threshold_dac=500
"#;

    #[test]
    fn parses_complete_fluorescence_protocol() {
        assert_eq!(parse(BASE).unwrap().points.len(), 1);
    }

    #[test]
    fn dark_point_has_duration_but_no_step_parameters() {
        let dark = BASE
            .replace("acquisition_mode=\"stepped\"", "acquisition_mode=\"dark\"")
            .replace("mean_u=0.3\n", "duration_s=30\n")
            .replace("depth_a=0.45\n", "")
            .replace("half_period_s=0.5\n", "")
            .replace("transitions_per_polarity=500\n", "")
            .replace("comparator_threshold_dac=500\n", "");
        let protocol = parse(&dark).unwrap();
        assert_eq!(protocol.points[0].acquisition_seconds(), 30.0);
        assert!(matches!(
            protocol.points[0].acquisition,
            Acquisition::Dark { .. }
        ));
    }

    #[test]
    fn tbd_gate_fails_before_hardware_moves() {
        let text = BASE.replace("h4-1", "TBD");
        assert!(parse(&text).unwrap_err().0.contains("h4_loopback_id"));
    }

    #[test]
    fn a2_requires_the_generic_photodiode_reference_set_id() {
        let text = BASE.replace(
            "photodiode_reference_set_id=\"pdref-1\"",
            "photodiode_reference_set_id=\"TBD\"",
        );
        assert!(parse(&text)
            .unwrap_err()
            .0
            .contains("photodiode_reference_set_id"));
    }

    #[test]
    fn shipped_followup_is_deliberately_not_runnable_before_bringup() {
        let text = include_str!("../protocols/a2_fluorescence_chain_followup.toml");
        let error = parse(text).unwrap_err().0;
        assert!(
            error.contains("camera.profile")
                || error.contains("optical_config_id")
                || error.contains("firmware")
                || error.contains("comparator_threshold_dac"),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn comparator_threshold_defaults_to_auto_when_the_key_is_absent() {
        let text = BASE.replace("comparator_threshold_dac=500\n", "");
        let protocol = parse(&text).unwrap();
        let Acquisition::Stepped {
            comparator_threshold_dac,
            ..
        } = protocol.points[0].acquisition
        else {
            panic!("expected a stepped point");
        };
        assert_eq!(comparator_threshold_dac, ComparatorThreshold::Auto);
        assert!(comparator_threshold_dac.is_auto());
        assert_eq!(comparator_threshold_dac.frozen_code(), None);
    }

    #[test]
    fn comparator_threshold_accepts_the_explicit_auto_token_and_a_frozen_code() {
        let auto = BASE.replace(
            "comparator_threshold_dac=500",
            "comparator_threshold_dac=\"auto\"",
        );
        let Acquisition::Stepped {
            comparator_threshold_dac,
            ..
        } = parse(&auto).unwrap().points[0].acquisition
        else {
            panic!("expected a stepped point");
        };
        assert_eq!(comparator_threshold_dac, ComparatorThreshold::Auto);

        let Acquisition::Stepped {
            comparator_threshold_dac,
            ..
        } = parse(BASE).unwrap().points[0].acquisition
        else {
            panic!("expected a stepped point");
        };
        assert_eq!(comparator_threshold_dac, ComparatorThreshold::Frozen(500));
        assert_eq!(comparator_threshold_dac.mode(), "frozen");
    }

    #[test]
    fn a_frozen_comparator_threshold_outside_the_dac_range_still_refuses() {
        let zero = BASE.replace("comparator_threshold_dac=500", "comparator_threshold_dac=0");
        assert!(parse(&zero)
            .unwrap_err()
            .0
            .contains("comparator_threshold_dac"));

        let over = BASE.replace(
            "comparator_threshold_dac=500",
            "comparator_threshold_dac=4096",
        );
        assert!(parse(&over)
            .unwrap_err()
            .0
            .contains("comparator_threshold_dac"));

        let negative = BASE.replace(
            "comparator_threshold_dac=500",
            "comparator_threshold_dac=-1",
        );
        assert!(parse(&negative)
            .unwrap_err()
            .0
            .contains("comparator_threshold_dac"));
    }

    #[test]
    fn an_unknown_comparator_threshold_mode_is_not_silently_treated_as_auto() {
        let text = BASE.replace(
            "comparator_threshold_dac=500",
            "comparator_threshold_dac=\"measure_it\"",
        );
        let error = parse(&text).unwrap_err().0;
        assert!(error.contains("comparator_threshold_dac"), "{error}");
    }

    #[test]
    fn min_half_us_may_be_omitted_for_owner_resolution_but_never_zero() {
        let omitted = BASE.replace("min_half_us=100\n", "");
        assert_eq!(parse(&omitted).unwrap().controller.min_half_us, None);

        let zero = BASE.replace("min_half_us=100", "min_half_us=0");
        assert!(parse(&zero).unwrap_err().0.contains("min_half_us"));
    }

    #[test]
    fn a_frozen_min_half_us_still_bounds_every_stepped_half_period() {
        let text = BASE.replace("min_half_us=100", "min_half_us=1000000");
        assert!(parse(&text).unwrap_err().0.contains("min_half_us"));
    }

    #[test]
    fn an_omitted_min_half_us_leaves_the_half_period_check_to_preflight() {
        // Without a frozen floor the schema cannot judge the half period, so it
        // must not pretend to: the runner checks it against the resolved floor
        // before any owner lease.
        let text = BASE
            .replace("min_half_us=100\n", "")
            .replace("half_period_s=0.5", "half_period_s=0.000001");
        assert!(parse(&text).is_ok());
    }

    #[test]
    fn hysteresis_above_the_comparator_range_refuses() {
        let text = BASE.replace("comparator_hysteresis=1", "comparator_hysteresis=4");
        assert!(parse(&text)
            .unwrap_err()
            .0
            .contains("comparator_hysteresis"));
    }

    #[test]
    fn the_lobe_endpoints_are_no_longer_a_protocol_field() {
        // Kind-1 values are owner-resolved (ADR 040). A file that still carries
        // them is a stale file, and silently ignoring the keys would let a run
        // cite a lobe nothing checked.
        let text = BASE.replace(
            "comparator_hysteresis=1",
            "v_null_dac=100\nv_peak_dac=1000\ncomparator_hysteresis=1",
        );
        assert!(parse(&text).is_err());
    }
}
