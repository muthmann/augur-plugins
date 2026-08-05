//! Compacts the host's sensor-telemetry CSV into the per-measurement readout
//! file that travels with a recording.
//!
//! The host polls the camera's monitoring block while recording and writes
//! `<raw-stem>.sensor-monitoring.csv` next to the RAW. Two things are wrong
//! with keeping that file as it is:
//!
//! 1. **It stays behind.** A1 gathers the camera RAW, its bias sidecar, the
//!    photodiode PDQ and the description file into one measurement folder
//!    under one name; the telemetry did not travel with them, so the bench
//!    conditions of a run were separated from the run at the first `mv`.
//!
//! 2. **It is a wide table of mostly-empty cells.** The channels are polled on
//!    different schedules — the die temperature drifts over minutes, the pixel
//!    dead time is read far more often — so a row-per-poll layout with a column
//!    per channel is padding by construction. The bias columns are pure
//!    duplication on top of that: the same codes are already in the camera's
//!    own bias sidecar, which travels with the RAW.
//!
//! So this rewrites it column-wise: one timestamp/value pair list per channel,
//! carrying only the samples where that channel was actually read. Nothing is
//! resampled, interpolated or aligned — a reading exists at the instant it was
//! taken or not at all.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Schema tag written into every readout file.
pub const SCHEMA: &str = "stage-a.a1.sensor.v1";

/// One channel's samples, in acquisition order.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Channel {
    /// Microseconds since the recording's host clock anchor — the midpoint of
    /// the poll, because a monitoring read is not instantaneous and the
    /// midpoint is the least wrong single instant to attribute it to.
    pub t_us: Vec<i64>,
    pub value: Vec<f64>,
}

impl Channel {
    fn push(&mut self, t_us: i64, value: f64) {
        self.t_us.push(t_us);
        self.value.push(value);
    }

    pub fn len(&self) -> usize {
        self.t_us.len()
    }

    pub fn is_empty(&self) -> bool {
        self.t_us.is_empty()
    }
}

/// A poll that returned nothing usable, kept so a gap in a channel is
/// distinguishable from a channel that was never polled.
#[derive(Debug, Clone, PartialEq)]
pub struct PollFault {
    pub t_us: i64,
    pub status: String,
    pub message: String,
}

/// The compacted readout for one recording.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SensorReadout {
    /// Channel name → samples. Empty channels are dropped entirely.
    pub channels: BTreeMap<String, Channel>,
    pub faults: Vec<PollFault>,
    /// Polls read out of the source file, including the ones that failed.
    pub polls: usize,
}

impl SensorReadout {
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty() && self.faults.is_empty()
    }

    /// Renders the readout as JSON.
    ///
    /// Hand-written rather than via `serde_json` so the arrays stay on one line
    /// each: these files are read by eye as often as by script, and a pretty
    /// printer puts one number per line — thousands of lines for what is
    /// conceptually one row.
    pub fn to_json(&self, measurement_id: &str, recording_stem: &str) -> String {
        let mut out = String::with_capacity(1_024 + self.polls * 24);
        out.push_str("{\n");
        let _ = writeln!(out, "  \"schema\": \"{SCHEMA}\",");
        let _ = writeln!(
            out,
            "  \"measurement_id\": {},",
            json_string(measurement_id)
        );
        let _ = writeln!(out, "  \"recording\": {},", json_string(recording_stem));
        out.push_str(
            "  \"time_base\": \"t_us is the midpoint of each poll, in microseconds on the \
             host clock anchored at the start of this recording\",\n",
        );
        out.push_str(
            "  \"note\": \"Channels are sampled independently and are not aligned; bias codes \
             are omitted because the camera's own bias sidecar already carries them.\",\n",
        );
        let _ = writeln!(out, "  \"polls\": {},", self.polls);
        out.push_str("  \"channels\": {\n");
        let mut first = true;
        for (name, channel) in &self.channels {
            if !first {
                out.push_str(",\n");
            }
            first = false;
            let _ = write!(
                out,
                "    {}: {{ \"t_us\": [{}], \"value\": [{}] }}",
                json_string(name),
                join_i64(&channel.t_us),
                join_f64(&channel.value),
            );
        }
        out.push_str("\n  },\n");
        out.push_str("  \"faults\": [");
        for (index, fault) in self.faults.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "\n    {{ \"t_us\": {}, \"status\": {}, \"message\": {} }}",
                fault.t_us,
                json_string(&fault.status),
                json_string(&fault.message),
            );
        }
        if self.faults.is_empty() {
            out.push_str("]\n");
        } else {
            out.push_str("\n  ]\n");
        }
        out.push_str("}\n");
        out
    }
}

/// Parses the host's `<stem>.sensor-monitoring.csv` into a compact readout.
///
/// Unknown or reordered columns are handled by name, so a host that adds a
/// column does not shift every value by one. Rows that cannot be read are
/// skipped rather than failing the whole file: a truncated last line is normal
/// if the recording was cut short, and losing the other 4 000 samples over it
/// would be the wrong trade.
pub fn parse_csv(text: &str) -> SensorReadout {
    let mut lines = text.lines();
    let Some(header) = lines.next() else {
        return SensorReadout::default();
    };
    let columns: Vec<&str> = header.split(',').map(str::trim).collect();
    let index_of = |name: &str| columns.iter().position(|column| *column == name);

    let start = index_of("host_elapsed_start_us");
    let end = index_of("host_elapsed_end_us");
    let status = index_of("status");
    let error = index_of("error");
    // Bias columns are deliberately absent from this list.
    let measured: Vec<(&str, usize)> = ["illumination_lux", "temperature_c", "pixel_dead_time_us"]
        .into_iter()
        .filter_map(|name| index_of(name).map(|index| (name, index)))
        .collect();

    let mut readout = SensorReadout::default();
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields = crate::csv::split_line(line);
        let at = |index: Option<usize>| {
            index
                .and_then(|index| fields.get(index))
                .map(String::as_str)
        };
        let midpoint = match (
            at(start).and_then(|value| value.parse::<i64>().ok()),
            at(end).and_then(|value| value.parse::<i64>().ok()),
        ) {
            (Some(start), Some(end)) => start + (end - start) / 2,
            (Some(start), None) => start,
            _ => continue,
        };
        readout.polls += 1;

        let mut any = false;
        for (name, index) in &measured {
            let Some(raw) = fields.get(*index) else {
                continue;
            };
            if raw.is_empty() {
                continue;
            }
            let Ok(value) = raw.parse::<f64>() else {
                continue;
            };
            if !value.is_finite() {
                continue;
            }
            readout
                .channels
                .entry((*name).to_owned())
                .or_default()
                .push(midpoint, value);
            any = true;
        }
        // A poll that produced no reading is only worth recording when the host
        // said why; an ordinary "nothing due yet" row is not a fault.
        let status_text = at(status).unwrap_or("").to_owned();
        let error_text = at(error).unwrap_or("").to_owned();
        if !any && (!error_text.is_empty() || (!status_text.is_empty() && status_text != "ok")) {
            readout.faults.push(PollFault {
                t_us: midpoint,
                status: status_text,
                message: error_text,
            });
        }
    }
    readout
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", other as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn join_i64(values: &[i64]) -> String {
    let mut out = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{value}");
    }
    out
}

fn join_f64(values: &[f64]) -> String {
    let mut out = String::new();
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        // Shortest round-trip form: these are f32 readings widened to f64, so
        // the default Display is both exact and compact.
        let _ = write!(out, "{value}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "schema_version,sample_id,poll_kind,host_elapsed_start_us,\
host_elapsed_end_us,raw_data_offset_before_bytes,raw_data_offset_after_bytes,illumination_lux,\
temperature_c,pixel_dead_time_us,bias_diff_on_code,bias_diff_off_code,bias_fo_code,bias_hpf_code,\
bias_refr_code,status,error";

    fn csv(rows: &[&str]) -> String {
        let mut text = String::from(HEADER);
        for row in rows {
            text.push('\n');
            text.push_str(row);
        }
        text.push('\n');
        text
    }

    #[test]
    fn channels_keep_only_the_polls_that_actually_read_them() {
        // The whole point: the die temperature is polled far less often than
        // the dead time, and a row-per-poll table pads the difference with
        // empty cells. Each channel carries its own samples and nothing else.
        let text = csv(&[
            "1,1,fast,1000,1200,0,0,,,12.5,,,,,,ok,",
            "1,2,fast,2000,2200,0,0,,,12.6,,,,,,ok,",
            "1,3,full,3000,3400,0,0,140.0,41.5,12.7,10,20,30,40,50,ok,",
            "1,4,fast,4000,4200,0,0,,,12.8,,,,,,ok,",
        ]);
        let readout = parse_csv(&text);

        assert_eq!(readout.polls, 4);
        assert_eq!(readout.channels["pixel_dead_time_us"].len(), 4);
        assert_eq!(readout.channels["temperature_c"].len(), 1);
        assert_eq!(readout.channels["illumination_lux"].len(), 1);
        assert_eq!(readout.channels["temperature_c"].value, vec![41.5]);
    }

    #[test]
    fn bias_codes_are_dropped_because_the_bias_sidecar_already_has_them() {
        let text = csv(&["1,1,full,1000,1200,0,0,140.0,41.5,12.7,10,20,30,40,50,ok,"]);
        let readout = parse_csv(&text);
        assert_eq!(readout.channels.len(), 3);
        for name in readout.channels.keys() {
            assert!(!name.starts_with("bias"), "bias channel survived: {name}");
        }
        let parsed: serde_json::Value =
            serde_json::from_str(&readout.to_json("m", "s")).expect("valid JSON");
        let channels = parsed["channels"].as_object().expect("channels object");
        assert!(
            channels.keys().all(|name| !name.starts_with("bias")),
            "{channels:?}"
        );
    }

    #[test]
    fn a_sample_is_timestamped_at_the_midpoint_of_its_poll() {
        // A monitoring read takes a few hundred microseconds; attributing it to
        // its start would systematically date every reading early.
        let text = csv(&["1,1,full,1000,1400,0,0,140.0,41.5,12.7,,,,,,ok,"]);
        let readout = parse_csv(&text);
        assert_eq!(readout.channels["temperature_c"].t_us, vec![1200]);
    }

    #[test]
    fn a_failed_poll_is_kept_as_a_fault_so_a_gap_is_explainable() {
        let text = csv(&[
            "1,1,full,1000,1200,0,0,140.0,41.5,12.7,,,,,,ok,",
            "1,2,full,2000,2200,0,0,,,,,,,,,error,\"i2c timeout, retrying\"",
        ]);
        let readout = parse_csv(&text);
        assert_eq!(readout.polls, 2);
        assert_eq!(readout.faults.len(), 1);
        assert_eq!(readout.faults[0].t_us, 2100);
        assert_eq!(readout.faults[0].status, "error");
        assert_eq!(readout.faults[0].message, "i2c timeout, retrying");
    }

    #[test]
    fn an_ordinary_empty_poll_is_not_a_fault() {
        let text = csv(&["1,1,fast,1000,1200,0,0,,,,,,,,,ok,"]);
        let readout = parse_csv(&text);
        assert_eq!(readout.polls, 1);
        assert!(readout.faults.is_empty());
        assert!(readout.channels.is_empty());
    }

    #[test]
    fn a_truncated_final_row_does_not_cost_the_rest_of_the_file() {
        // Cutting a recording short leaves a partial last line. Losing 4 000
        // good samples over it would be the wrong trade.
        let mut text = csv(&["1,1,full,1000,1200,0,0,140.0,41.5,12.7,,,,,,ok,"]);
        text.push_str("1,2,full,20");
        let readout = parse_csv(&text);
        assert_eq!(readout.channels["temperature_c"].len(), 1);
    }

    #[test]
    fn columns_are_found_by_name_not_by_position() {
        // A host that inserts a column must not shift every reading by one.
        let text = "host_elapsed_start_us,host_elapsed_end_us,new_column,temperature_c,status\n\
                    1000,1200,x,41.5,ok\n";
        let readout = parse_csv(text);
        assert_eq!(readout.channels["temperature_c"].value, vec![41.5]);
    }

    #[test]
    fn an_empty_or_header_only_file_produces_an_empty_readout() {
        assert!(parse_csv("").is_empty());
        assert!(parse_csv(HEADER).is_empty());
    }

    #[test]
    fn the_json_is_one_line_per_channel_and_parses_back() {
        let text = csv(&[
            "1,1,full,1000,1200,0,0,140.0,41.5,12.7,,,,,,ok,",
            "1,2,fast,2000,2200,0,0,,,12.8,,,,,,ok,",
        ]);
        let json = parse_csv(&text).to_json("meas-1", "meas-1_20260731T120000Z");

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed["schema"], SCHEMA);
        assert_eq!(parsed["measurement_id"], "meas-1");
        assert_eq!(parsed["polls"], 2);
        assert_eq!(parsed["channels"]["pixel_dead_time_us"]["value"][1], 12.8);
        assert_eq!(parsed["channels"]["temperature_c"]["t_us"][0], 1100);

        // Compactness is the point of hand-rendering it: a pretty printer would
        // put one number per line.
        for line in json.lines() {
            assert!(
                !line.trim_start().starts_with("12.8"),
                "an array was expanded one value per line:\n{json}"
            );
        }
    }

    #[test]
    fn quoted_error_text_with_commas_survives_the_round_trip() {
        let text = csv(&["1,1,full,1000,1200,0,0,,,,,,,,,error,\"a, b, \"\"c\"\"\""]);
        let readout = parse_csv(&text);
        assert_eq!(readout.faults[0].message, "a, b, \"c\"");
        let parsed: serde_json::Value =
            serde_json::from_str(&readout.to_json("m", "s")).expect("valid JSON");
        assert_eq!(parsed["faults"][0]["message"], "a, b, \"c\"");
    }
}
