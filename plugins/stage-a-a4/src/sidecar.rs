//! The per-recording A4 sidecar, and the run-level protocol receipt.
//!
//! A threshold point is only worth keeping if it can answer, months later,
//! *which absolute bias codes were on the die while this file was written* —
//! and under what optical and thermal conditions. That is what this file is
//! for. It is written for every point, including the ones that failed, because
//! the record of a failed point is the reason the survey has a hole in it.
//!
//! Fields the sensor could not report are **absent**, never `0`. A die
//! temperature of 0 °C and "this camera has no temperature readback" are
//! opposite facts, and a reader six months from now cannot tell them apart from
//! a zero.

use serde::Serialize;

pub const SIDECAR_SCHEMA: &str = "stage-a.a4.sidecar.v1";
pub const RECEIPT_SCHEMA: &str = "stage-a.a4.protocol-status.v1";

#[derive(Debug, Serialize)]
pub struct SidecarDoc {
    pub schema: &'static str,
    pub measurement_id: String,
    pub recording: String,
    pub recorded_at_utc: String,
    pub plugin_version: &'static str,
    pub protocol: ProtocolSection,
    pub bias: BiasSection,
    pub optics: OpticsSection,
    pub sensor: SensorSection,
    pub filters: FiltersSection,
    pub camera: CameraSection,
    pub files: FilesSection,
    pub qc: QcSection,
}

/// The protocol row this recording came from, copied verbatim, plus where in
/// the file it sat and which file that was.
#[derive(Debug, Serialize)]
pub struct ProtocolSection {
    pub name: String,
    pub file: String,
    pub sha256: String,
    /// 1-based, so it matches what the operator counts in the file.
    pub row: usize,
    pub rows_total: usize,
    pub label: String,
    pub repeat: u32,
    pub repeats: u32,
    pub requested_duration_s: i64,
    pub requested_settle_s: f64,
}

/// What was asked for, what was programmed, and what the sensor said it was
/// running. The three are kept separate on purpose: they are the same number
/// only when nothing went wrong, and this file exists to prove that.
#[derive(Debug, Serialize)]
pub struct BiasSection {
    /// Offsets the protocol row asked for.
    pub requested_diff_on: i64,
    pub requested_diff_off: i64,
    /// Offsets the host programmed, after its own range clamp.
    pub applied_diff_on: i32,
    pub applied_diff_off: i32,
    /// Absolute 8-bit codes read back off the die.
    pub code_diff_on: u8,
    pub code_diff_off: u8,
    /// The per-unit factory trim the offsets are relative to.
    pub factory_diff_on: u8,
    pub factory_diff_off: u8,
    /// Codes for the biases A4 never touches, recorded so a reader can confirm
    /// they were the same across the survey.
    pub code_fo: u8,
    pub code_hpf: u8,
    pub code_refr: u8,
    /// Seconds between the reconfigure and the reading that confirmed it.
    pub readback_age_s: f64,
    pub confirmed: bool,
}

/// The optical condition, which A4 never changes and only records.
#[derive(Debug, Serialize)]
pub struct OpticsSection {
    pub optical_state: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub filter_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub flux_id: String,
    /// Whether the operator was asked to intervene before this point.
    pub paused_for_operator: bool,
}

/// Bench conditions at the two ends of the recording. Every field is optional;
/// a sensor that cannot report a quantity leaves it out.
#[derive(Debug, Serialize)]
pub struct SensorSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c_start: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_c_end: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub illumination_lux_start: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub illumination_lux_end: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pixel_dead_time_us: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reading_age_s: Option<f64>,
    /// Sensor lux is a stability indicator, not a calibrated optical power.
    /// Stated in the file so nobody later reads it as one.
    pub illumination_note: &'static str,
}

/// The on-sensor filters, which must all be off for the counts to mean
/// anything. Recorded rather than assumed.
#[derive(Debug, Serialize)]
pub struct FiltersSection {
    pub stc_enabled: bool,
    pub trail_enabled: bool,
    pub erc_enabled: bool,
    pub erc_note: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CameraSection {
    pub roi_x: u16,
    pub roi_y: u16,
    pub roi_width: u16,
    pub roi_height: u16,
    pub masked_pixels: usize,
    pub sensor_width: u16,
    pub sensor_height: u16,
}

#[derive(Debug, Serialize)]
pub struct FilesSection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recorded_duration_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sensor_readout: Option<String>,
    /// True only for a clean `RecordingFinalized` with a plausible size, hash
    /// and duration. A partial receipt is never complete, whatever survived.
    pub complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QcSection {
    pub status: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    pub on_events: u64,
    pub off_events: u64,
    pub total_events: u64,
    /// Seconds of the recording the plugin actually observed events over. Less
    /// than the recording duration when frames were dropped, so a reader can
    /// see the coverage the rates were computed from rather than assuming it.
    pub counted_seconds: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_drift_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub illumination_drift_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_temperature_drift_c: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_illumination_drift_percent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_event_rate: Option<f64>,
    pub rate_note: &'static str,
}

// ---- run-level receipt -----------------------------------------------------

/// Written next to the copy of the protocol, so the folder says which rows ran
/// and which did not without anyone having to diff filenames against the file.
#[derive(Debug, Serialize)]
pub struct ProtocolReceipt {
    pub schema: &'static str,
    pub measurement_id: String,
    pub protocol_name: String,
    pub protocol_file: String,
    pub protocol_sha256: String,
    pub started_at_utc: String,
    pub finished_at_utc: String,
    pub outcome: String,
    pub rows_total: usize,
    pub rows_recorded: usize,
    pub rows_failed: usize,
    pub rows_flagged: usize,
    /// The bias offsets the bench was on before the survey, restored afterwards.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_diff_on: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored_diff_off: Option<i32>,
    pub biases_restored: bool,
    pub row: Vec<ReceiptRow>,
}

#[derive(Debug, Serialize)]
pub struct ReceiptRow {
    pub row: usize,
    pub label: String,
    pub diff_on: i64,
    pub diff_off: i64,
    pub repeat: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    pub qc: String,
}
