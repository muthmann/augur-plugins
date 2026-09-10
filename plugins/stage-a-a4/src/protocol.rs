//! Declarative threshold protocols: a file naming the bias points to record,
//! expanded into the flat list the runner walks.
//!
//! A4 holds the optical condition still and sweeps the sensor. Every row states
//! one `(diff_on, diff_off)` pair, how long to record it, how long to settle
//! first, and how many times to repeat it — plus the QC limits that row is
//! judged against and the optical state it was taken under, so the file is a
//! complete description of the survey six months later.
//!
//! ## CSV — one row per recording
//!
//! ```csv
//! label,optical_state,diff_on,diff_off,duration_s,settle_s,repeats
//! threshold-01,LP647+BP700,-20,-10,60,5,2
//! threshold-02,LP647+BP700,0,0,60,5,2
//! threshold-03,LP647+BP700,20,20,60,5,2
//! ```
//!
//! Only `diff_on` and `diff_off` are required. Columns are found **by header
//! name**, so their order does not matter and any of the optional ones may be
//! left out entirely. Optional columns: `label`, `optical_state`, `duration_s`,
//! `settle_s`, `repeats`, `pause_before`, `max_temperature_drift_c`,
//! `max_illumination_drift_percent`, `max_event_rate`, `filter_id`, `flux_id`.
//!
//! ## TOML — blocks and ranges
//!
//! ```toml
//! name = "a4-threshold"
//!
//! [defaults]
//! duration_s     = 60
//! settle_s       = 5
//! repeats        = 2
//! optical_state  = "LP647+BP700"
//!
//! [[block]]
//! name     = "on-sweep"
//! diff_on  = { min = -20, max = 20, step = 10 }
//! diff_off = 0
//! ```
//!
//! An axis is a single value, an explicit list, or an inclusive
//! `{ min, max, step }` range (`step` defaults to 1). Bias codes are integers,
//! so a range is stated by its step rather than by a point count — asking for
//! "5 points from -20 to 20" would have to invent a spacing, and the one it
//! invented would not be a code the operator chose.
//!
//! A block expands to the **product** of its two axes, which is the 2D
//! threshold map. A symmetric sweep — where `diff_on` and `diff_off` move
//! together — is a set of specific pairs, not a product, so it belongs in the
//! CSV form where each pair is written out.
//!
//! ## Ordering
//!
//! Points come out in file order, `diff_on` outermost within a block, and each
//! row's repeats consecutively. Nothing is reordered: a threshold survey drifts
//! with the bench, so the order the operator wrote is the order that has to be
//! defensible against the temperature log.

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;

use stage_a_plugin_contract::csv::split_line;

/// Hard ceiling on the recordings one protocol may expand to. Repeats multiply,
/// so an operator who typed one zero too many should be told on the button
/// press rather than after the bench has spent a night on it.
pub const MAX_POINTS: usize = 4_096;

/// Bias offset window the host accepts (and the IMX636 driver behind it).
/// Checked here so a bad value names its own line instead of surfacing as a
/// rejected command on point 37.
pub const BIAS_OFFSET_RANGE: (i64, i64) = (-85, 140);
const DURATION_RANGE: (i64, i64) = (1, 3_600);
const SETTLE_RANGE: (f64, f64) = (0.0, 600.0);
const REPEATS_RANGE: (i64, i64) = (1, 100);

/// Stability limits one point is judged against.
///
/// Every limit is optional and every one is a **flag, not a gate**: a breach is
/// recorded in the point's sidecar and the run summary, and the recording is
/// still kept. A threshold survey that silently dropped its drifting points
/// would hide exactly the evidence needed to decide whether the drift mattered.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct QcLimits {
    /// Maximum |T − T_start| over the recording, in °C.
    pub max_temperature_drift_c: Option<f64>,
    /// Maximum |lux − lux_start| / lux_start over the recording, in percent.
    pub max_illumination_drift_percent: Option<f64>,
    /// Maximum mean event rate over the recording, in events per second.
    pub max_event_rate: Option<f64>,
}

impl QcLimits {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One recording the protocol asks for, with every parameter resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct A4Point {
    /// Where this row came from — a CSV `label` or the `[[block]]` name — for
    /// the status line, the file stem and the sidecar.
    pub label: String,
    /// Free text naming the optical condition: filters, dark cap, flux. A4
    /// never changes it; it is recorded so two points can be shown to have been
    /// taken under the same one.
    pub optical_state: String,
    /// Bias offsets around the factory trim, as the host settings panel
    /// expresses them. The absolute codes are read back from the sensor.
    pub diff_on: i64,
    pub diff_off: i64,
    pub duration_s: i64,
    pub settle_s: f64,
    /// Which repeat of its row this is, and how many there are: `(1, 2)` is the
    /// first of two. `(1, 1)` for a row recorded once.
    pub repeat: (u32, u32),
    /// Stop and wait for the operator before this point — a filter change or a
    /// dark cap. The run does not continue until Continue is pressed.
    pub pause_before: bool,
    pub limits: QcLimits,
    pub filter_id: String,
    pub flux_id: String,
}

impl A4Point {
    /// Filename fragment identifying this point inside the measurement folder.
    ///
    /// Signed offsets are rendered with an explicit `p`/`m` rather than a
    /// leading `-`, so a stem never starts a shell argument with a dash and
    /// sorts the way it reads.
    pub fn tag(&self) -> String {
        let mut tag = format!(
            "on{}_off{}",
            signed_tag(self.diff_on),
            signed_tag(self.diff_off)
        );
        if self.repeat.1 > 1 {
            tag.push_str(&format!("_r{:02}", self.repeat.0));
        }
        tag
    }
}

fn signed_tag(value: i64) -> String {
    if value < 0 {
        format!("m{}", value.unsigned_abs())
    } else {
        format!("p{value}")
    }
}

/// A parsed protocol: what to record, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct Protocol {
    pub preserve_current: bool,
    pub name: String,
    pub points: Vec<A4Point>,
}

impl Protocol {
    /// Distinct values on each bias axis, for the summary shown before starting.
    pub fn axis_counts(&self) -> (usize, usize) {
        let count = |values: Vec<i64>| {
            let mut values = values;
            values.sort_unstable();
            values.dedup();
            values.len()
        };
        (
            count(self.points.iter().map(|point| point.diff_on).collect()),
            count(self.points.iter().map(|point| point.diff_off).collect()),
        )
    }

    /// Total bench time the protocol asks for, settling included. The bias
    /// handshake per point is not in this number, so it reads a little short.
    pub fn total_seconds(&self) -> f64 {
        self.points
            .iter()
            .map(|point| point.duration_s as f64 + point.settle_s)
            .sum()
    }

    /// Whether any row asks the operator to intervene. A survey with a pause in
    /// it cannot be left alone, and the panel should say so before it starts.
    pub fn has_pauses(&self) -> bool {
        self.points.iter().any(|point| point.pause_before)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProtocolError {
    Toml(String),
    /// A named row, column, block or default is unusable, with the reason.
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
                "the protocol has no points to record — add at least one row with a diff_on \
                 and a diff_off",
            ),
            Self::TooManyPoints(count) => write!(
                f,
                "the protocol expands to {count} recordings, past the {MAX_POINTS} limit — \
                 narrow an axis, lower the repeats, or split it into several files"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// Parses a protocol, choosing the form from the file extension.
pub fn parse_file(path: &str, text: &str) -> Result<Protocol, ProtocolError> {
    let is_csv = std::path::Path::new(path)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("csv"));
    if is_csv {
        parse_csv(text)
    } else {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| ProtocolError::Toml(e.to_string()))?;
        if value.get("mode").and_then(toml::Value::as_str) == Some("current_reference") {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Threshold {
                diff_on: i64,
                diff_off: i64,
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Reference {
                mode: String,
                name: String,
                duration_s: i64,
                settle_s: f64,
                repeats: i64,
                #[serde(default)]
                threshold: Vec<Threshold>,
            }
            let reference: Reference =
                toml::from_str(text).map_err(|e| ProtocolError::Toml(e.to_string()))?;
            let _ = reference.mode;
            let rows = if reference.threshold.is_empty() {
                vec![(0, 0)]
            } else {
                reference.threshold.iter().map(|p| (p.diff_on, p.diff_off)).collect()
            };
            let csv = std::iter::once("diff_on,diff_off,duration_s,settle_s,repeats".to_owned())
                .chain(rows.into_iter().map(|(diff_on, diff_off)| format!(
                    "{diff_on},{diff_off},{},{},{}",
                    reference.duration_s, reference.settle_s, reference.repeats
                )))
                .collect::<Vec<_>>()
                .join("\n");
            let mut plan = parse_csv(&csv)?;
            plan.name = reference.name;
            plan.preserve_current = true;
            for point in &mut plan.points {
                point.label = "bright-reference".into();
                point.optical_state = "constant light; unchanged A1-A3 attenuation".into();
                point.flux_id = "BRIGHT_REFERENCE".into();
            }
            Ok(plan)
        } else {
            parse_toml(text)
        }
    }
}

// ---- CSV form --------------------------------------------------------------

const CSV_REQUIRED: [&str; 2] = ["diff_on", "diff_off"];
const CSV_OPTIONAL: [&str; 10] = [
    "label",
    "optical_state",
    "duration_s",
    "settle_s",
    "repeats",
    "pause_before",
    "max_temperature_drift_c",
    "max_illumination_drift_percent",
    "max_event_rate",
    "filter_id",
];

/// Parses the row-per-recording CSV form.
///
/// Columns are located **by header name**, so their order does not matter and a
/// column can be left out entirely — which is what keeps a file working after
/// someone drags a column in a spreadsheet. Blank lines and `#` comments are
/// skipped so a file can explain itself, and errors carry the **file line
/// number** because that is what an editor and a spreadsheet both show.
pub fn parse_csv(text: &str) -> Result<Protocol, ProtocolError> {
    let mut header: Option<Vec<String>> = None;
    let mut points = Vec::new();

    // `lines()` already absorbs CRLF; the BOM is what it leaves behind.
    for (offset, raw) in strip_bom(text).lines().enumerate() {
        let line_no = offset + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields = split_line(raw);

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
                            "has no '{required}' column. Required: {}. Optional: {}, flux_id",
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
        let invalid = |name: &str, detail: String| ProtocolError::Invalid {
            what: format!("line {line_no}: {name}"),
            detail,
        };
        let integer = |name: &str, range: (i64, i64)| -> Result<Option<i64>, ProtocolError> {
            let raw = cell(name).unwrap_or("");
            if raw.is_empty() {
                return Ok(None);
            }
            let value: i64 = raw
                .parse()
                .map_err(|_| invalid(name, format!("'{raw}' is not a whole number")))?;
            check_range_i64(name, value, range).map(Some)
        };
        let float = |name: &str, range: (f64, f64)| -> Result<Option<f64>, ProtocolError> {
            let raw = cell(name).unwrap_or("");
            if raw.is_empty() {
                return Ok(None);
            }
            let value: f64 = raw
                .parse()
                .map_err(|_| invalid(name, format!("'{raw}' is not a number")))?;
            check_range_f64(name, value, range).map(Some)
        };

        let diff_on = integer("diff_on", BIAS_OFFSET_RANGE)?
            .ok_or_else(|| invalid("diff_on", "is empty".into()))?;
        let diff_off = integer("diff_off", BIAS_OFFSET_RANGE)?
            .ok_or_else(|| invalid("diff_off", "is empty".into()))?;
        let duration_s = integer("duration_s", DURATION_RANGE)?.unwrap_or(60);
        let settle_s = float("settle_s", SETTLE_RANGE)?.unwrap_or(5.0);
        let repeats = integer("repeats", REPEATS_RANGE)?.unwrap_or(1) as u32;
        let pause_before = parse_bool(cell("pause_before").unwrap_or(""))
            .ok_or_else(|| invalid("pause_before", "is not yes/no".into()))?;

        let limits = QcLimits {
            max_temperature_drift_c: float("max_temperature_drift_c", (0.0, 1_000.0))?,
            max_illumination_drift_percent: float(
                "max_illumination_drift_percent",
                (0.0, 100_000.0),
            )?,
            max_event_rate: float("max_event_rate", (0.0, 1e12))?,
        };

        let label = cell("label").unwrap_or("").trim().to_owned();
        let label = if label.is_empty() {
            format!("row{}", points.len() + 1)
        } else {
            label
        };
        push_repeats(
            &mut points,
            A4Point {
                label,
                optical_state: cell("optical_state").unwrap_or("").to_owned(),
                diff_on,
                diff_off,
                duration_s,
                settle_s,
                repeat: (1, repeats),
                pause_before,
                limits,
                filter_id: cell("filter_id").unwrap_or("").to_owned(),
                flux_id: cell("flux_id").unwrap_or("").to_owned(),
            },
            repeats,
        )?;
    }

    if header.is_none() {
        return Err(ProtocolError::Invalid {
            what: "the protocol file".into(),
            detail: format!(
                "has no header line. The first line that is not blank or a # comment must name \
                 the columns, at least: {}",
                CSV_REQUIRED.join(", ")
            ),
        });
    }
    if points.is_empty() {
        return Err(ProtocolError::Empty);
    }
    Ok(Protocol {
        preserve_current: false,
        name: "protocol".to_owned(),
        points,
    })
}

/// A repeated row is N recordings, not one recorded N times: each gets its own
/// file, its own sidecar and its own QC verdict, because the drift between two
/// repeats is one of the things the survey is measuring.
fn push_repeats(
    points: &mut Vec<A4Point>,
    point: A4Point,
    repeats: u32,
) -> Result<(), ProtocolError> {
    for index in 1..=repeats.max(1) {
        let mut repeat = point.clone();
        repeat.repeat = (index, repeats.max(1));
        // Only the first repeat stops for the operator: the filter is already
        // changed by the time the second one starts.
        repeat.pause_before = point.pause_before && index == 1;
        points.push(repeat);
        if points.len() > MAX_POINTS {
            return Err(ProtocolError::TooManyPoints(points.len()));
        }
    }
    Ok(())
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "no" | "false" | "n" => Some(false),
        "1" | "yes" | "true" | "y" => Some(true),
        _ => None,
    }
}

fn check_range_i64(name: &str, value: i64, range: (i64, i64)) -> Result<i64, ProtocolError> {
    if value < range.0 || value > range.1 {
        return Err(ProtocolError::Invalid {
            what: name.to_owned(),
            detail: format!("{value} is outside the supported {}..={}", range.0, range.1),
        });
    }
    Ok(value)
}

fn check_range_f64(name: &str, value: f64, range: (f64, f64)) -> Result<f64, ProtocolError> {
    if !value.is_finite() || value < range.0 || value > range.1 {
        return Err(ProtocolError::Invalid {
            what: name.to_owned(),
            detail: format!("{value} is outside the supported {}..={}", range.0, range.1),
        });
    }
    Ok(value)
}

// ---- TOML form -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProtocolDoc {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    defaults: Defaults,
    #[serde(default, rename = "block")]
    blocks: Vec<BlockDoc>,
}

#[derive(Debug, Default, Deserialize)]
struct Defaults {
    #[serde(default)]
    duration_s: Option<i64>,
    #[serde(default)]
    settle_s: Option<f64>,
    #[serde(default)]
    repeats: Option<i64>,
    #[serde(default)]
    optical_state: Option<String>,
    #[serde(default)]
    filter_id: Option<String>,
    #[serde(default)]
    flux_id: Option<String>,
    #[serde(default)]
    max_temperature_drift_c: Option<f64>,
    #[serde(default)]
    max_illumination_drift_percent: Option<f64>,
    #[serde(default)]
    max_event_rate: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct BlockDoc {
    #[serde(default)]
    name: Option<String>,
    diff_on: Axis,
    diff_off: Axis,
    #[serde(default)]
    duration_s: Option<i64>,
    #[serde(default)]
    settle_s: Option<f64>,
    #[serde(default)]
    repeats: Option<i64>,
    #[serde(default)]
    optical_state: Option<String>,
    #[serde(default)]
    filter_id: Option<String>,
    #[serde(default)]
    flux_id: Option<String>,
    #[serde(default)]
    pause_before: Option<bool>,
    #[serde(default)]
    max_temperature_drift_c: Option<f64>,
    #[serde(default)]
    max_illumination_drift_percent: Option<f64>,
    #[serde(default)]
    max_event_rate: Option<f64>,
}

/// One bias axis: a single value, an explicit list, or an inclusive range.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Axis {
    One(i64),
    List(Vec<i64>),
    Range {
        min: i64,
        max: i64,
        step: Option<i64>,
    },
}

impl Axis {
    /// Expand to the values to visit, in the order they are recorded.
    fn values(&self, what: &str) -> Result<Vec<i64>, ProtocolError> {
        let invalid = |detail: String| ProtocolError::Invalid {
            what: what.to_owned(),
            detail,
        };
        let values = match self {
            Self::One(value) => vec![*value],
            Self::List(values) => {
                if values.is_empty() {
                    return Err(invalid("is an empty list".into()));
                }
                values.clone()
            }
            Self::Range { min, max, step } => {
                let step = step.unwrap_or(1);
                if step <= 0 {
                    return Err(invalid(format!("step {step} must be positive")));
                }
                if max < min {
                    return Err(invalid(format!("max {max} is below min {min}")));
                }
                // Inclusive of `min`, and of `max` when the step lands on it.
                // A range whose step overshoots simply stops early rather than
                // silently recording a point the file never named.
                let mut values = Vec::new();
                let mut value = *min;
                while value <= *max {
                    values.push(value);
                    value += step;
                }
                values
            }
        };
        for value in &values {
            check_range_i64(what, *value, BIAS_OFFSET_RANGE)?;
        }
        Ok(values)
    }
}

/// Parses the block/range TOML form and expands it into points.
pub fn parse_toml(text: &str) -> Result<Protocol, ProtocolError> {
    let doc: ProtocolDoc =
        toml::from_str(strip_bom(text)).map_err(|error| ProtocolError::Toml(error.to_string()))?;

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
            format!("{base}#{occurrence}")
        };

        let duration_s = check_range_i64(
            &format!("block '{name}': duration_s"),
            block.duration_s.or(doc.defaults.duration_s).unwrap_or(60),
            DURATION_RANGE,
        )?;
        let settle_s = check_range_f64(
            &format!("block '{name}': settle_s"),
            block.settle_s.or(doc.defaults.settle_s).unwrap_or(5.0),
            SETTLE_RANGE,
        )?;
        let repeats = check_range_i64(
            &format!("block '{name}': repeats"),
            block.repeats.or(doc.defaults.repeats).unwrap_or(1),
            REPEATS_RANGE,
        )? as u32;

        let limits = QcLimits {
            max_temperature_drift_c: block
                .max_temperature_drift_c
                .or(doc.defaults.max_temperature_drift_c),
            max_illumination_drift_percent: block
                .max_illumination_drift_percent
                .or(doc.defaults.max_illumination_drift_percent),
            max_event_rate: block.max_event_rate.or(doc.defaults.max_event_rate),
        };
        let optical_state = block
            .optical_state
            .clone()
            .or_else(|| doc.defaults.optical_state.clone())
            .unwrap_or_default();
        let filter_id = block
            .filter_id
            .clone()
            .or_else(|| doc.defaults.filter_id.clone())
            .unwrap_or_default();
        let flux_id = block
            .flux_id
            .clone()
            .or_else(|| doc.defaults.flux_id.clone())
            .unwrap_or_default();

        let on_values = block.diff_on.values(&format!("block '{name}': diff_on"))?;
        let off_values = block
            .diff_off
            .values(&format!("block '{name}': diff_off"))?;
        // `diff_on` outermost: a block is the 2D threshold map, walked one ON
        // row at a time.
        let mut first_of_block = true;
        for diff_on in &on_values {
            for diff_off in &off_values {
                push_repeats(
                    &mut points,
                    A4Point {
                        label: name.clone(),
                        optical_state: optical_state.clone(),
                        diff_on: *diff_on,
                        diff_off: *diff_off,
                        duration_s,
                        settle_s,
                        repeat: (1, repeats),
                        // A block-level pause is about the optical condition
                        // the whole block shares, so it stops once, before the
                        // block, not before each of its points.
                        pause_before: block.pause_before.unwrap_or(false) && first_of_block,
                        limits,
                        filter_id: filter_id.clone(),
                        flux_id: flux_id.clone(),
                    },
                    repeats,
                )?;
                first_of_block = false;
            }
        }
    }

    if points.is_empty() {
        return Err(ProtocolError::Empty);
    }
    Ok(Protocol {
        preserve_current: false,
        name: doc
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "protocol".to_owned()),
        points,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "\
label,optical_state,diff_on,diff_off,duration_s,settle_s,repeats
threshold-01,LP647+BP700,-20,-10,60,5,2
threshold-02,LP647+BP700,0,0,60,5,2
threshold-03,LP647+BP700,20,20,60,5,2
";

    #[test]
    fn the_requirements_example_parses_into_six_recordings() {
        let protocol = parse_file("a4.csv", CSV).expect("valid protocol");
        // Three rows, two repeats each.
        assert_eq!(protocol.points.len(), 6);
        assert_eq!(protocol.points[0].label, "threshold-01");
        assert_eq!(protocol.points[0].diff_on, -20);
        assert_eq!(protocol.points[0].diff_off, -10);
        assert_eq!(protocol.points[0].duration_s, 60);
        assert_eq!(protocol.points[0].optical_state, "LP647+BP700");
        assert!((protocol.points[0].settle_s - 5.0).abs() < f64::EPSILON);
        assert_eq!(protocol.axis_counts(), (3, 3));
    }

    #[test]
    fn repeats_are_separate_recordings_numbered_in_order() {
        // Each repeat is its own file and its own QC verdict — the drift
        // between two repeats is part of what the survey measures.
        let protocol = parse_file("a4.csv", CSV).expect("valid protocol");
        let first_row: Vec<(u32, u32)> = protocol.points[..2]
            .iter()
            .map(|point| point.repeat)
            .collect();
        assert_eq!(first_row, vec![(1, 2), (2, 2)]);
        assert_eq!(protocol.points[0].tag(), "onm20_offm10_r01");
        assert_eq!(protocol.points[1].tag(), "onm20_offm10_r02");
    }

    #[test]
    fn a_row_recorded_once_carries_no_repeat_suffix() {
        let csv = "diff_on,diff_off\n0,0\n";
        let protocol = parse_file("a4.csv", csv).expect("valid protocol");
        assert_eq!(protocol.points[0].repeat, (1, 1));
        assert_eq!(protocol.points[0].tag(), "onp0_offp0");
    }

    #[test]
    fn a_negative_offset_never_starts_a_stem_with_a_dash() {
        let csv = "diff_on,diff_off\n-85,140\n";
        let protocol = parse_file("a4.csv", csv).expect("valid protocol");
        let tag = protocol.points[0].tag();
        assert_eq!(tag, "onm85_offp140");
        assert!(!tag.starts_with('-'), "{tag}");
    }

    #[test]
    fn only_the_two_bias_columns_are_required() {
        let csv = "diff_off,diff_on\n5,-5\n";
        let protocol = parse_file("a4.csv", csv).expect("column order must not matter");
        assert_eq!(protocol.points[0].diff_on, -5);
        assert_eq!(protocol.points[0].diff_off, 5);
        // The rest fall back rather than refusing.
        assert_eq!(protocol.points[0].duration_s, 60);
        assert_eq!(protocol.points[0].repeat, (1, 1));
        assert!(protocol.points[0].limits.is_empty());
    }

    #[test]
    fn a_missing_bias_column_names_itself_and_the_line() {
        let csv = "label,diff_on\nx,0\n";
        let error = parse_file("a4.csv", csv).expect_err("diff_off is required");
        let text = error.to_string();
        assert!(text.contains("diff_off"), "{text}");
        assert!(text.contains("line 1"), "{text}");
    }

    #[test]
    fn an_offset_the_host_would_reject_is_refused_at_parse_time() {
        // The point of checking here: the operator finds out on the button
        // press, not when point 37 is rejected at 3 a.m.
        let csv = "diff_on,diff_off\n0,200\n";
        let error = parse_file("a4.csv", csv).expect_err("200 is out of range");
        let text = error.to_string();
        assert!(text.contains("diff_off"), "{text}");
        assert!(text.contains("-85..=140"), "{text}");
    }

    #[test]
    fn optional_qc_limits_are_read_per_row() {
        let csv = "diff_on,diff_off,max_temperature_drift_c,max_event_rate\n\
                   0,0,1.5,250000\n\
                   10,10,,\n";
        let protocol = parse_file("a4.csv", csv).expect("valid protocol");
        assert_eq!(protocol.points[0].limits.max_temperature_drift_c, Some(1.5));
        assert_eq!(protocol.points[0].limits.max_event_rate, Some(250_000.0));
        // An empty cell is "no limit", not zero — a zero limit would flag
        // every point.
        assert!(protocol.points[1].limits.is_empty());
    }

    #[test]
    fn a_pause_stops_once_per_row_not_once_per_repeat() {
        // The filter is already changed by the time the second repeat starts.
        let csv = "diff_on,diff_off,repeats,pause_before\n0,0,3,yes\n";
        let protocol = parse_file("a4.csv", csv).expect("valid protocol");
        let pauses: Vec<bool> = protocol.points.iter().map(|p| p.pause_before).collect();
        assert_eq!(pauses, vec![true, false, false]);
        assert!(protocol.has_pauses());
    }

    #[test]
    fn comments_and_blank_lines_let_a_file_explain_itself() {
        let csv = "# threshold survey, 2026-08-07\n\
                   \n\
                   diff_on,diff_off\n\
                   # the symmetric points\n\
                   0,0\n\
                   \n\
                   10,10\n";
        let protocol = parse_file("a4.csv", csv).expect("valid protocol");
        assert_eq!(protocol.points.len(), 2);
    }

    #[test]
    fn a_spreadsheet_bom_and_crlf_do_not_hide_the_first_column() {
        let csv = "\u{feff}diff_on,diff_off\r\n-20,-10\r\n";
        let protocol = parse_file("a4.csv", csv).expect("BOM + CRLF CSV");
        assert_eq!(protocol.points.len(), 1);
        assert_eq!(protocol.points[0].diff_on, -20);
    }

    const TOML: &str = r#"
name = "a4-map"

[defaults]
duration_s    = 30
settle_s      = 2.0
optical_state = "LP647+BP700"
max_temperature_drift_c = 2.0

[[block]]
name     = "on-sweep"
diff_on  = { min = -20, max = 20, step = 10 }
diff_off = 0

[[block]]
name     = "corner"
diff_on  = [30, 40]
diff_off = [30, 40]
repeats  = 2
"#;

    #[test]
    fn a_block_expands_to_the_product_of_its_two_bias_axes() {
        let protocol = parse_toml(TOML).expect("valid protocol");
        assert_eq!(protocol.name, "a4-map");
        // 5 × 1 + (2 × 2) × 2 repeats
        assert_eq!(protocol.points.len(), 5 + 8);
    }

    #[test]
    fn a_range_is_inclusive_and_walks_diff_on_outermost() {
        let protocol = parse_toml(TOML).expect("valid protocol");
        let sweep: Vec<i64> = protocol
            .points
            .iter()
            .filter(|point| point.label == "on-sweep")
            .map(|point| point.diff_on)
            .collect();
        assert_eq!(sweep, vec![-20, -10, 0, 10, 20]);

        let corner: Vec<(i64, i64)> = protocol
            .points
            .iter()
            .filter(|point| point.label == "corner" && point.repeat.0 == 1)
            .map(|point| (point.diff_on, point.diff_off))
            .collect();
        assert_eq!(corner, vec![(30, 30), (30, 40), (40, 30), (40, 40)]);
    }

    #[test]
    fn block_values_override_the_defaults_they_do_not_replace_them() {
        let protocol = parse_toml(TOML).expect("valid protocol");
        let corner = protocol
            .points
            .iter()
            .find(|point| point.label == "corner")
            .expect("corner block");
        // `repeats` was overridden; everything else still comes from defaults.
        assert_eq!(corner.repeat, (1, 2));
        assert_eq!(corner.duration_s, 30);
        assert_eq!(corner.optical_state, "LP647+BP700");
        assert_eq!(corner.limits.max_temperature_drift_c, Some(2.0));
    }

    #[test]
    fn two_blocks_with_one_name_stay_distinguishable() {
        let text = r#"
[[block]]
name = "sweep"
diff_on = 0
diff_off = 0

[[block]]
name = "sweep"
diff_on = 10
diff_off = 10
"#;
        let protocol = parse_toml(text).expect("valid protocol");
        let labels: Vec<&str> = protocol
            .points
            .iter()
            .map(|point| point.label.as_str())
            .collect();
        assert_eq!(labels, vec!["sweep", "sweep#2"]);
    }

    #[test]
    fn an_empty_protocol_says_what_to_add() {
        assert_eq!(parse_toml("name = \"x\"\n"), Err(ProtocolError::Empty));
        let error = parse_file("a4.csv", "diff_on,diff_off\n").expect_err("no rows");
        assert_eq!(error, ProtocolError::Empty);
    }

    #[test]
    fn a_protocol_too_large_to_run_is_refused_before_the_bench_starts() {
        let text = "[[block]]\ndiff_on = { min = -85, max = 140, step = 1 }\n\
                    diff_off = { min = -85, max = 140, step = 1 }\n";
        let error = parse_toml(text).expect_err("226 × 226 is far past the limit");
        assert!(matches!(error, ProtocolError::TooManyPoints(_)));
    }

    #[test]
    fn a_reversed_or_zero_step_range_names_the_block_it_is_in() {
        let text = "[[block]]\nname = \"bad\"\ndiff_on = { min = 20, max = 0 }\ndiff_off = 0\n";
        let error = parse_toml(text).expect_err("max below min");
        let message = error.to_string();
        assert!(message.contains("bad"), "{message}");
        assert!(message.contains("diff_on"), "{message}");

        let text = "[[block]]\ndiff_on = { min = 0, max = 20, step = 0 }\ndiff_off = 0\n";
        let error = parse_toml(text).expect_err("zero step");
        assert!(error.to_string().contains("step"), "{error}");
    }

    #[test]
    fn total_bench_time_counts_every_repeat() {
        let protocol = parse_file("a4.csv", CSV).expect("valid protocol");
        // 6 recordings × (60 s + 5 s)
        assert!((protocol.total_seconds() - 390.0).abs() < f64::EPSILON);
    }
}

#[cfg(test)]
mod shipped_protocol_tests {
    use super::*;

    /// The files under `protocols/` are what an operator copies to start from.
    /// A broken example is worse than none, so they are parsed as fixtures.
    fn shipped(name: &str) -> Protocol {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/protocols/");
        let full = format!("{path}{name}");
        let text = std::fs::read_to_string(&full)
            .unwrap_or_else(|error| panic!("{name} must be readable: {error}"));
        parse_file(&full, &text).unwrap_or_else(|error| panic!("{name} must parse: {error}"))
    }

    #[test]
    fn the_symmetric_example_is_five_pairs_recorded_twice() {
        let protocol = shipped("example.csv");
        assert_eq!(protocol.points.len(), 10);
        // Symmetric by construction — that is what the file is demonstrating.
        for point in &protocol.points {
            assert_eq!(point.diff_on, point.diff_off, "{}", point.label);
        }
        assert!(!protocol.has_pauses());
    }

    #[test]
    fn the_asymmetric_example_pauses_once_for_the_dark_cap() {
        let protocol = shipped("example_asymmetric.csv");
        assert_eq!(protocol.points.len(), 6);
        let paused: Vec<&str> = protocol
            .points
            .iter()
            .filter(|point| point.pause_before)
            .map(|point| point.label.as_str())
            .collect();
        assert_eq!(paused, vec!["dark-01"]);
        assert_eq!(
            protocol.points[4].limits.max_event_rate,
            Some(50_000.0),
            "the dark rows carry a much tighter rate limit"
        );
    }

    #[test]
    fn the_toml_example_expands_to_the_map_it_documents() {
        let protocol = shipped("example.toml");
        assert_eq!(protocol.name, "a4-threshold-map");
        // 5×5 coarse + 5×1 centre × 2 repeats + 2×2 dark
        assert_eq!(protocol.points.len(), 25 + 10 + 4);
        // diff_on: the coarse five plus the four centre-detail values;
        // diff_off: the coarse five, which the other blocks stay inside.
        assert_eq!(protocol.axis_counts(), (9, 5));
        // Every point inherits the defaults it does not override.
        let coarse = protocol
            .points
            .iter()
            .find(|point| point.label == "coarse-map")
            .expect("coarse block");
        assert_eq!(coarse.optical_state, "LP647+BP700");
        assert_eq!(coarse.duration_s, 60);
        assert_eq!(coarse.limits.max_temperature_drift_c, Some(2.0));
    }

    #[test]
    fn every_shipped_protocol_stays_inside_the_hosts_bias_range() {
        // The parser enforces this, so a passing parse is the assertion; this
        // states the intent so the reason is not lost.
        for name in ["example.csv", "example_asymmetric.csv", "example.toml"] {
            for point in shipped(name).points {
                assert!(
                    (BIAS_OFFSET_RANGE.0..=BIAS_OFFSET_RANGE.1).contains(&point.diff_on),
                    "{name}: {}",
                    point.label
                );
                assert!(
                    (BIAS_OFFSET_RANGE.0..=BIAS_OFFSET_RANGE.1).contains(&point.diff_off),
                    "{name}: {}",
                    point.label
                );
            }
        }
    }
}

#[cfg(test)]
mod reference_tests {
    use super::*;
    #[test]
    fn matched_reference_preserves_camera_and_takes_six_minutes_plus_settling() {
        let p = parse_file(
            "reference.toml",
            include_str!("../protocols/a4_bright_reference.toml"),
        )
        .unwrap();
        assert!(p.preserve_current);
        assert_eq!(p.points.len(), 15);
        assert_eq!(p.total_seconds(), 1950.0);
        assert!(p.points.iter().all(|p| !p.pause_before));
    }
}
