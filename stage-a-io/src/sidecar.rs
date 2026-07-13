//! Run sidecar (manifest): everything needed to reproduce or audit one
//! Stage-A recording, written as JSON next to the camera RAW / PDQ files.
//!
//! Per the control-software spec, each recording sidecar includes the run
//! ID, plugin/firmware/protocol versions, raw PDQ path and checksum,
//! ADC/front-end calibration, load, configured and measured sample cadence,
//! drop/CRC counters, the ACKed configuration revision, bias set, optical
//! configuration, flux point, measured `a`, and trigger source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::client::StreamIntegrity;
use crate::estimator::{AdcCalibration, ContrastEstimate};
use crate::pdq::PdqSummary;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerSource {
    /// Teensy waveform phase-0 sync TTL (A1/A3 drive fiducial).
    DrivePhase0,
    /// Photodiode → comparator 50 % crossing (A2 light fiducial).
    Comparator,
    /// No hardware trigger wired; software phase recovery in use.
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorLoad {
    /// 50 Ω — A1/A2 (speed over signal).
    FiftyOhm,
    /// Characterised high-Z load — A3 only ($f \ll f_c$).
    HighZ { nominal_ohms: u64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSidecar {
    pub run_id: String,
    pub protocol: String,
    pub created_utc: String,

    pub plugin_name: String,
    pub plugin_version: String,
    pub firmware_version: String,
    pub wire_protocol_version: u8,

    /// Path + CRC32 of the raw PDQ photodiode file.
    pub pdq_path: PathBuf,
    pub pdq_crc32: u32,
    pub pdq_frames: u64,
    /// Path of the camera RAW recording this run belongs to, if any.
    pub camera_raw_path: Option<PathBuf>,

    pub adc_calibration: AdcCalibration,
    pub detector_load: DetectorLoad,
    pub configured_sample_rate_hz: u32,
    pub measured_sample_rate_hz: Option<f64>,

    pub integrity: IntegrityRecord,
    /// Overall validity — false on any drop/CRC/sequence/cadence fault or
    /// estimator rejection. An invalid point is re-measured, never patched.
    pub valid: bool,

    /// ACKed controller configuration (verbatim key=value fields) and its
    /// revision, exactly as the firmware confirmed them.
    pub acked_config_revision: Option<u32>,
    pub acked_config: BTreeMap<String, String>,

    /// Frozen camera bias set identifier (registry lives in the knowledge
    /// base `setup/bias-sets.md`).
    pub bias_set: Option<String>,
    /// Optical configuration / flux point labels from the run plan.
    pub optical_configuration: Option<String>,
    pub flux_point: Option<String>,

    /// Measured optical log-contrast for this run/point, when applicable.
    pub measured_contrast: Option<ContrastEstimate>,
    pub trigger_source: TriggerSource,

    /// Free-form notes (operator observations, deviations).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct IntegrityRecord {
    pub skipped_bytes: u64,
    pub crc_failures: u64,
    pub sequence_gaps: u64,
    pub dropped_samples: u64,
}

impl From<StreamIntegrity> for IntegrityRecord {
    fn from(value: StreamIntegrity) -> Self {
        Self {
            skipped_bytes: value.skipped_bytes,
            crc_failures: value.crc_failures,
            sequence_gaps: value.sequence_gaps,
            dropped_samples: value.dropped_samples,
        }
    }
}

impl RunSidecar {
    /// Builds a sidecar skeleton from a finished PDQ file. Protocol fields
    /// and run metadata are filled by the owning plugin before writing.
    pub fn from_pdq(run_id: &str, protocol: &str, pdq: &PdqSummary) -> Self {
        Self {
            run_id: run_id.to_owned(),
            protocol: protocol.to_owned(),
            created_utc: now_utc_iso8601(),
            plugin_name: String::new(),
            plugin_version: String::new(),
            firmware_version: String::new(),
            wire_protocol_version: crate::wire::PROTOCOL_VERSION,
            pdq_path: pdq.path.clone(),
            pdq_crc32: pdq.file_crc32,
            pdq_frames: pdq.frames_written,
            camera_raw_path: None,
            adc_calibration: AdcCalibration::default(),
            detector_load: DetectorLoad::FiftyOhm,
            configured_sample_rate_hz: 0,
            measured_sample_rate_hz: None,
            integrity: pdq.integrity.into(),
            valid: pdq.valid,
            acked_config_revision: None,
            acked_config: BTreeMap::new(),
            bias_set: None,
            optical_configuration: None,
            flux_point: None,
            measured_contrast: None,
            trigger_source: TriggerSource::None,
            notes: Vec::new(),
        }
    }

    pub fn write_json(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        std::fs::write(path, json)
    }

    pub fn read_json(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(std::io::Error::other)
    }
}

fn now_utc_iso8601() -> String {
    // Seconds-resolution UTC timestamp without pulling in chrono.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days` (public domain algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trips_through_json() {
        let pdq = PdqSummary {
            path: PathBuf::from("/data/A1-20260713-01.pdq"),
            frames_written: 128,
            bytes_written: 65_536,
            file_crc32: 0xDEAD_BEEF,
            integrity: StreamIntegrity::default(),
            valid: true,
        };
        let mut sidecar = RunSidecar::from_pdq("A1-20260713-01", "A1", &pdq);
        sidecar.plugin_name = "stage-a-a1".into();
        sidecar.acked_config_revision = Some(4);
        sidecar.acked_config.insert("mode".into(), "A1".into());
        sidecar.trigger_source = TriggerSource::DrivePhase0;

        let json = serde_json::to_string(&sidecar).expect("serializes");
        let decoded: RunSidecar = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(decoded, sidecar);
        assert!(decoded.created_utc.ends_with('Z'));
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_282), (2025, 7, 13));
    }
}
