//! Versioned, serde-only messages shared by Stage-A experiment workflows and
//! the two persistent Teensy device-owner plugins, plus the small pure helpers
//! more than one Stage-A workflow needs ([`telemetry`], [`csv`]).
//!
//! This crate intentionally contains no Augur ABI types, serial transports,
//! filesystem access, raw ADC arrays, or experiment state machines. Every
//! experiment plugin exports `augur_plugin_vtable`, so shared code cannot live
//! in one of them and be linked by another — it lives here, in a plain library
//! that exports no vtable at all (ADR 031).

#![forbid(unsafe_code)]

pub mod csv;
pub mod telemetry;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

pub const CONTRACT_VERSION_V1: u16 = 1;

/// Firmware-qualified periodic-drive range. These values mirror
/// `stage-a-controller/include/board_config.h`; all Rust-side UI, service and
/// protocol validation uses this one definition rather than duplicating the
/// literals. The connected firmware remains authoritative and rejects outside
/// this range as well.
pub const DRIVE_FREQUENCY_MIN_MILLIHZ: u64 = 10;
pub const DRIVE_FREQUENCY_MAX_MILLIHZ: u64 = 2_000_000;
pub const DRIVE_DAC_UPDATE_RATE_HZ: u32 = 40_000;
/// Minimum sample density for an A1 photodiode waveform measurement. Nyquist
/// alone only proves non-aliasing; 16 samples/cycle is the project's minimum
/// shape-resolution acceptance threshold.
pub const A1_MIN_SAMPLES_PER_CYCLE: u32 = 16;

pub fn drive_frequency_supported(frequency_millihz: u64) -> bool {
    (DRIVE_FREQUENCY_MIN_MILLIHZ..=DRIVE_FREQUENCY_MAX_MILLIHZ).contains(&frequency_millihz)
}

pub fn a1_measurement_frequency_limit_hz(sample_rate_hz: u32) -> f64 {
    f64::from(sample_rate_hz) / f64::from(A1_MIN_SAMPLES_PER_CYCLE)
}

pub const PLUGIN_ID_STAGE_A_MODULATION: &str = "stage-a.modulation";
pub const PLUGIN_ID_STAGE_A_PHOTODIODE: &str = "stage-a.photodiode";
pub const SERVICE_STAGE_A_MODULATION_CONTROL_V1: &str = "stage_a.modulation.control.v1";
pub const SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1: &str = "stage_a.photodiode.control.v1";

pub const CTX_STAGE_A_MODULATION_REQUEST_V1: &str = "stage_a.modulation_request.v1";
pub const CTX_STAGE_A_MODULATION_RESPONSE_V1: &str = "stage_a.modulation_response.v1";
pub const CTX_STAGE_A_MODULATION_STATE_V1: &str = "stage_a.modulation_state.v1";
pub const CTX_STAGE_A_PHOTODIODE_REQUEST_V1: &str = "stage_a.photodiode_request.v1";
pub const CTX_STAGE_A_PHOTODIODE_RESPONSE_V1: &str = "stage_a.photodiode_response.v1";
pub const CTX_STAGE_A_PHOTODIODE_SUMMARY_V1: &str = "stage_a.photodiode_summary.v1";

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(ClientId);
string_id!(LeaseId);
string_id!(OwnerInstanceId);
string_id!(RunId);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct RequestId(pub u64);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct SemanticRevision(pub u64);

/// Wall-clock freshness information transferable between dynamic plugins.
/// The consumer determines staleness against its current Unix time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessV1 {
    pub observed_at_unix_ms: u64,
    pub valid_for_ms: u64,
}

impl FreshnessV1 {
    pub fn is_stale_at(self, now_unix_ms: u64) -> bool {
        now_unix_ms.saturating_sub(self.observed_at_unix_ms) > self.valid_for_ms
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionStateV1 {
    Disconnected,
    Connecting,
    Connected {
        port_label: String,
        firmware_version: Option<String>,
    },
    Faulted {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseSnapshotV1 {
    pub lease_id: LeaseId,
    pub holder: ClientId,
    pub expires_at_unix_ms: u64,
    pub run_id: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsyncedReasonV1 {
    NoOwnerSnapshot,
    OwnerRestarted,
    StaleSnapshot,
    NoLease,
    LeaseMismatch,
    RunMismatch,
    RequestedRevisionNotAcknowledged,
    FirmwareRevisionUnavailable,
    StreamEpochChanged,
    DeviceFault,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SynchronizationV1 {
    Synced {
        run_id: RunId,
        acknowledged_revision: SemanticRevision,
        stream_epoch: Option<u64>,
    },
    Unsynced {
        reason: UnsyncedReasonV1,
        detail: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceErrorCodeV1 {
    ContractVersion,
    WrongOwnerInstance,
    StaleRequest,
    DuplicateRequestConflict,
    NotConnected,
    LeaseRequired,
    LeaseBusy,
    LeaseMismatch,
    LeaseExpired,
    UnsafeExecutionContext,
    InvalidCommand,
    InvalidPath,
    DeviceRejected,
    Transport,
    Io,
    Integrity,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceErrorV1 {
    pub code: ServiceErrorCodeV1,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestOutcomeV1 {
    InProgress,
    Applied,
    Rejected,
}

/// Common request envelope. The command-specific aliases below are the
/// public mailbox payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelopeV1<C> {
    pub contract_version: u16,
    pub request_id: RequestId,
    pub requester: ClientId,
    /// `None` is allowed only for discovery/connect or first lease acquire.
    pub target_owner_instance: Option<OwnerInstanceId>,
    pub lease_id: Option<LeaseId>,
    pub run_id: Option<RunId>,
    pub requested_revision: Option<SemanticRevision>,
    pub issued_at_unix_ms: u64,
    pub command: C,
}

impl<C> RequestEnvelopeV1<C> {
    pub fn new(request_id: RequestId, requester: ClientId, command: C) -> Self {
        Self {
            contract_version: CONTRACT_VERSION_V1,
            request_id,
            requester,
            target_owner_instance: None,
            lease_id: None,
            run_id: None,
            requested_revision: None,
            issued_at_unix_ms: 0,
            command,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseCommonV1 {
    pub contract_version: u16,
    pub request_id: RequestId,
    pub owner_instance: OwnerInstanceId,
    pub run_id: Option<RunId>,
    pub requested_revision: Option<SemanticRevision>,
    pub acknowledged_revision: Option<SemanticRevision>,
    pub outcome: RequestOutcomeV1,
    pub completed_at_unix_ms: Option<u64>,
    pub error: Option<ServiceErrorV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PeriodicWaveformV1 {
    Sine,
    Square,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaveformV1 {
    Off,
    Constant {
        level_dac: u16,
    },
    Periodic {
        waveform: PeriodicWaveformV1,
        min_dac: u16,
        max_dac: u16,
        frequency_millihz: u64,
    },
}

/// Complete semantic configuration for one A1 controller acquisition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct A1AcquisitionConfigV1 {
    pub waveform: PeriodicWaveformV1,
    pub frequency_millihz: u64,
    pub center_dac: u16,
    pub amplitude_dac: u16,
    pub sample_rate_hz: u32,
    pub block_samples: u32,
    pub emit_raw_samples: bool,
    pub emit_summary: bool,
    pub optical_lut_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModulationCommandV1 {
    Connect,
    Disconnect {
        safe_off: bool,
        reason: String,
    },
    AcquireLease {
        ttl_ms: u64,
    },
    RenewLease {
        ttl_ms: u64,
    },
    ReleaseLease {
        safe_off: bool,
        reason: String,
    },
    SetWaveform {
        waveform: WaveformV1,
    },
    /// Retarget the owner's *calibrated optical drive* to a new modulation
    /// depth `a` (log contrast, in milli-units) without changing anything else
    /// about the armed drive: waveform shape, frequency, requested normalized
    /// cycle mean and calibration stay whatever the operator armed in the
    /// modulation plugin.
    /// This is the scoped amplitude-sweep path (A1 automation): the owner
    /// rejects the command when its current drive cannot express `a`
    /// (anything other than calibrated `OPTICAL_LOG_SINE` with an identified
    /// transfer calibration) or the device link is closed.
    SetOpticalDepth {
        depth_a_milli: u32,
    },
    /// Retarget the armed drive's *frequency*, leaving everything else — the
    /// waveform shape, the depth, the operating point and the calibration — as
    /// the operator armed it. The frequency counterpart of
    /// [`ModulationCommandV1::SetOpticalDepth`], and the same scoping rules
    /// apply: leased only, rejected when the link is closed or the armed drive
    /// has no frequency to retarget (manual DAC method, constant mode).
    ///
    /// A1's frequency sweep drives this. The owner parks the operator's armed
    /// frequency on the first one and restores it when the lease ends, so a
    /// finished sweep does not leave the bench on its last point.
    SetDriveFrequency {
        frequency_millihz: u64,
    },
    /// Retarget the armed drive's *operating point* — the normalized cycle-mean
    /// lobe coordinate `ū`, in milli-units — leaving the waveform, depth,
    /// frequency and calibration alone. The third axis alongside
    /// [`ModulationCommandV1::SetOpticalDepth`] and
    /// [`ModulationCommandV1::SetDriveFrequency`], and scoped the same way:
    /// leased only, rejected when the link is closed or the armed drive has no
    /// operating point to retarget (manual DAC method).
    ///
    /// This is what makes an `I_k` sweep possible. `ū` is a *normalized* lobe
    /// coordinate, not physical flux — but it is the one knob that moves the
    /// mean illumination without touching the depth, so a protocol that walks
    /// it walks the bench's brightness axis.
    ///
    /// The owner parks the operator's armed `ū` on the first point and restores
    /// it when the lease ends, so a finished sweep does not leave the bench on
    /// its last one.
    SetOperatingPoint {
        mean_u_milli: u32,
    },
    PrepareA1 {
        configuration: A1AcquisitionConfigV1,
    },
    StartAcquisition,
    StopAcquisition {
        reason: String,
    },
    SafeOff {
        reason: String,
    },
}

pub type ModulationRequestV1 = RequestEnvelopeV1<ModulationCommandV1>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerStateV1 {
    Unknown,
    SafeIdle,
    Configured,
    Running,
    Faulted,
}

/// The full desired or board-acknowledged command-port state at one semantic
/// revision. Owners never infer an ACK from the requested state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModulationTargetV1 {
    pub revision: SemanticRevision,
    pub waveform: Option<WaveformV1>,
    pub a1_configuration: Option<A1AcquisitionConfigV1>,
    pub acquisition_running: bool,
    pub board_dac_code: Option<u16>,
    pub firmware_configuration_revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModulationResponseV1 {
    #[serde(flatten)]
    pub common: ResponseCommonV1,
    pub controller_state: ControllerStateV1,
    pub acknowledged_target: Option<ModulationTargetV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpticalTargetV1 {
    LogSine,
    LinearSine,
}

/// Exact optical-inversion parameters currently resolved by the modulation
/// owner. Additive in V1 so A1 sidecars can reproduce the requested drive
/// without misusing physical flux `I_k` for the normalized lobe coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpticalDriveStateV1 {
    pub target: OpticalTargetV1,
    /// Requested normalized cycle-mean lobe coordinate `ū`.
    pub requested_mean_u_milli: u32,
    /// Mean reconstructed from the quantized internal wire coordinate.
    pub resolved_mean_u_milli: u32,
    /// Internal target pedestal/centre sent in the wire's legacy `u_k_milli`
    /// field (`u_g` for log-sine, `u_c` for linear-sine).
    pub internal_u_milli: u32,
    pub depth_a_milli: u32,
    /// DAC code at the excitation minimum of the lobe in use.
    pub v_null_dac: u16,
    /// DAC code at the excitation maximum of the same lobe.
    ///
    /// An absolute code, like `v_null_dac` — not the half-wave *span* between
    /// them, which the earlier `v_pi_dac` field carried. One lobe is named by
    /// two codes an operator can point at on the transfer curve, and mixing an
    /// absolute code with a distance is exactly the confusion this pair exists
    /// to prevent (ADR 016). The span is `v_peak_dac − v_null_dac`.
    pub v_peak_dac: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModulationStateV1 {
    pub contract_version: u16,
    pub owner_instance: OwnerInstanceId,
    pub service_revision: u64,
    pub connection: ConnectionStateV1,
    pub capabilities: Vec<String>,
    pub lease: Option<LeaseSnapshotV1>,
    pub controller_state: ControllerStateV1,
    pub active_run_id: Option<RunId>,
    pub requested: Option<ModulationTargetV1>,
    pub acknowledged: Option<ModulationTargetV1>,
    pub synchronization: SynchronizationV1,
    pub last_response: Option<ModulationResponseV1>,
    pub freshness: FreshnessV1,
    /// Identifier of the measured Pockels transfer calibration currently
    /// applied to `V_null`/`Vπ`, so a consumer's sidecar can cite which
    /// inversion produced a run's optical depth. `None` means the operator
    /// entered the lobe parameters by hand. Additive in V1.
    #[serde(default)]
    pub calibration_id: Option<String>,
    /// Additive V1 optical-drive provenance.
    #[serde(default)]
    pub optical_drive: Option<OpticalDriveStateV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StreamIntegrityV1 {
    pub skipped_bytes: u64,
    pub crc_failures: u64,
    pub sequence_gaps: u64,
    pub dropped_samples: u64,
    pub segment_restarts: u64,
    pub truncated_bytes: u64,
}

impl StreamIntegrityV1 {
    pub fn is_clean(self) -> bool {
        self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleRangeV1 {
    pub first_sample_index: u64,
    pub end_sample_index_exclusive: u64,
    pub sample_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Sha256V1(String);

impl Sha256V1 {
    pub fn parse(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("SHA-256 must be exactly 64 hexadecimal characters".into());
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256V1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256V1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// The exact file to open at a recording boundary. Metadata is deliberately
/// string-valued and bounded by the owner; scientific sidecars remain the
/// canonical rich metadata record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdqStartSpecV1 {
    pub pdq_path: String,
    pub sidecar_path: String,
    pub expected_sample_rate_hz: Option<u32>,
    pub expected_stream_epoch: Option<u64>,
    pub metadata: BTreeMap<String, String>,
    /// Absolute directory the workflow client wants this recording written
    /// below, so a coordinated run can put every file in one measurement
    /// folder instead of the owner's own data directory. `None` keeps the
    /// owner's configured data directory. `pdq_path`/`sidecar_path` stay
    /// relative to whichever root applies, and the owner still refuses
    /// traversal and symlinked path components below it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdqStartedReceiptV1 {
    pub run_id: RunId,
    pub pdq_path: String,
    pub sidecar_path: String,
    pub opened_at_unix_ms: u64,
    pub stream_epoch: u64,
    pub first_sample_index: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PdqTerminationV1 {
    Completed,
    OperatorStopped,
    LeaseExpired,
    SafeOff,
    DeviceFault,
    IoFault,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PdqFinalizedReceiptV1 {
    pub run_id: RunId,
    pub pdq_path: String,
    pub sidecar_path: String,
    pub opened_at_unix_ms: u64,
    pub finalized_at_unix_ms: u64,
    pub file_size_bytes: u64,
    pub sha256: Sha256V1,
    pub frames_written: u64,
    pub sample_frames_written: u64,
    pub sample_range: Option<SampleRangeV1>,
    pub sample_rate_hz: Option<u32>,
    pub segment_count: u64,
    pub integrity: StreamIntegrityV1,
    pub termination: PdqTerminationV1,
    pub valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PdqReceiptV1 {
    Started(PdqStartedReceiptV1),
    Finalized(PdqFinalizedReceiptV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PhotodiodeCommandV1 {
    Connect,
    Disconnect {
        finalize_recording: bool,
        reason: String,
    },
    AcquireLease {
        ttl_ms: u64,
    },
    RenewLease {
        ttl_ms: u64,
    },
    ReleaseLease {
        finalize_recording: bool,
        reason: String,
    },
    BeginRecording {
        specification: PdqStartSpecV1,
    },
    FinalizeRecording {
        termination: PdqTerminationV1,
    },
    AbortRecording {
        reason: String,
    },
}

pub type PhotodiodeRequestV1 = RequestEnvelopeV1<PhotodiodeCommandV1>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotodiodeResponseV1 {
    #[serde(flatten)]
    pub common: ResponseCommonV1,
    pub receipt: Option<PdqReceiptV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotodiodeCalibrationV1 {
    pub adc_calibration_id: String,
    pub dark_id: String,
    pub anchor_id: String,
    pub dark_volts: f64,
    /// Named full-extinction anchor after dark subtraction.
    pub total_power_volts: f64,
}

/// Bounded optical result for one named run. It contains no raw or decimated
/// waveform samples; the finalized PDQ remains the source for replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotodiodeOpticalSummaryV1 {
    pub run_id: RunId,
    pub calibration: PhotodiodeCalibrationV1,
    pub measured_log_contrast: f64,
    pub log_contrast_stddev: Option<f64>,
    pub excitation_min_volts: f64,
    pub excitation_max_volts: f64,
    pub excitation_headroom_volts: f64,
    pub low_clip_fraction: f64,
    pub high_clip_fraction: f64,
    pub measured_frequency_hz: Option<f64>,
    pub fundamental_phase_rad: Option<f64>,
    pub total_harmonic_distortion: Option<f64>,
    /// Duration of the trailing window `measured_log_contrast` was estimated
    /// over. A consumer that *commands* a depth and then reads this value back
    /// has to wait at least this long, or it averages the previous depth in.
    /// Additive in V1: absent from older owners, ignored by older consumers.
    #[serde(default)]
    pub window_seconds: Option<f64>,
    /// Whole modulation cycles that window covered, from the phase-0 markers.
    /// `a` is peak-to-peak, so below one cycle the owner withholds it entirely
    /// rather than publish a phase-dependent under-estimate. `None` when there
    /// is no marker period to measure against.
    #[serde(default)]
    pub covered_cycles: Option<f64>,
}

/// Settled detector level over the owner's **measurement** window, in **raw
/// detector volts**: the ADC affine map only, before dark subtraction and before
/// any [`PhotodiodeOpticalSummaryV1`] geometry transform. Unlike the optical
/// summary this never refuses — it stays present while the window clips (see
/// `clipped`), because a consumer sweeping a static transfer curve needs a
/// level exactly where the detector is brightest.
///
/// The window is fixed by the owner and **independent of any display setting**;
/// `sample_count` reports how long it actually was. Deriving it from the chart's
/// averaging preference instead let a display knob set the precision of the
/// Pockels transfer calibration downstream (ADR 019).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PhotodiodeLevelV1 {
    pub mean_volts: f64,
    /// Spread over the averaged window. A settled `CONST` point has a small
    /// peak-to-peak; a drifting or still-slewing one does not.
    pub peak_to_peak_volts: f64,
    pub sample_count: u64,
    /// Exclusive end of the averaged window on the device sample clock. The
    /// window covers `[end_sample_index - sample_count, end_sample_index)`, so
    /// a consumer can prove a level was measured *after* it commanded a
    /// change without needing a shared wall clock.
    pub end_sample_index: u64,
    /// The window touches an ADC rail; `mean_volts` is a truncated estimate.
    pub clipped: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotodiodeStreamV1 {
    pub stream_epoch: u64,
    pub sample_range: Option<SampleRangeV1>,
    pub sample_rate_hz: Option<u32>,
    pub latest_adc_code: Option<u16>,
    pub integrity: StreamIntegrityV1,
    /// Additive in V1: absent from older owners, and older consumers ignore it.
    #[serde(default)]
    pub level: Option<PhotodiodeLevelV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhotodiodeSummaryV1 {
    pub contract_version: u16,
    pub owner_instance: OwnerInstanceId,
    pub service_revision: u64,
    pub connection: ConnectionStateV1,
    pub lease: Option<LeaseSnapshotV1>,
    pub active_run_id: Option<RunId>,
    pub requested_revision: Option<SemanticRevision>,
    pub acknowledged_revision: Option<SemanticRevision>,
    pub stream: PhotodiodeStreamV1,
    /// Directory the owner resolves relative PDQ/sidecar paths against. `None`
    /// when it is unset, in which case every recording command is rejected —
    /// automation clients check this before they start a coordinated run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_dir: Option<String>,
    pub active_recording: Option<PdqStartedReceiptV1>,
    pub last_finalized_recording: Option<PdqFinalizedReceiptV1>,
    pub optical_summary: Option<PhotodiodeOpticalSummaryV1>,
    /// Why `optical_summary` is absent, in the owner's own words.
    ///
    /// A withheld `a` is a fail-closed refusal, not missing data, and every
    /// automation client that gates on `a` has to be able to tell the operator
    /// which gate rejected the window — otherwise the only readout is "no `a`"
    /// and the fix is a guess. Set exactly when `optical_summary` is `None` and
    /// a window was available to judge.
    ///
    /// Additive in V1: absent from older owners, and older consumers ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optical_unavailable: Option<String>,
    pub synchronization: SynchronizationV1,
    pub last_response: Option<PhotodiodeResponseV1>,
    pub freshness: FreshnessV1,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn common(request_id: u64) -> ResponseCommonV1 {
        ResponseCommonV1 {
            contract_version: CONTRACT_VERSION_V1,
            request_id: RequestId(request_id),
            owner_instance: OwnerInstanceId::from("owner-7"),
            run_id: Some(RunId::from("A1-20260721-003")),
            requested_revision: Some(SemanticRevision(4)),
            acknowledged_revision: Some(SemanticRevision(4)),
            outcome: RequestOutcomeV1::Applied,
            completed_at_unix_ms: Some(1_721_000_001_000),
            error: None,
        }
    }

    #[test]
    fn context_keys_are_stable_and_versioned() {
        assert_eq!(PLUGIN_ID_STAGE_A_MODULATION, "stage-a.modulation");
        assert_eq!(PLUGIN_ID_STAGE_A_PHOTODIODE, "stage-a.photodiode");
        assert_eq!(
            SERVICE_STAGE_A_MODULATION_CONTROL_V1,
            "stage_a.modulation.control.v1"
        );
        assert_eq!(
            SERVICE_STAGE_A_PHOTODIODE_CONTROL_V1,
            "stage_a.photodiode.control.v1"
        );
        assert_eq!(
            CTX_STAGE_A_MODULATION_REQUEST_V1,
            "stage_a.modulation_request.v1"
        );
        assert_eq!(
            CTX_STAGE_A_MODULATION_RESPONSE_V1,
            "stage_a.modulation_response.v1"
        );
        assert_eq!(
            CTX_STAGE_A_MODULATION_STATE_V1,
            "stage_a.modulation_state.v1"
        );
        assert_eq!(
            CTX_STAGE_A_PHOTODIODE_REQUEST_V1,
            "stage_a.photodiode_request.v1"
        );
        assert_eq!(
            CTX_STAGE_A_PHOTODIODE_RESPONSE_V1,
            "stage_a.photodiode_response.v1"
        );
        assert_eq!(
            CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
            "stage_a.photodiode_summary.v1"
        );
    }

    #[test]
    fn firmware_drive_bounds_and_a1_measurement_bounds_are_distinct() {
        assert!(drive_frequency_supported(DRIVE_FREQUENCY_MIN_MILLIHZ));
        assert!(drive_frequency_supported(DRIVE_FREQUENCY_MAX_MILLIHZ));
        assert!(!drive_frequency_supported(DRIVE_FREQUENCY_MIN_MILLIHZ - 1));
        assert!(!drive_frequency_supported(DRIVE_FREQUENCY_MAX_MILLIHZ + 1));
        assert_eq!(a1_measurement_frequency_limit_hz(20_000), 1_250.0);
        assert_eq!(a1_measurement_frequency_limit_hz(500_000), 31_250.0);
    }

    #[test]
    fn modulation_request_round_trips_with_semantic_discriminants() {
        let mut request = ModulationRequestV1::new(
            RequestId(12),
            ClientId::from("stage-a-a1"),
            ModulationCommandV1::PrepareA1 {
                configuration: A1AcquisitionConfigV1 {
                    waveform: PeriodicWaveformV1::Sine,
                    frequency_millihz: 10_000,
                    center_dac: 2_048,
                    amplitude_dac: 512,
                    sample_rate_hz: 20_000,
                    block_samples: 256,
                    emit_raw_samples: true,
                    emit_summary: true,
                    optical_lut_id: Some("lut-2026-07".into()),
                },
            },
        );
        request.target_owner_instance = Some(OwnerInstanceId::from("mod-owner-1"));
        request.lease_id = Some(LeaseId::from("lease-a1"));
        request.run_id = Some(RunId::from("run-3"));
        request.requested_revision = Some(SemanticRevision(9));
        request.issued_at_unix_ms = 42;

        let json = serde_json::to_value(&request).expect("serializes");
        assert_eq!(json["command"]["kind"], "prepare_a1");
        assert_eq!(
            json["command"]["configuration"]["frequency_millihz"],
            10_000
        );
        let decoded: ModulationRequestV1 = serde_json::from_value(json).expect("deserializes");
        assert_eq!(decoded, request);
    }

    #[test]
    fn set_optical_depth_round_trips_in_milli_units() {
        let request = ModulationRequestV1::new(
            RequestId(7),
            ClientId::from("stage-a-a1"),
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: 1_250,
            },
        );
        let json = serde_json::to_value(&request).expect("serializes");
        assert_eq!(json["command"]["kind"], "set_optical_depth");
        assert_eq!(json["command"]["depth_a_milli"], 1_250);
        let decoded: ModulationRequestV1 = serde_json::from_value(json).expect("deserializes");
        assert_eq!(decoded, request);
    }

    #[test]
    fn snapshots_keep_requested_and_acknowledged_revisions_distinct() {
        let requested = ModulationTargetV1 {
            revision: SemanticRevision(5),
            waveform: Some(WaveformV1::Constant { level_dac: 900 }),
            a1_configuration: None,
            acquisition_running: false,
            board_dac_code: None,
            firmware_configuration_revision: None,
        };
        let acknowledged = ModulationTargetV1 {
            revision: SemanticRevision(4),
            waveform: Some(WaveformV1::Constant { level_dac: 800 }),
            board_dac_code: Some(800),
            ..requested.clone()
        };
        let snapshot = ModulationStateV1 {
            contract_version: CONTRACT_VERSION_V1,
            owner_instance: OwnerInstanceId::from("mod-owner-1"),
            service_revision: 17,
            connection: ConnectionStateV1::Connected {
                port_label: "mock".into(),
                firmware_version: Some("0.4.0".into()),
            },
            capabilities: vec!["MOD".into(), "PDSTREAM".into()],
            lease: None,
            controller_state: ControllerStateV1::SafeIdle,
            active_run_id: None,
            requested: Some(requested),
            acknowledged: Some(acknowledged),
            synchronization: SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::RequestedRevisionNotAcknowledged,
                detail: Some("requested 5, acknowledged 4".into()),
            },
            last_response: None,
            freshness: FreshnessV1 {
                observed_at_unix_ms: 100,
                valid_for_ms: 500,
            },
            calibration_id: Some("pockels-20260724-120000".into()),
            optical_drive: Some(OpticalDriveStateV1 {
                target: OpticalTargetV1::LogSine,
                requested_mean_u_milli: 400,
                resolved_mean_u_milli: 399,
                internal_u_milli: 355,
                depth_a_milli: 1_000,
                v_null_dac: 1_630,
                v_peak_dac: 1_160,
            }),
        };
        let encoded = serde_json::to_vec(&snapshot).expect("serializes");
        let decoded: ModulationStateV1 = serde_json::from_slice(&encoded).expect("deserializes");
        assert_eq!(decoded.requested.unwrap().revision, SemanticRevision(5));
        assert_eq!(decoded.acknowledged.unwrap().revision, SemanticRevision(4));
        assert!(matches!(
            decoded.synchronization,
            SynchronizationV1::Unsynced { .. }
        ));
        assert_eq!(
            decoded
                .optical_drive
                .as_ref()
                .unwrap()
                .requested_mean_u_milli,
            400
        );
        assert_eq!(decoded.optical_drive.unwrap().resolved_mean_u_milli, 399);

        let mut legacy = serde_json::to_value(&snapshot).expect("serializes");
        let object = legacy.as_object_mut().expect("state object");
        object.remove("calibration_id");
        object.remove("optical_drive");
        let decoded_legacy: ModulationStateV1 =
            serde_json::from_value(legacy).expect("pre-provenance state decodes");
        assert!(decoded_legacy.calibration_id.is_none());
        assert!(decoded_legacy.optical_drive.is_none());
    }

    #[test]
    fn finalized_pdq_receipt_round_trips_without_raw_samples() {
        let receipt = PdqFinalizedReceiptV1 {
            run_id: RunId::from("run-3"),
            pdq_path: "/data/run-3_pd.pdq".into(),
            sidecar_path: "/data/run-3.toml".into(),
            opened_at_unix_ms: 1_000,
            finalized_at_unix_ms: 2_000,
            file_size_bytes: 8_192,
            sha256: Sha256V1::parse("ab".repeat(32)).expect("digest"),
            frames_written: 32,
            sample_frames_written: 30,
            sample_range: Some(SampleRangeV1 {
                first_sample_index: 10_000,
                end_sample_index_exclusive: 17_680,
                sample_count: 7_680,
            }),
            sample_rate_hz: Some(20_000),
            segment_count: 1,
            integrity: StreamIntegrityV1::default(),
            termination: PdqTerminationV1::Completed,
            valid: true,
        };
        let response = PhotodiodeResponseV1 {
            common: common(22),
            receipt: Some(PdqReceiptV1::Finalized(receipt.clone())),
        };
        let json = serde_json::to_value(&response).expect("serializes");
        assert_eq!(json["receipt"]["kind"], "finalized");
        assert!(json.to_string().len() < 2_048, "receipt stays bounded");
        let decoded: PhotodiodeResponseV1 = serde_json::from_value(json).expect("deserializes");
        assert_eq!(decoded.receipt, Some(PdqReceiptV1::Finalized(receipt)));
    }

    #[test]
    fn sha256_and_freshness_validate_boundaries() {
        assert!(Sha256V1::parse("0".repeat(64)).is_ok());
        assert!(
            Sha256V1::parse("A".repeat(64)).is_ok_and(|digest| digest.as_str() == "a".repeat(64))
        );
        assert!(Sha256V1::parse("0".repeat(63)).is_err());
        assert!(Sha256V1::parse("z".repeat(64)).is_err());
        assert!(serde_json::from_str::<Sha256V1>(&format!("\"{}\"", "z".repeat(64))).is_err());

        let freshness = FreshnessV1 {
            observed_at_unix_ms: 1_000,
            valid_for_ms: 500,
        };
        assert!(!freshness.is_stale_at(1_500));
        assert!(freshness.is_stale_at(1_501));
        assert!(!freshness.is_stale_at(900), "clock rollback saturates");
    }

    #[test]
    fn stream_integrity_is_fail_closed() {
        assert!(StreamIntegrityV1::default().is_clean());
        assert!(!StreamIntegrityV1 {
            segment_restarts: 1,
            ..StreamIntegrityV1::default()
        }
        .is_clean());
    }

    #[test]
    fn additive_v1_fields_decode_from_payloads_that_predate_them() {
        // An older owner's stream block carries no `level`.
        let stream: PhotodiodeStreamV1 = serde_json::from_value(json!({
            "stream_epoch": 3,
            "sample_range": null,
            "sample_rate_hz": 20_000,
            "latest_adc_code": 1_024,
            "integrity": StreamIntegrityV1::default(),
        }))
        .expect("stream without level decodes");
        assert!(stream.level.is_none());

        let level = PhotodiodeLevelV1 {
            mean_volts: 1.5,
            peak_to_peak_volts: 0.01,
            sample_count: 4_096,
            end_sample_index: 1_000_000,
            clipped: false,
        };
        let round_tripped: PhotodiodeLevelV1 =
            serde_json::from_value(serde_json::to_value(level).expect("serializes"))
                .expect("deserializes");
        assert_eq!(round_tripped, level);
        // The window is identified without a wall clock: it ends at
        // `end_sample_index` and spans `sample_count` samples.
        assert_eq!(
            level.end_sample_index - level.sample_count,
            1_000_000 - 4_096
        );
    }
}
