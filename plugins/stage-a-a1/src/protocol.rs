//! Declarative recording protocols: a TOML file naming the points to record,
//! expanded into a flat list the runner walks.
//!
//! The buttons in the Record section each sweep exactly one axis (or two, for
//! the `a × f` surface) with whatever is currently armed on the other axes.
//! That is the right shape for exploring, and the wrong shape for a survey
//! that has to run overnight and be reproducible six months later. A protocol
//! is the survey form: it names every axis explicitly — the operating point
//! `ū` (the `I_k` axis), the frequency `f`, and the depth `a` — plus the dwell
//! and duration each point is recorded with, in a file that travels with the
//! results.
//!
//! ## Format
//!
//! ```toml
//! name = "a1-survey"
//!
//! [defaults]
//! duration_s = 10
//! settle_s   = 2.0
//!
//! [[block]]
//! name         = "depth-sweep-at-10Hz"
//! mean_u       = [0.5]
//! frequency_hz = [10.0]
//! depth_a      = { min = 0.2, max = 2.0, points = 7 }
//!
//! [[block]]
//! name         = "frequency-ladder"
//! mean_u       = [0.3, 0.5]
//! frequency_hz = { min = 1.0, max = 200.0, points = 5, spacing = "log" }
//! depth_a      = [0.8]
//! duration_s   = 20
//! ```
//!
//! Every axis takes either an explicit list or a `{ min, max, points }` range
//! (`spacing = "linear"` by default, `"log"` for anything read per decade).
//! A block expands to the full product of its three axes.
//!
//! ## Ordering
//!
//! Points come out `ū` outermost, then `f`, then `a`. That is the order of how
//! expensive each change is to settle: the operating point moves the mean
//! illumination the sensor has to re-adapt to, the frequency has to be
//! confirmed against the trigger, and the depth is the cheap innermost step.
//! Any other nesting would spend the whole run settling.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

/// Hard ceiling on the points one protocol may expand to. A three-axis product
/// grows fast, and an operator who typed one zero too many should be told
/// before the bench spends a night on it, not after.
pub const MAX_POINTS: usize = 4_096;

/// Bounds mirrored from the settings so a protocol cannot ask for a point the
/// plugin would refuse anyway — checked at parse time, where the operator can
/// still see which line was wrong.
const DEPTH_A_RANGE: (f64, f64) = (0.01, 6.0);
const MEAN_U_RANGE: (f64, f64) = (0.01, 1.0);
const FREQUENCY_RANGE: (f64, f64) = (
    stage_a_plugin_contract::DRIVE_FREQUENCY_MIN_MILLIHZ as f64 / 1_000.0,
    stage_a_plugin_contract::DRIVE_FREQUENCY_MAX_MILLIHZ as f64 / 1_000.0,
);
const DURATION_RANGE: (i64, i64) = (1, 3_600);
const SETTLE_RANGE: (f64, f64) = (0.0, 60.0);

/// What one protocol row records. The same three roles the Record section's
/// buttons offer, so a protocol can carry a complete measurement — its own
/// background reference and pilot, then the points scored against them —
/// instead of needing two button presses before it can be started.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum PointRole {
    #[default]
    Normal,
    /// Bright reference; freezes the ON/OFF windows for the measurement.
    Pilot,
    /// Unmodulated reference; captures the false-response floor.
    Background,
}

impl PointRole {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "" | "normal" | "point" => Some(Self::Normal),
            "pilot" => Some(Self::Pilot),
            "background" => Some(Self::Background),
            _ => None,
        }
    }
}

/// One recording the protocol asks for, with every parameter already resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolPoint {
    /// Where this row came from — the `[[block]]` name, or a CSV `label` — for
    /// the status line and the sidecar.
    pub block: String,
    /// Normalized cycle-mean lobe point `ū` — the `I_k` axis.
    pub mean_u: f64,
    pub frequency_hz: f64,
    pub depth_a: f64,
    pub duration_s: i64,
    pub settle_s: f64,
    pub role: PointRole,
    /// Optional per-point contrast-threshold offsets. These are the host's
    /// canonical `diff_on`/`diff_off` values, not absolute sensor codes.
    pub diff_on: Option<i32>,
    pub diff_off: Option<i32>,
}

impl ProtocolPoint {
    /// Filename fragment identifying this point inside the measurement folder.
    pub fn tag(&self) -> String {
        format!(
            "u{:.0}m_f{}_a{:.0}m",
            self.mean_u * 1_000.0,
            frequency_tag(self.frequency_hz),
            self.depth_a * 1_000.0,
        )
    }
}

/// A parsed protocol: what to record, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Protocol {
    pub name: String,
    /// Optional revision declared by the protocol author. The exact source
    /// file is archived separately, so this is a human-facing revision, not a
    /// substitute for content identity.
    pub version: Option<String>,
    pub points: Vec<ProtocolPoint>,
    pub camera: Option<CameraSelection>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CameraSelection {
    NamedProfile(String),
    Snapshot(augur_plugin_api::CameraConfigurationSnapshotV1),
}

impl Protocol {
    /// Distinct values on each axis, for the summary shown before starting.
    pub fn axis_counts(&self) -> (usize, usize, usize) {
        let count = |values: Vec<f64>| {
            let mut keys: Vec<u64> = values.into_iter().map(|value| value.to_bits()).collect();
            keys.sort_unstable();
            keys.dedup();
            keys.len()
        };
        (
            count(self.points.iter().map(|point| point.mean_u).collect()),
            count(self.points.iter().map(|point| point.frequency_hz).collect()),
            count(self.points.iter().map(|point| point.depth_a).collect()),
        )
    }

    /// Total bench time the protocol asks for, settling included.
    pub fn total_seconds(&self) -> f64 {
        self.remaining_seconds(0)
    }

    /// Bench time the points from `index` onwards still ask for, settling
    /// included. The point at `index` counts whole: it is the one in flight,
    /// and the recording's own countdown says how far into it the run is.
    ///
    /// Recording overhead (camera start/stop, the lease handshake, the a₀
    /// search) is not in here, so this is a lower bound on the wall clock —
    /// the same quantity [`Self::total_seconds`] announces before the start.
    pub fn remaining_seconds(&self, index: usize) -> f64 {
        self.points
            .iter()
            .skip(index)
            .map(|point| point.duration_s as f64 + point.settle_s)
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolError {
    Toml(String),
    /// A named axis, block or default is unusable, with the reason.
    Invalid {
        what: String,
        detail: String,
    },
    Empty,
    TooManyPoints(usize),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Toml(detail) => write!(f, "the protocol file is not valid TOML: {detail}"),
            Self::Invalid { what, detail } => write!(f, "{what}: {detail}"),
            Self::Empty => f.write_str(
                "the protocol has no points to record — add at least one [[block]] with a \
                 mean_u, a frequency_hz and a depth_a",
            ),
            Self::TooManyPoints(count) => write!(
                f,
                "the protocol expands to {count} recordings, past the {MAX_POINTS} limit — \
                 narrow one of the axes or split it into several files"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

// ---- wire form -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProtocolDoc {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    defaults: Defaults,
    #[serde(default, rename = "block")]
    blocks: Vec<BlockDoc>,
    #[serde(default)]
    camera: Option<CameraDoc>,
}

#[derive(Debug, Deserialize)]
struct CameraDoc {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    snapshot: Option<augur_plugin_api::CameraConfigurationSnapshotV1>,
}

#[derive(Debug, Default, Deserialize)]
struct Defaults {
    #[serde(default)]
    duration_s: Option<i64>,
    #[serde(default)]
    settle_s: Option<f64>,
    #[serde(default)]
    diff_on: Option<i32>,
    #[serde(default)]
    diff_off: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct BlockDoc {
    #[serde(default)]
    name: Option<String>,
    mean_u: Axis,
    frequency_hz: Axis,
    depth_a: Axis,
    #[serde(default)]
    duration_s: Option<i64>,
    #[serde(default)]
    settle_s: Option<f64>,
    #[serde(default)]
    diff_on: Option<i32>,
    #[serde(default)]
    diff_off: Option<i32>,
}

/// One axis: an explicit list, a single value, or a generated range.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Axis {
    One(f64),
    List(Vec<f64>),
    Range {
        min: f64,
        max: f64,
        points: usize,
        #[serde(default)]
        spacing: Spacing,
    },
}

#[derive(Debug, Default, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Spacing {
    #[default]
    Linear,
    Log,
}

impl Axis {
    fn values(&self, what: &str, range: (f64, f64)) -> Result<Vec<f64>, ProtocolError> {
        let invalid = |detail: String| ProtocolError::Invalid {
            what: what.to_owned(),
            detail,
        };
        let values = match self {
            Self::One(value) => vec![*value],
            Self::List(values) => {
                if values.is_empty() {
                    return Err(invalid("the list is empty".into()));
                }
                values.clone()
            }
            Self::Range {
                min,
                max,
                points,
                spacing,
            } => {
                if *points == 0 {
                    return Err(invalid("points must be at least 1".into()));
                }
                if !min.is_finite() || !max.is_finite() {
                    return Err(invalid("min and max must be numbers".into()));
                }
                if max < min {
                    return Err(invalid(format!("max {max} is below min {min}")));
                }
                if *spacing == Spacing::Log && *min <= 0.0 {
                    return Err(invalid(
                        "log spacing needs a min above 0 — a decade ladder has no zero".into(),
                    ));
                }
                if *points == 1 {
                    vec![*min]
                } else {
                    let last = *points - 1;
                    (0..*points)
                        .map(|index| {
                            let t = index as f64 / last as f64;
                            match spacing {
                                Spacing::Linear => min + t * (max - min),
                                Spacing::Log => (min.ln() + t * (max.ln() - min.ln())).exp(),
                            }
                        })
                        .collect()
                }
            }
        };
        for value in &values {
            if !value.is_finite() || *value < range.0 || *value > range.1 {
                return Err(invalid(format!(
                    "{value} is outside the supported {}..={}",
                    range.0, range.1
                )));
            }
        }
        Ok(values)
    }
}

/// Drops a leading UTF-8 byte-order mark.
///
/// Saving a protocol as "CSV UTF-8" in Excel — the obvious choice on a Windows
/// bench — writes a BOM. Left in place it becomes part of the first header
/// cell, so `mean_u` stops matching `mean_u` and the file is refused for
/// missing a column that is plainly there; in the TOML form it fails the parse
/// outright. Neither message would point at an invisible character.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Parses a protocol and expands it into the points to record.
pub fn parse(text: &str) -> Result<Protocol, ProtocolError> {
    let doc: ProtocolDoc =
        toml::from_str(strip_bom(text)).map_err(|error| ProtocolError::Toml(error.to_string()))?;

    let default_duration = doc.defaults.duration_s.unwrap_or(10);
    let default_settle = doc.defaults.settle_s.unwrap_or(2.0);
    let camera = match doc.camera {
        None => None,
        Some(CameraDoc {
            profile: Some(profile),
            snapshot: None,
        }) if !profile.trim().is_empty() => Some(CameraSelection::NamedProfile(profile)),
        Some(CameraDoc {
            profile: None,
            snapshot: Some(snapshot),
        }) => Some(CameraSelection::Snapshot(snapshot)),
        Some(_) => {
            return Err(ProtocolError::Invalid {
                what: "camera".into(),
                detail: "provide exactly one non-empty profile or snapshot".into(),
            });
        }
    };

    let mut points = Vec::new();
    // Blocks may be named or not; unnamed ones get a stable positional name so
    // every recording can still say which part of the protocol it belongs to.
    let mut seen_names: BTreeMap<String, usize> = BTreeMap::new();
    for (index, block) in doc.blocks.iter().enumerate() {
        let base = block
            .name
            .clone()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| format!("block{}", index + 1));
        // Two blocks sharing a name would put two different sets of points in
        // one namespace; keep them distinguishable rather than refusing.
        let occurrence = seen_names.entry(base.clone()).or_insert(0);
        *occurrence += 1;
        let name = if *occurrence == 1 {
            base
        } else {
            format!("{base}-{occurrence}")
        };

        let duration_s = block.duration_s.unwrap_or(default_duration);
        if duration_s < DURATION_RANGE.0 || duration_s > DURATION_RANGE.1 {
            return Err(ProtocolError::Invalid {
                what: format!("block '{name}' duration_s"),
                detail: format!(
                    "{duration_s} is outside the supported {}..={}",
                    DURATION_RANGE.0, DURATION_RANGE.1
                ),
            });
        }
        let settle_s = block.settle_s.unwrap_or(default_settle);
        if !settle_s.is_finite() || settle_s < SETTLE_RANGE.0 || settle_s > SETTLE_RANGE.1 {
            return Err(ProtocolError::Invalid {
                what: format!("block '{name}' settle_s"),
                detail: format!(
                    "{settle_s} is outside the supported {}..={}",
                    SETTLE_RANGE.0, SETTLE_RANGE.1
                ),
            });
        }
        let diff_on = block.diff_on.or(doc.defaults.diff_on);
        let diff_off = block.diff_off.or(doc.defaults.diff_off);

        let mean_u = block
            .mean_u
            .values(&format!("block '{name}' mean_u"), MEAN_U_RANGE)?;
        let frequency_hz = block
            .frequency_hz
            .values(&format!("block '{name}' frequency_hz"), FREQUENCY_RANGE)?;
        let depth_a = block
            .depth_a
            .values(&format!("block '{name}' depth_a"), DEPTH_A_RANGE)?;

        // `ū` outermost, `a` innermost — see the module docs.
        for mean_u in &mean_u {
            for frequency_hz in &frequency_hz {
                for depth_a in &depth_a {
                    points.push(ProtocolPoint {
                        block: name.clone(),
                        mean_u: *mean_u,
                        frequency_hz: *frequency_hz,
                        depth_a: *depth_a,
                        duration_s,
                        settle_s,
                        role: PointRole::Normal,
                        diff_on,
                        diff_off,
                    });
                    if points.len() > MAX_POINTS {
                        return Err(ProtocolError::TooManyPoints(points.len()));
                    }
                }
            }
        }
    }

    if points.is_empty() {
        return Err(ProtocolError::Empty);
    }
    Ok(Protocol {
        name: doc
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "protocol".to_owned()),
        version: doc.version.filter(|version| !version.trim().is_empty()),
        points,
        camera,
    })
}

/// Compact frequency fragment for a filename: `10Hz`, `1500mHz`, `2k5Hz`.
fn frequency_tag(hz: f64) -> String {
    if hz < 1.0 {
        format!("{:.0}mHz", hz * 1_000.0)
    } else if hz < 1_000.0 {
        let rounded = (hz * 10.0).round() / 10.0;
        if (rounded - rounded.round()).abs() < f64::EPSILON {
            format!("{rounded:.0}Hz")
        } else {
            format!("{rounded:.1}Hz").replace('.', "p")
        }
    } else {
        format!("{:.0}Hz", hz.round())
    }
}

/// Reads a protocol from a file, choosing the reader by extension.
///
/// `.csv` is the row-per-recording form and the one to reach for: one line is
/// one recording, every parameter is a column, and it opens in a spreadsheet
/// or comes straight out of a script. `.toml` is the block/range form — more
/// compact for a dense regular sweep, and kept because it expresses one.
///
/// Both produce the same flat list, so nothing downstream knows which was used.
pub fn parse_file(path: &str, text: &str) -> Result<Protocol, ProtocolError> {
    let is_csv = std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("csv"));
    if is_csv {
        let mut protocol = parse_csv(text)?;
        protocol.name = std::path::Path::new(path)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
            .unwrap_or("protocol")
            .to_owned();
        Ok(protocol)
    } else {
        parse(text)
    }
}

/// Columns a protocol CSV may carry. `mean_u`, `frequency_hz` and `depth_a` are
/// required; the rest fall back to their defaults.
const CSV_REQUIRED: [&str; 3] = ["mean_u", "frequency_hz", "depth_a"];
const CSV_OPTIONAL: [&str; 7] = [
    "duration_s",
    "settle_s",
    "label",
    "role",
    "camera_profile",
    "diff_on",
    "diff_off",
];

/// Parses the row-per-recording CSV form.
///
/// Columns are located **by header name**, so their order does not matter and a
/// column can be left out entirely — the same rule the sensor readout follows,
/// and the reason a file edited in a spreadsheet keeps working after someone
/// drags a column.
///
/// Blank lines and `#` comments are skipped, so a file can explain itself.
/// Errors carry the **file line number**, not the row index, because that is
/// what an editor and a spreadsheet both show.
pub fn parse_csv(text: &str) -> Result<Protocol, ProtocolError> {
    let mut header: Option<Vec<String>> = None;
    let mut points = Vec::new();
    let mut camera_profile: Option<String> = None;

    // `lines()` already absorbs CRLF; the BOM is the part it leaves behind.
    for (offset, raw) in strip_bom(text).lines().enumerate() {
        let line_no = offset + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = crate::csv::split_line(raw);

        let Some(columns) = header.as_ref() else {
            let columns: Vec<String> = fields
                .iter()
                .map(|field| field.trim().to_ascii_lowercase())
                .collect();
            for required in CSV_REQUIRED {
                if !columns.iter().any(|column| column == required) {
                    return Err(ProtocolError::Invalid {
                        what: format!("line {line_no}: the header"),
                        detail: format!(
                            "has no '{required}' column. Required: {}. Optional: {}",
                            CSV_REQUIRED.join(", "),
                            CSV_OPTIONAL.join(", ")
                        ),
                    });
                }
            }
            header = Some(columns);
            continue;
        };

        let cell = |name: &str| -> Option<&str> {
            let index = columns.iter().position(|column| column == name)?;
            fields.get(index).map(|field| field.trim())
        };
        let number = |name: &str, range: (f64, f64)| -> Result<f64, ProtocolError> {
            let raw = cell(name).unwrap_or("");
            let invalid = |detail: String| ProtocolError::Invalid {
                what: format!("line {line_no}: {name}"),
                detail,
            };
            if raw.is_empty() {
                return Err(invalid("is empty".into()));
            }
            let value: f64 = raw
                .parse()
                .map_err(|_| invalid(format!("'{raw}' is not a number")))?;
            if !value.is_finite() || value < range.0 || value > range.1 {
                return Err(invalid(format!(
                    "{value} is outside the supported {}..={}",
                    range.0, range.1
                )));
            }
            Ok(value)
        };

        let mean_u = number("mean_u", MEAN_U_RANGE)?;
        let frequency_hz = number("frequency_hz", FREQUENCY_RANGE)?;
        let depth_a = number("depth_a", DEPTH_A_RANGE)?;

        // Absent column *or* empty cell falls back, so a file can carry a
        // duration column that only some rows fill in.
        let duration_s = match cell("duration_s").unwrap_or("") {
            "" => 10,
            raw => {
                let value: i64 = raw.parse().map_err(|_| ProtocolError::Invalid {
                    what: format!("line {line_no}: duration_s"),
                    detail: format!("'{raw}' is not a whole number of seconds"),
                })?;
                if value < DURATION_RANGE.0 || value > DURATION_RANGE.1 {
                    return Err(ProtocolError::Invalid {
                        what: format!("line {line_no}: duration_s"),
                        detail: format!(
                            "{value} is outside the supported {}..={}",
                            DURATION_RANGE.0, DURATION_RANGE.1
                        ),
                    });
                }
                value
            }
        };
        let settle_s = match cell("settle_s").unwrap_or("") {
            "" => 2.0,
            _ => number("settle_s", SETTLE_RANGE)?,
        };
        let role =
            PointRole::parse(cell("role").unwrap_or("")).ok_or_else(|| ProtocolError::Invalid {
                what: format!("line {line_no}: role"),
                detail: format!(
                    "'{}' is not one of normal, pilot, background",
                    cell("role").unwrap_or("")
                ),
            })?;
        let label = cell("label").unwrap_or("").trim().to_owned();
        let row_profile = cell("camera_profile").unwrap_or("").trim();
        if !row_profile.is_empty() {
            match &camera_profile {
                Some(existing) if existing != row_profile => {
                    return Err(ProtocolError::Invalid {
                        what: format!("line {line_no}: camera_profile"),
                        detail: format!(
                            "'{row_profile}' differs from the series profile '{existing}'"
                        ),
                    });
                }
                None => camera_profile = Some(row_profile.to_owned()),
                _ => {}
            }
        }
        let parse_bias = |name: &str| -> Result<Option<i32>, ProtocolError> {
            let raw = cell(name).unwrap_or("");
            if raw.is_empty() {
                return Ok(None);
            }
            raw.parse::<i32>()
                .map(Some)
                .map_err(|_| ProtocolError::Invalid {
                    what: format!("line {line_no}: {name}"),
                    detail: format!("'{raw}' is not a signed integer offset"),
                })
        };
        let diff_on = parse_bias("diff_on")?;
        let diff_off = parse_bias("diff_off")?;

        points.push(ProtocolPoint {
            block: if label.is_empty() {
                format!("row{}", points.len() + 1)
            } else {
                label
            },
            mean_u,
            frequency_hz,
            depth_a,
            duration_s,
            settle_s,
            role,
            diff_on,
            diff_off,
        });
        if points.len() > MAX_POINTS {
            return Err(ProtocolError::TooManyPoints(points.len()));
        }
    }

    if header.is_none() {
        return Err(ProtocolError::Invalid {
            what: "the protocol file".into(),
            detail: format!(
                "has no header line. The first line that is not blank or a # comment must name \
                 the columns: {}",
                CSV_REQUIRED.join(", ")
            ),
        });
    }
    if points.is_empty() {
        return Err(ProtocolError::Empty);
    }
    Ok(Protocol {
        name: "protocol".to_owned(),
        version: None,
        points,
        camera: camera_profile.map(CameraSelection::NamedProfile),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
name = "sample"
version = "2026-08-13"

[defaults]
duration_s = 5
settle_s = 1.5

[[block]]
name = "flat"
mean_u = 0.5
frequency_hz = [10.0, 20.0]
depth_a = { min = 0.5, max = 1.5, points = 3 }

[[block]]
name = "ladder"
mean_u = [0.3, 0.6]
frequency_hz = { min = 1.0, max = 100.0, points = 3, spacing = "log" }
depth_a = 0.8
duration_s = 30
"#;

    #[test]
    fn a_spreadsheet_bom_does_not_hide_the_first_column() {
        // Excel's "CSV UTF-8" writes a BOM. Without stripping it, `mean_u`
        // reads as `\u{feff}mean_u` and the file is refused for missing the
        // column it visibly has.
        let csv = "\u{feff}mean_u,frequency_hz,depth_a\n0.5,10,1.0\n";
        let protocol = parse_file("survey.csv", csv).expect("BOM-prefixed CSV");
        assert_eq!(protocol.points.len(), 1);
        assert_eq!(protocol.points[0].mean_u, 0.5);
    }

    #[test]
    fn a_bom_does_not_break_the_toml_form_either() {
        let protocol = parse(&format!("\u{feff}{SAMPLE}")).expect("BOM-prefixed TOML");
        assert_eq!(protocol.name, "sample");
        assert_eq!(protocol.version.as_deref(), Some("2026-08-13"));
    }

    #[test]
    fn a_spreadsheet_crlf_file_parses() {
        let csv = "mean_u,frequency_hz,depth_a\r\n0.5,10,1.0\r\n";
        let protocol = parse_file("survey.csv", csv).expect("CRLF CSV");
        assert_eq!(protocol.points.len(), 1);
        assert_eq!(protocol.points[0].frequency_hz, 10.0);
    }

    #[test]
    fn a_protocol_expands_to_the_product_of_its_axes() {
        let protocol = parse(SAMPLE).expect("valid protocol");
        assert_eq!(protocol.name, "sample");
        // 1×2×3 + 2×3×1
        assert_eq!(protocol.points.len(), 6 + 6);
        assert_eq!(protocol.axis_counts(), (3, 5, 4));
    }

    #[test]
    fn points_run_mean_u_outermost_and_depth_innermost() {
        // The nesting is the whole reason the protocol is worth having over
        // three nested button presses: it settles the expensive axis least
        // often. Assert the actual emitted order, not just the count.
        let protocol = parse(SAMPLE).expect("valid protocol");
        let flat: Vec<(f64, f64, f64)> = protocol
            .points
            .iter()
            .filter(|point| point.block == "flat")
            .map(|point| (point.mean_u, point.frequency_hz, point.depth_a))
            .collect();
        assert_eq!(
            flat,
            vec![
                (0.5, 10.0, 0.5),
                (0.5, 10.0, 1.0),
                (0.5, 10.0, 1.5),
                (0.5, 20.0, 0.5),
                (0.5, 20.0, 1.0),
                (0.5, 20.0, 1.5),
            ]
        );
    }

    #[test]
    fn defaults_apply_unless_the_block_overrides_them() {
        let protocol = parse(SAMPLE).expect("valid protocol");
        let flat = protocol
            .points
            .iter()
            .find(|point| point.block == "flat")
            .expect("flat block");
        assert_eq!(flat.duration_s, 5);
        assert!((flat.settle_s - 1.5).abs() < f64::EPSILON);

        let ladder = protocol
            .points
            .iter()
            .find(|point| point.block == "ladder")
            .expect("ladder block");
        assert_eq!(ladder.duration_s, 30);
        // settle_s was not overridden, so the default still applies.
        assert!((ladder.settle_s - 1.5).abs() < f64::EPSILON);
        assert_eq!(protocol.camera, None);
        assert!(protocol
            .points
            .iter()
            .all(|point| point.diff_on.is_none() && point.diff_off.is_none()));
    }

    #[test]
    fn a_named_camera_profile_and_canonical_bias_offsets_are_parsed() {
        let protocol = parse(
            r#"
name = "camera-series"

[camera]
profile = "A1 low noise"

[defaults]
diff_on = 12
diff_off = -7

[[block]]
name = "first"
mean_u = 0.5
frequency_hz = 10.0
depth_a = 0.5

[[block]]
name = "override"
mean_u = 0.5
frequency_hz = 20.0
depth_a = 0.5
diff_on = 20
"#,
        )
        .expect("camera protocol");
        assert_eq!(
            protocol.camera,
            Some(CameraSelection::NamedProfile("A1 low noise".into()))
        );
        assert_eq!(protocol.points[0].diff_on, Some(12));
        assert_eq!(protocol.points[0].diff_off, Some(-7));
        assert_eq!(protocol.points[1].diff_on, Some(20));
        assert_eq!(protocol.points[1].diff_off, Some(-7));
    }

    #[test]
    fn sensor_specific_bias_ranges_are_left_to_the_host_backend() {
        let protocol = parse(
            r#"
[[block]]
mean_u = 0.5
frequency_hz = 10.0
depth_a = 0.5
diff_on = 141
diff_off = 191
"#,
        )
        .expect("the active camera backend owns its supported ranges");
        assert_eq!(protocol.points[0].diff_on, Some(141));
        assert_eq!(protocol.points[0].diff_off, Some(191));
    }

    #[test]
    fn an_inline_camera_snapshot_roundtrips_through_toml() {
        let protocol = parse(
            r#"
[camera.snapshot]
schema_version = 1
masked_pixels = [[3, 4]]

[camera.snapshot.biases]
diff_on = 12
diff_off = -7
fo = 0
hpf = 0
refr = 0

[camera.snapshot.roi]
x = 0
y = 0
width = 1280
height = 720

[camera.snapshot.digital_filter]
stc_enabled = false
stc_threshold_us = 0
trail_enabled = false

[camera.snapshot.external_trigger]
enabled = false
channel = 0

[camera.snapshot.global]
nm_per_pixel = 1000.0
pixel_scale_calibrated = true
sensor_width = 1280
sensor_height = 720
acq_time_ms = 1
event_store_budget_mib = 512
preview_interval_ms = 16
point_cloud_interval_ms = 50
disk_writer_buffer_mib = 64
record_sensor_telemetry = true

[[block]]
mean_u = 0.5
frequency_hz = 10.0
depth_a = 0.5
"#,
        )
        .expect("inline snapshot protocol");
        let Some(CameraSelection::Snapshot(snapshot)) = protocol.camera else {
            panic!("inline snapshot was not selected");
        };
        assert_eq!(snapshot.biases.diff_on, 12);
        assert!(snapshot.global.record_sensor_telemetry);
        assert_eq!(snapshot.masked_pixels, vec![(3, 4)]);
    }

    #[test]
    fn log_spacing_is_geometric() {
        let protocol = parse(SAMPLE).expect("valid protocol");
        let ladder: Vec<f64> = protocol
            .points
            .iter()
            .filter(|point| point.block == "ladder" && point.mean_u == 0.3)
            .map(|point| point.frequency_hz)
            .collect();
        assert_eq!(ladder.len(), 3);
        assert!((ladder[0] - 1.0).abs() < 1e-9);
        assert!((ladder[1] - 10.0).abs() < 1e-9);
        assert!((ladder[2] - 100.0).abs() < 1e-9);
    }

    #[test]
    fn total_seconds_counts_settling_too() {
        let protocol = parse(SAMPLE).expect("valid protocol");
        // 6 × (5 + 1.5) + 6 × (30 + 1.5)
        assert!((protocol.total_seconds() - (6.0 * 6.5 + 6.0 * 31.5)).abs() < 1e-9);
    }

    #[test]
    fn remaining_seconds_drops_the_points_already_done() {
        let protocol = parse(SAMPLE).expect("valid protocol");
        // The point in flight counts whole, so after six 6.5 s points only the
        // six 31.5 s ones are left.
        assert!((protocol.remaining_seconds(6) - 6.0 * 31.5).abs() < 1e-9);
        // Past the end nothing is left, rather than an index panic.
        assert_eq!(protocol.remaining_seconds(protocol.points.len()), 0.0);
        assert!((protocol.remaining_seconds(0) - protocol.total_seconds()).abs() < 1e-9);
    }

    #[test]
    fn out_of_range_values_name_the_axis_that_is_wrong() {
        let error = parse(
            r#"
[[block]]
name = "too-deep"
mean_u = 0.5
frequency_hz = 10.0
depth_a = 99.0
"#,
        )
        .expect_err("a depth of 99 is not drivable");
        let text = error.to_string();
        assert!(text.contains("too-deep"), "{text}");
        assert!(text.contains("depth_a"), "{text}");
    }

    #[test]
    fn log_spacing_from_zero_is_refused_rather_than_producing_infinities() {
        let error = parse(
            r#"
[[block]]
mean_u = 0.5
depth_a = 1.0
frequency_hz = { min = 0.0, max = 100.0, points = 3, spacing = "log" }
"#,
        )
        .expect_err("log from zero");
        assert!(error.to_string().contains("no zero"), "{error}");
    }

    #[test]
    fn an_empty_protocol_says_so_instead_of_running_nothing() {
        assert_eq!(
            parse("name = \"nothing\"").unwrap_err(),
            ProtocolError::Empty
        );
    }

    #[test]
    fn a_runaway_product_is_refused_before_the_bench_spends_a_night_on_it() {
        let error = parse(
            r#"
[[block]]
mean_u = { min = 0.1, max = 1.0, points = 20 }
frequency_hz = { min = 1.0, max = 100.0, points = 20 }
depth_a = { min = 0.1, max = 2.0, points = 20 }
"#,
        )
        .expect_err("8000 points");
        assert!(
            matches!(error, ProtocolError::TooManyPoints(_)),
            "{error:?}"
        );
    }

    #[test]
    fn unnamed_and_repeated_blocks_stay_distinguishable() {
        let protocol = parse(
            r#"
[[block]]
mean_u = 0.5
frequency_hz = 10.0
depth_a = 1.0

[[block]]
name = "dup"
mean_u = 0.5
frequency_hz = 10.0
depth_a = 1.0

[[block]]
name = "dup"
mean_u = 0.5
frequency_hz = 20.0
depth_a = 1.0
"#,
        )
        .expect("valid");
        let names: Vec<&str> = protocol
            .points
            .iter()
            .map(|point| point.block.as_str())
            .collect();
        assert_eq!(names, vec!["block1", "dup", "dup-2"]);
    }

    #[test]
    fn a_point_tag_is_stable_and_filename_safe() {
        let point = ProtocolPoint {
            block: "b".into(),
            mean_u: 0.5,
            frequency_hz: 12.5,
            depth_a: 0.75,
            duration_s: 5,
            settle_s: 1.0,
            role: PointRole::Normal,
            diff_on: None,
            diff_off: None,
        };
        assert_eq!(point.tag(), "u500m_f12p5Hz_a750m");
        assert!(!point.tag().contains('.'));
    }
}

#[cfg(test)]
mod csv_tests {
    use super::*;

    const SAMPLE: &str = "\
label,mean_u,frequency_hz,depth_a,duration_s,settle_s,role
floor,0.5,10,0.02,20,3,background
curve,0.5,10,0.5,,,
slow,0.4,1,0.8,40,4,
";

    #[test]
    fn one_row_is_one_recording_in_file_order() {
        let protocol = parse_csv(SAMPLE).expect("valid CSV");
        assert_eq!(protocol.points.len(), 3);
        let order: Vec<(f64, f64)> = protocol
            .points
            .iter()
            .map(|point| (point.frequency_hz, point.depth_a))
            .collect();
        assert_eq!(order, vec![(10.0, 0.02), (10.0, 0.5), (1.0, 0.8)]);
    }

    #[test]
    fn csv_protocol_identity_comes_from_its_source_filename() {
        let protocol = parse_file("a1_fc_flux_discriminator.csv", SAMPLE).expect("valid CSV");
        assert_eq!(protocol.name, "a1_fc_flux_discriminator");
        assert_eq!(protocol.version, None);
    }

    /// The reason for the row-per-recording form: a low frequency needs longer
    /// than a high one, with no block gymnastics to express it.
    #[test]
    fn each_row_carries_its_own_duration_and_settle() {
        let protocol = parse_csv(SAMPLE).expect("valid CSV");
        assert_eq!(protocol.points[0].duration_s, 20);
        assert_eq!(protocol.points[2].duration_s, 40);
        assert!((protocol.points[2].settle_s - 4.0).abs() < f64::EPSILON);
        // Blank cells fall back rather than failing the row.
        assert_eq!(protocol.points[1].duration_s, 10);
        assert!((protocol.points[1].settle_s - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn csv_carries_one_series_profile_and_per_point_diff_offsets() {
        let csv = "camera_profile,mean_u,frequency_hz,depth_a,diff_on,diff_off\n\
A1 low noise,0.5,10,0.5,12,-7\n\
A1 low noise,0.5,20,0.5,20,-8\n";
        let protocol = parse_csv(csv).expect("camera CSV");
        assert_eq!(
            protocol.camera,
            Some(CameraSelection::NamedProfile("A1 low noise".into()))
        );
        assert_eq!(protocol.points[0].diff_on, Some(12));
        assert_eq!(protocol.points[1].diff_off, Some(-8));
    }

    #[test]
    fn csv_refuses_profile_changes_within_one_measurement_series() {
        let csv = "camera_profile,mean_u,frequency_hz,depth_a\n\
profile-a,0.5,10,0.5\n\
profile-b,0.5,20,0.5\n";
        let error = parse_csv(csv).expect_err("two series profiles");
        assert!(error.to_string().contains("profile-b"), "{error}");
        assert!(error.to_string().contains("profile-a"), "{error}");
    }

    #[test]
    fn a_row_can_name_its_role_so_a_file_carries_its_own_references() {
        let protocol = parse_csv(SAMPLE).expect("valid CSV");
        assert_eq!(protocol.points[0].role, PointRole::Background);
        assert_eq!(protocol.points[1].role, PointRole::Normal);
    }

    #[test]
    fn columns_are_found_by_name_not_by_position() {
        // Someone drags a column in a spreadsheet; the file must still mean the
        // same thing.
        let reordered = "\
depth_a,role,frequency_hz,label,mean_u
0.02,background,10,floor,0.5
";
        let protocol = parse_csv(reordered).expect("valid CSV");
        assert_eq!(protocol.points[0].depth_a, 0.02);
        assert_eq!(protocol.points[0].mean_u, 0.5);
        assert_eq!(protocol.points[0].role, PointRole::Background);
        assert_eq!(protocol.points[0].block, "floor");
    }

    #[test]
    fn comments_and_blank_lines_are_skipped_so_a_file_can_explain_itself() {
        let commented = "\
# a survey
mean_u,frequency_hz,depth_a

# the only point
0.5,10,0.5
";
        assert_eq!(parse_csv(commented).expect("valid").points.len(), 1);
    }

    #[test]
    fn errors_name_the_line_number_the_editor_shows() {
        // Not a row index: the operator is looking at a spreadsheet.
        let bad = "\
# comment
mean_u,frequency_hz,depth_a
0.5,10,0.5
0.5,10,99
";
        let error = parse_csv(bad).expect_err("a depth of 99 is not drivable");
        let text = error.to_string();
        // Line 4 counting the comment and the header, which is what an editor
        // and a spreadsheet both show.
        assert!(text.contains("line 4"), "{text}");
        assert!(text.contains("depth_a"), "{text}");
    }

    #[test]
    fn a_missing_required_column_says_which_one_and_lists_the_rest() {
        let error = parse_csv("mean_u,frequency_hz\n0.5,10\n").expect_err("no depth_a");
        let text = error.to_string();
        assert!(text.contains("depth_a"), "{text}");
        assert!(
            text.contains("duration_s"),
            "optional columns unlisted: {text}"
        );
    }

    #[test]
    fn a_header_only_or_empty_file_is_refused_rather_than_running_nothing() {
        assert_eq!(
            parse_csv("mean_u,frequency_hz,depth_a\n").unwrap_err(),
            ProtocolError::Empty
        );
        assert!(matches!(
            parse_csv("# nothing but a comment\n").unwrap_err(),
            ProtocolError::Invalid { .. }
        ));
    }

    #[test]
    fn a_quoted_label_may_contain_a_comma() {
        let quoted = "label,mean_u,frequency_hz,depth_a\n\"ladder, low end\",0.5,1,0.8\n";
        let protocol = parse_csv(quoted).expect("valid");
        assert_eq!(protocol.points[0].block, "ladder, low end");
    }

    #[test]
    fn an_unlabelled_row_still_gets_a_stable_name() {
        let protocol =
            parse_csv("mean_u,frequency_hz,depth_a\n0.5,10,0.5\n0.5,20,0.5\n").expect("valid");
        assert_eq!(protocol.points[0].block, "row1");
        assert_eq!(protocol.points[1].block, "row2");
    }

    #[test]
    fn an_unknown_role_is_refused_rather_than_silently_recorded_as_normal() {
        let error = parse_csv("mean_u,frequency_hz,depth_a,role\n0.5,10,0.5,piolt\n")
            .expect_err("typo in role");
        assert!(error.to_string().contains("pilot"), "{error}");
    }

    #[test]
    fn the_reader_is_chosen_by_extension() {
        let csv = "mean_u,frequency_hz,depth_a\n0.5,10,0.5\n";
        assert_eq!(parse_file("survey.csv", csv).expect("csv").points.len(), 1);
        assert_eq!(parse_file("SURVEY.CSV", csv).expect("csv").points.len(), 1);
        // A .toml path goes to the block reader, and the CSV text is not TOML.
        assert!(parse_file("survey.toml", csv).is_err());
    }
}

#[cfg(test)]
mod example_file_tests {
    use super::*;

    /// The shipped example is documentation the operator copies, so it has to
    /// stay valid as the format moves — a stale example is worse than none.
    /// The shipped CSV is what an operator copies, so it has to stay valid as
    /// the format moves — a stale example is worse than none.
    #[test]
    fn the_shipped_example_csv_parses_and_exercises_every_column() {
        let text = include_str!("../protocols/example.csv");
        let protocol = parse_csv(text).expect("the shipped CSV example must parse");
        assert!(protocol.points.len() > 10);
        assert!(protocol
            .points
            .iter()
            .any(|point| point.role == PointRole::Background));
        assert!(protocol
            .points
            .iter()
            .any(|point| point.role == PointRole::Pilot));
        // The whole reason for the row form: durations genuinely differ.
        let durations: std::collections::BTreeSet<i64> = protocol
            .points
            .iter()
            .map(|point| point.duration_s)
            .collect();
        assert!(durations.len() > 2, "{durations:?}");
        // And every axis is exercised.
        let (means, frequencies, depths) = protocol.axis_counts();
        assert!(means > 1 && frequencies > 1 && depths > 1);
    }

    #[test]
    fn the_shipped_example_protocol_parses() {
        let text = include_str!("../protocols/example.toml");
        let protocol = parse(text).expect("the shipped example must parse");
        assert_eq!(protocol.name, "a1-example-survey");
        // 1×1×7 + 2×6×1 + 1×3×4 + 4×1×1
        assert_eq!(protocol.points.len(), 7 + 12 + 12 + 4);
        // Every axis is genuinely exercised, so the example demonstrates what
        // it claims to.
        let (means, frequencies, depths) = protocol.axis_counts();
        assert!(means > 1 && frequencies > 1 && depths > 1);
        assert!(protocol.total_seconds() > 0.0);
    }
}
