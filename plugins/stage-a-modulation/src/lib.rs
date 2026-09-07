//! Stage-A laser modulation control.
//!
//! Drives the laser modulation input (Hermit J23, `DAC1.4`/address 3) through
//! the firmware 0.3.0 `MOD` command. Two orthogonal settings define a drive:
//! the method selects a manually entered or optically calibrated DAC band,
//! while the mode selects the waveform that fills that band. A separate max
//! limit is the hard DAC ceiling for every drive. Every accepted change is
//! transferred to the Teensy immediately, with no Apply button.
//!
//! **Frame-independent by design.** The host only calls `process_frame()`
//! while camera frames flow, so nothing here depends on it: connecting is a
//! checkbox *setting* (settings arrive from the UI thread at any time), a
//! dedicated device thread owns the serial client, and slider changes are
//! coalesced into a pending-command slot that thread drains. The bench works
//! with no camera attached. `process_frame()` only tears the connection down
//! defensively in replay mode.
//!
//! The plugin owns the Teensy **command port**; the photodiode stream port is
//! owned by `stage-a-photodiode`. The firmware output is set-and-hold
//! (`stage-a-controller` ADR 002): disconnecting does NOT switch the
//! modulation off — drag the power slider to 0 to drive 0 V.

mod calibration;
#[cfg(test)]
mod protocol_validation_tests;
mod waveform;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use augur_plugin_api::{
    export_plugin, EventStoreHandle, ExecutionMode, HostContext, HostDatasetDescriptor,
    HostDatasetKind, HostOutput, HostViewDescriptor, HostViewKind, HostViewPlacement,
    HostViewRegistry, PathDialogKind, Plugin, PluginControlContext, PluginControlSnapshot,
    PluginFrame, PluginRuntimeRole, PluginServiceOutcome, PluginServiceReply, PluginServiceRequest,
    Series1dLine, Series1dPoint, Series1dV1, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1,
    TableSchema, TableValueType,
};
use serde_json::{json, Value};
use stage_a_io::{Command, DeviceEvent, MockController, StageAClient, Transport};
use stage_a_plugin_contract::drive_frequency_supported;
use stage_a_plugin_contract::{
    A2AcquisitionConfigV1, A2TimingReferenceV1, ClientId, ConnectionStateV1, ControllerStateV1,
    FreshnessV1, LeaseId, LeaseSnapshotV1, ModulationCommandV1, ModulationRequestV1,
    ModulationResponseV1, ModulationStateV1, ModulationTargetV1, OpticalDriveStateV1,
    OpticalLobeStateV1, OpticalTargetV1, OwnerInstanceId, PhotodiodeLevelV1, PhotodiodeSummaryV1,
    RequestOutcomeV1, ResponseCommonV1, RunId, SemanticRevision, ServiceErrorCodeV1,
    ServiceErrorV1, SynchronizationV1, UnsyncedReasonV1, WaveformV1, CONTRACT_VERSION_V1,
    CTX_STAGE_A_MODULATION_STATE_V1, CTX_STAGE_A_PHOTODIODE_SUMMARY_V1,
    DRIVE_FREQUENCY_MAX_MILLIHZ, DRIVE_FREQUENCY_MIN_MILLIHZ, PLUGIN_ID_STAGE_A_MODULATION,
    PLUGIN_ID_STAGE_A_PHOTODIODE, SERVICE_STAGE_A_MODULATION_CONTROL_V1,
};

const STATUS_DATASET_ID: &str = "stage-a-modulation.status";
const STATUS_VIEW_ID: &str = "stage-a-modulation.status.view";
const CURVE_DATASET_ID: &str = "stage-a-modulation.transfer-curve";
const CURVE_VIEW_ID: &str = "stage-a-modulation.transfer-curve.view";

/// Codes measured per sweep pass. 49 points over the full range put a sample
/// every ~85 codes, ~19 per lobe at a typical Vπ of 860.
const SWEEP_POINTS_PER_PASS: usize = 49;
/// How long the detector must have run *after* a code was commanded before its
/// window counts as settled — enough for the HV amplifier and the cell to
/// arrive, proven from the sample clock rather than assumed from a timer.
///
/// A duration, not a sample count: settling is a property of the amplifier and
/// the crystal, not of the acquisition rate. The former fixed 2 000 samples was
/// written for 20 kSa/s (100 ms) and silently became 4 ms when the bench moved to
/// 500 kSa/s.
const SETTLE_SECONDS: f64 = 0.1;
/// Settle window when the photodiode has not published a sample rate. Matches
/// [`SETTLE_SECONDS`] at the firmware's original 20 kSa/s.
const SETTLE_SAMPLES: u64 = 2_000;
/// Give up on a point if no settled level arrives within this long. A stalled
/// photodiode stream must abort the sweep, not hang it.
const POINT_TIMEOUT: Duration = Duration::from_secs(5);
/// Warn (never block) above this residual, as a fraction of the detector span.
/// A clean bench sits near 1 %; a stray point or two reaches ~10 % while `Vπ`
/// stays good, which is why this warns rather than refuses.
const WARN_QUALITY: f64 = 0.05;
/// Warn above this ascending/descending disagreement, as a fraction of the span.
const WARN_HYSTERESIS: f64 = 0.05;

const MAX_DAC_CODE: i64 = 4_095;
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEVICE_LOOP_TICK: Duration = Duration::from_millis(10);
/// After this many serial requests failing in a row the device thread declares
/// the link dead and exits, so the owner can reap it and reconnect. A wedged
/// link that stays "up" otherwise swallows every queued command while the
/// settings UI keeps responding.
const DEVICE_MAX_CONSECUTIVE_ERRORS: u32 = 5;
/// Minimum spacing between automatic reconnect attempts after the device
/// thread died.
const RECONNECT_BACKOFF_MS: u64 = 2_000;
const REQUEST_CACHE_LIMIT: usize = 256;
const MIN_LEASE_TTL_MS: u64 = 250;
const MAX_LEASE_TTL_MS: u64 = 60_000;

/// The lobe a calibration was last applied to, published process-wide.
///
/// The host runs two instances of this plugin — a UI mirror that renders the
/// settings and a live worker that owns the device link — and settings only
/// ever travel *mirror → worker*: every live-analysis pass collects
/// `get_setting` from the mirror and writes it onto the worker.
///
/// The measured fit lives on the worker (it is the instance with the
/// photodiode and the DAC), so "Apply to V_null / V_peak" wrote the new lobe
/// into the worker's fields and the very next sync overwrote it with the
/// mirror's stale codes. The button appeared to do nothing, twice over: the
/// mirror had no fit to apply, and the worker's result did not survive a tick.
///
/// Both instances live in the same process, so the applied lobe is published
/// here with a monotonic generation and adopted by whichever instance is
/// behind. The generation is what makes adoption one-way: a mirror that has
/// already seen generation *n* keeps accepting ordinary edits.
static APPLIED_LOBE: Mutex<Option<AppliedLobe>> = Mutex::new(None);
static APPLIED_LOBE_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct AppliedLobe {
    generation: u64,
    v_null_dac: i64,
    v_peak_dac: i64,
    calibration_id: String,
}

/// Publishes a freshly applied lobe to every instance in this process.
fn publish_applied_lobe(v_null_dac: i64, v_peak_dac: i64, calibration_id: String) -> u64 {
    let generation = APPLIED_LOBE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    if let Ok(mut slot) = APPLIED_LOBE.lock() {
        *slot = Some(AppliedLobe {
            generation,
            v_null_dac,
            v_peak_dac,
            calibration_id,
        });
    }
    generation
}

/// The applied lobe an instance at `seen` generation has not adopted yet.
fn applied_lobe_after(seen: u64) -> Option<AppliedLobe> {
    if APPLIED_LOBE_GENERATION.load(Ordering::SeqCst) <= seen {
        return None;
    }
    APPLIED_LOBE
        .lock()
        .ok()?
        .clone()
        .filter(|applied| applied.generation > seen)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Const,
    /// Pure DAC sine (DAC_SINE): the firmware synthesises a sinusoid directly in
    /// DAC codes. The optical output is the non-linear `sin²` of this drive.
    Sine,
    Square,
    /// OPTICAL_LOG_SINE: the DAC is warped so the *optical* output is a
    /// log-intensity sine (the clean A1 target). Requires the lobe inversion.
    OpticalLogSine,
    /// OPTICAL_LINEAR_SINE: the DAC is warped so the optical output is a
    /// linear-intensity sine.
    OpticalLinearSine,
}

impl Mode {
    const VARIANTS: [Mode; 5] = [
        Mode::Const,
        Mode::Sine,
        Mode::Square,
        Mode::OpticalLogSine,
        Mode::OpticalLinearSine,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Const => "CONST",
            Self::Sine => "DAC_SINE",
            Self::Square => "SQUARE",
            Self::OpticalLogSine => "OPTICAL_LOG_SINE",
            Self::OpticalLinearSine => "OPTICAL_LINEAR_SINE",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        // Accept the historical "SINE" alias for the pure DAC sine.
        if name == "SINE" {
            return Some(Self::Sine);
        }
        Self::VARIANTS.into_iter().find(|m| m.name() == name)
    }

    fn is_periodic(self) -> bool {
        !matches!(self, Self::Const)
    }

    /// Firmware `wave` token. Optical modes upload a warp table and share the
    /// `WARP` playback path.
    fn wire_wave(self) -> &'static str {
        match self {
            Self::Const => "CONST",
            Self::Sine => "SINE",
            Self::Square => "SQUARE",
            Self::OpticalLogSine | Self::OpticalLinearSine => "WARP",
        }
    }

    fn optical_target(self) -> Option<waveform::OpticalTarget> {
        match self {
            Self::OpticalLogSine => Some(waveform::OpticalTarget::LogSine),
            Self::OpticalLinearSine => Some(waveform::OpticalTarget::LinearSine),
            _ => None,
        }
    }

    /// How this mode's peak intensity follows from `ū` and `a` on a calibrated
    /// band. This is what bounds both controls — see [`waveform::PeakLaw`].
    fn peak_law(self) -> waveform::PeakLaw {
        match self.optical_target() {
            Some(target) => waveform::PeakLaw::of(target),
            None => match self {
                Self::Const => waveform::PeakLaw::Constant,
                _ => waveform::PeakLaw::LogSwing,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveMethod {
    Manual,
    Calibrated,
}

impl DriveMethod {
    const VARIANTS: [Self; 2] = [Self::Manual, Self::Calibrated];

    fn name(self) -> &'static str {
        match self {
            Self::Manual => "MANUAL",
            Self::Calibrated => "CALIBRATED",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::VARIANTS
            .into_iter()
            .find(|method| method.name() == name)
    }
}

/// State the device thread reports back for the UI (status entries, table).
struct DeviceState {
    connected: bool,
    firmware: String,
    capabilities: Vec<String>,
    board_code: Option<i64>,
    board_mod: String,
    /// Structured board-echoed modulation (`mod_wave`/`mod_level`/`mod_min`/
    /// `mod_freq_mhz` reply fields). Lets the published snapshot expose the
    /// *operator-armed* drive to consumers (A1 derives its fallback
    /// modulation period from it) — UI-driven MOD commands never populate the
    /// service-path `acknowledged` target.
    board_wave: Option<String>,
    board_level: Option<i64>,
    board_min: Option<i64>,
    board_freq_millihz: Option<u64>,
    last_error: Option<String>,
    controller_state: ControllerStateV1,
    /// Acquisition settings echoed by `STATUS`/`CONFIG`. `CONFIG` needs a mode
    /// *and* a rate, so a mode change has to restate the rest of the
    /// controller's configuration rather than invent it.
    controller_mode: Option<String>,
    controller_rate_hz: Option<u32>,
    controller_block_samples: Option<u32>,
    controller_emit_raw: Option<bool>,
    controller_emit_summary: Option<bool>,
    requested: Option<ModulationTargetV1>,
    acknowledged: Option<ModulationTargetV1>,
    last_response: Option<ModulationResponseV1>,
    last_device_update_unix_ms: u64,
    marker_diagnostics: Option<stage_a_plugin_contract::A2MarkerDiagnosticsV1>,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            connected: false,
            firmware: String::new(),
            capabilities: Vec::new(),
            controller_mode: None,
            controller_rate_hz: None,
            controller_block_samples: None,
            controller_emit_raw: None,
            controller_emit_summary: None,
            board_code: None,
            board_mod: String::new(),
            board_wave: None,
            board_level: None,
            board_min: None,
            board_freq_millihz: None,
            last_error: None,
            controller_state: ControllerStateV1::Unknown,
            requested: None,
            acknowledged: None,
            last_response: None,
            last_device_update_unix_ms: 0,
            marker_diagnostics: None,
        }
    }
}

impl DeviceState {
    /// Board-echo view of the armed drive as a contract target (revision 0),
    /// for the published snapshot when no service-path acknowledgement
    /// exists. WARP (optical) drives report as `Periodic` — the consumers of
    /// this fallback only need the modulation frequency.
    fn board_echo_target(&self) -> Option<ModulationTargetV1> {
        let wave = self.board_wave.as_deref()?;
        let level = || u16::try_from(self.board_level.unwrap_or(0)).unwrap_or(0);
        let min = || u16::try_from(self.board_min.unwrap_or(0)).unwrap_or(0);
        let waveform = match wave {
            "OFF" => WaveformV1::Off,
            "CONST" => WaveformV1::Constant { level_dac: level() },
            "SINE" | "WARP" => WaveformV1::Periodic {
                waveform: stage_a_plugin_contract::PeriodicWaveformV1::Sine,
                min_dac: min(),
                max_dac: level(),
                frequency_millihz: self.board_freq_millihz.unwrap_or(0),
            },
            "SQUARE" => WaveformV1::Periodic {
                waveform: stage_a_plugin_contract::PeriodicWaveformV1::Square,
                min_dac: min(),
                max_dac: level(),
                frequency_millihz: self.board_freq_millihz.unwrap_or(0),
            },
            _ => return None,
        };
        Some(ModulationTargetV1 {
            revision: SemanticRevision(0),
            waveform: Some(waveform),
            a2_configuration: None,
            acquisition_running: self.controller_state == ControllerStateV1::Running,
            board_dac_code: self.board_code.and_then(|code| u16::try_from(code).ok()),
            firmware_configuration_revision: None,
        })
    }
}

#[derive(Clone)]
struct OperationMeta {
    request_id: stage_a_plugin_contract::RequestId,
    requester: ClientId,
    run_id: Option<RunId>,
    requested_revision: SemanticRevision,
    target: ModulationTargetV1,
    owner_instance: OwnerInstanceId,
}

struct PendingOperation {
    commands: Vec<Command>,
    purpose: &'static str,
    meta: Option<OperationMeta>,
}

/// Everything shared between the plugin (UI thread) and the device thread.
struct SharedLink {
    state: Mutex<DeviceState>,
    /// Latest not-yet-sent command; newer settings overwrite older ones so
    /// slider drags coalesce instead of queueing.
    pending: Mutex<Option<PendingOperation>>,
    priority: Mutex<Option<PendingOperation>>,
    automation: Mutex<VecDeque<PendingOperation>>,
    completions: Mutex<VecDeque<(ClientId, ModulationResponseV1)>>,
    stop: AtomicBool,
    fail_closed_on_stop: AtomicBool,
    generation: AtomicU64,
}

impl SharedLink {
    fn new() -> Self {
        Self {
            state: Mutex::new(DeviceState::default()),
            pending: Mutex::new(None),
            priority: Mutex::new(None),
            automation: Mutex::new(VecDeque::new()),
            completions: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            fail_closed_on_stop: AtomicBool::new(false),
            generation: AtomicU64::new(1),
        }
    }

    fn bump(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }
}

/// In-process mock controller thread behind the `mock` port.
struct MockService {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl MockService {
    fn spawn() -> (Self, StageAClient<stage_a_io::MockTransport>) {
        let link = stage_a_io::MockLink::new();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let mut controller = MockController::new(link.device_end()).with_waveform_extension();
        let join = std::thread::Builder::new()
            .name("stage-a-modulation-mock".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    controller.poll_commands();
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
            .expect("spawning the mock controller thread must succeed");
        (
            Self {
                stop,
                join: Some(join),
            },
            StageAClient::new(link.host_end()),
        )
    }
}

impl Drop for MockService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Handle to the running device thread; dropping it stops the thread.
struct DeviceLink {
    shared: Arc<SharedLink>,
    join: Option<JoinHandle<()>>,
    _mock: Option<MockService>,
}

impl Drop for DeviceLink {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Device thread: HELLO once, then drain the pending command slot and poll
/// STATUS. All serial I/O lives here — the UI thread never blocks.
fn run_device<T: Transport>(mut client: StageAClient<T>, shared: Arc<SharedLink>) {
    match client.request(&Command::new("HELLO").field("protocol", 1)) {
        Ok(fields) => {
            let mut state = shared.state.lock().expect("device state lock");
            state.connected = true;
            state.controller_state = ControllerStateV1::SafeIdle;
            state.firmware = fields
                .get("firmware")
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            state.capabilities = fields
                .get("capabilities")
                .map(|value| value.split(',').map(str::to_owned).collect())
                .unwrap_or_default();
            let has_mod = fields
                .get("capabilities")
                .is_some_and(|caps| caps.split(',').any(|c| c == "MOD"));
            state.last_error = (!has_mod).then(|| {
                "firmware has no MOD capability — flash stage-a-controller 0.3.0+".to_owned()
            });
        }
        Err(err) => {
            let mut state = shared.state.lock().expect("device state lock");
            state.connected = false;
            state.controller_state = ControllerStateV1::Faulted;
            state.last_error = Some(format!("HELLO failed: {err}"));
            shared.bump();
            return;
        }
    }
    shared.bump();

    let mut last_status = Instant::now() - STATUS_POLL_INTERVAL;
    let mut consecutive_errors = 0u32;
    while !shared.stop.load(Ordering::Relaxed) {
        let priority = shared.priority.lock().expect("priority lock").take();
        let pending = priority
            .or_else(|| {
                shared
                    .automation
                    .lock()
                    .expect("automation lock")
                    .pop_front()
            })
            .or_else(|| shared.pending.lock().expect("pending lock").take());
        if let Some(operation) = pending {
            if execute_operation(&mut client, &shared, operation) {
                consecutive_errors = 0;
            } else {
                consecutive_errors += 1;
            }
        } else if last_status.elapsed() >= STATUS_POLL_INTERVAL {
            last_status = Instant::now();
            let result = client.request(&Command::new("STATUS"));
            if result.is_ok() {
                consecutive_errors = 0;
            } else {
                consecutive_errors += 1;
            }
            apply_status_reply(&shared, "STATUS", result);
        } else {
            if let Ok(events) = client.poll_events() {
                apply_device_events(&shared, events);
            }
            std::thread::sleep(DEVICE_LOOP_TICK);
        }
        if consecutive_errors >= DEVICE_MAX_CONSECUTIVE_ERRORS {
            // The link is wedged (unplugged cable, stale fd): declare it dead
            // so the owner reaps this thread and reconnects, instead of
            // silently swallowing every queued command from here on.
            let mut state = shared.state.lock().expect("device state lock");
            state.connected = false;
            state.controller_state = ControllerStateV1::Faulted;
            state.last_error = Some("serial link failed repeatedly — reconnecting".to_owned());
            drop(state);
            shared.bump();
            return;
        }
    }

    if shared.fail_closed_on_stop.load(Ordering::Relaxed) {
        let _ = client.request(&Command::new("STOP").field("reason", "owner_shutdown"));
        let result = client.request(&Command::new("MOD").field("wave", "OFF"));
        apply_status_reply(&shared, "SAFE_OFF", result);
    }

    let mut state = shared.state.lock().expect("device state lock");
    state.connected = false;
    shared.bump();
}

/// Runs one queued operation; returns whether every command succeeded.
fn execute_operation<T: Transport>(
    client: &mut StageAClient<T>,
    shared: &SharedLink,
    operation: PendingOperation,
) -> bool {
    let requester = operation.meta.as_ref().map(|meta| meta.requester.clone());
    let mut merged = BTreeMap::new();
    let mut error = None;
    for command in &operation.commands {
        match client.request(command) {
            Ok(fields) => merged.extend(fields),
            Err(err) => {
                error = Some(err.to_string());
                break;
            }
        }
        if let Ok(events) = client.poll_events() {
            apply_device_events(shared, events);
        }
    }

    // Snapshot the firmware's loss counters at A2 command boundaries. USB
    // frame CRCs cannot reveal a marker dropped before it was serialized.
    if error.is_none()
        && operation
            .meta
            .as_ref()
            .is_some_and(|m| m.target.a2_configuration.is_some())
    {
        match client.request(&Command::new("STATUS")) {
            Ok(fields) => merged.extend(fields),
            Err(e) => error = Some(format!("A2 status readback failed: {e}")),
        }
    }

    if error.is_none() && operation.purpose == "PREPARE_A2" {
        let reference = operation
            .meta
            .as_ref()
            .and_then(|m| m.target.a2_configuration.as_ref())
            .map_or(A2TimingReferenceV1::Comparator, |c| c.timing_reference);
        if let Err(reason) = verify_a2_prepared(&merged, reference) {
            error = Some(reason);
        }
    }

    if error.is_none()
        && operation.purpose == "PREPARE_A1"
        && (merged.get("mode").map(String::as_str) != Some("A1")
            || merged.get("state").map(String::as_str) != Some("RUNNING")
            || merged.get("trigger_source").map(String::as_str) != Some("J24_PHASE0")
            || merged.get("cmp_armed").map(String::as_str) != Some("0"))
    {
        error = Some("A1 preparation not confirmed: expected mode=A1, running acquisition, J24_PHASE0 and comparator off".into());
    }

    let mut state = shared.state.lock().expect("device state lock");
    let succeeded = error.is_none();
    if let Some(message) = error {
        state.last_error = Some(format!("{}: {message}", operation.purpose));
        if let Some(meta) = operation.meta {
            state.last_response = Some(ModulationResponseV1 {
                marker_diagnostics: state.marker_diagnostics,
                common: ResponseCommonV1 {
                    contract_version: CONTRACT_VERSION_V1,
                    request_id: meta.request_id,
                    owner_instance: meta.owner_instance,
                    run_id: meta.run_id,
                    requested_revision: Some(meta.requested_revision),
                    acknowledged_revision: state.acknowledged.as_ref().map(|value| value.revision),
                    outcome: RequestOutcomeV1::Rejected,
                    completed_at_unix_ms: Some(now_unix_ms()),
                    error: Some(ServiceErrorV1 {
                        code: ServiceErrorCodeV1::DeviceRejected,
                        message,
                        retryable: false,
                    }),
                },
                controller_state: state.controller_state,
                acknowledged_target: state.acknowledged.clone(),
            });
        }
    } else {
        apply_reply_fields(&mut state, &merged);
        state.last_error = None;
        if let Some(meta) = operation.meta {
            let mut acknowledged = meta.target;
            if operation.purpose == "MOD" {
                acknowledged.waveform =
                    state.board_echo_target().and_then(|target| target.waveform);
            }
            acknowledged.board_dac_code =
                state.board_code.and_then(|code| u16::try_from(code).ok());
            acknowledged.firmware_configuration_revision = merged
                .get("rev")
                .and_then(|value| value.parse::<u64>().ok());
            state.acknowledged = Some(acknowledged.clone());
            state.last_response = Some(ModulationResponseV1 {
                marker_diagnostics: state.marker_diagnostics,
                common: ResponseCommonV1 {
                    contract_version: CONTRACT_VERSION_V1,
                    request_id: meta.request_id,
                    owner_instance: meta.owner_instance,
                    run_id: meta.run_id,
                    requested_revision: Some(meta.requested_revision),
                    acknowledged_revision: Some(meta.requested_revision),
                    outcome: RequestOutcomeV1::Applied,
                    completed_at_unix_ms: Some(now_unix_ms()),
                    error: None,
                },
                controller_state: state.controller_state,
                acknowledged_target: Some(acknowledged),
            });
        }
    }
    if let Some(requester) = requester {
        if let Some(response) = state.last_response.clone() {
            let mut completions = shared.completions.lock().expect("completions lock");
            completions.push_back((requester, response));
            while completions.len() > 128 {
                completions.pop_front();
            }
        }
    }
    state.last_device_update_unix_ms = now_unix_ms();
    drop(state);
    shared.bump();
    succeeded
}

fn apply_status_reply(
    shared: &SharedLink,
    purpose: &str,
    result: Result<BTreeMap<String, String>, stage_a_io::ClientError>,
) {
    let mut state = shared.state.lock().expect("device state lock");
    match result {
        Ok(fields) => {
            apply_reply_fields(&mut state, &fields);
            if purpose == "MOD" {
                state.last_error = None;
            }
        }
        Err(err) => state.last_error = Some(format!("{purpose}: {err}")),
    }
    state.last_device_update_unix_ms = now_unix_ms();
    drop(state);
    shared.bump();
}

fn apply_reply_fields(state: &mut DeviceState, fields: &BTreeMap<String, String>) {
    if let (Some(marker_drops), Some(stream_marker_drops)) = (
        fields.get("marker_drops").and_then(|v| v.parse().ok()),
        fields
            .get("stream_marker_drops")
            .and_then(|v| v.parse().ok()),
    ) {
        state.marker_diagnostics = Some(stage_a_plugin_contract::A2MarkerDiagnosticsV1 {
            dma_sample_clock: fields.get("marker_clock").map(String::as_str)
                == Some("dma_cursor_v1"),
            marker_drops,
            stream_marker_drops,
            observed_at_unix_ms: now_unix_ms(),
        });
    }

    if let Some(code) = fields.get("code").and_then(|v| v.parse::<i64>().ok()) {
        state.board_code = Some(code);
    }
    if let Some(mode) = fields.get("mode") {
        state.controller_mode = Some(mode.clone());
    }
    if let Some(rate) = fields.get("rate_hz").and_then(|v| v.parse::<u32>().ok()) {
        state.controller_rate_hz = Some(rate);
    }
    if let Some(block) = fields
        .get("block_samples")
        .and_then(|v| v.parse::<u32>().ok())
    {
        state.controller_block_samples = Some(block);
    }
    if let Some(raw) = fields.get("raw") {
        state.controller_emit_raw = Some(raw == "1");
    }
    if let Some(summary) = fields.get("summary") {
        state.controller_emit_summary = Some(summary == "1");
    }
    if let Some(controller) = fields.get("state") {
        state.controller_state = match controller.as_str() {
            "SAFE_IDLE" => ControllerStateV1::SafeIdle,
            "CONFIGURED" => ControllerStateV1::Configured,
            "RUNNING" => ControllerStateV1::Running,
            _ => ControllerStateV1::Unknown,
        };
    }
    if let Some(wave) = fields.get("mod_wave") {
        let level = fields.get("mod_level").map(String::as_str).unwrap_or("?");
        let min = fields.get("mod_min").map(String::as_str).unwrap_or("?");
        // Parse the echoed frequency exactly once, as f64. Parsing it a second
        // time as u64 silently yielded None the moment the firmware echoed a
        // decimal ("10000.0"): `board_echo_target` then published
        // frequency_millihz: 0, A1 rejected it, and A1 lost its only fallback
        // modulation period whenever the EXT_TRIGGER markers were absent.
        let freq_millihz = fields
            .get("mod_freq_mhz")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|hz| hz.is_finite() && *hz >= 0.0);
        let freq_mhz = freq_millihz.unwrap_or(0.0);
        state.board_mod = if wave == "SINE" || wave == "SQUARE" {
            format!("{wave} {min}..{level} @ {:.3} Hz", freq_mhz / 1_000.0)
        } else {
            format!("{wave} level={level}")
        };
        state.board_wave = Some(wave.clone());
        state.board_level = fields.get("mod_level").and_then(|v| v.parse().ok());
        state.board_min = fields.get("mod_min").and_then(|v| v.parse().ok());
        state.board_freq_millihz = freq_millihz.map(|hz| hz.round() as u64);
    }
}

fn apply_device_events(shared: &SharedLink, events: Vec<DeviceEvent>) {
    let fault = events.into_iter().find_map(|event| match event {
        DeviceEvent::Async { name, fields } if name == "FAULT" => Some(
            fields
                .get("code")
                .cloned()
                .unwrap_or_else(|| "unknown".into()),
        ),
        _ => None,
    });
    if let Some(code) = fault {
        let mut state = shared.state.lock().expect("device state lock");
        state.controller_state = ControllerStateV1::Faulted;
        state.last_error = Some(format!("controller fault: {code}"));
        state.last_device_update_unix_ms = now_unix_ms();
        drop(state);
        shared.bump();
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// A transfer-curve sweep in flight. One point at a time: command a settled
/// `CONST` code, wait for a photodiode window that *starts* after the command,
/// record it, move on.
struct CalibrationSweep {
    /// Remaining `(code, direction)` steps, and the points collected so far.
    steps: Vec<(u16, calibration::Direction)>,
    index: usize,
    points: Vec<calibration::SweepPoint>,
    /// Detector sample index when the current code was commanded. A level only
    /// counts once its window begins after this plus [`SETTLE_SAMPLES`], which
    /// needs no shared clock and tolerates any tick jitter.
    commanded_at_sample: Option<u64>,
    /// Wall-clock guard for a stream that stops delivering entirely.
    point_started: Instant,
    /// Drive to restore when the sweep ends, however it ends.
    restore: Option<Command>,
    /// Max limit in force when the sweep started; the fit's branch constraint.
    max_code: u16,
}

/// Forwards momentary button presses across the host's UI-mirror → live-worker
/// settings snapshot. A click arrives as `true` on the clicked instance; the
/// other instance only ever sees the snapshot value from `get_setting`, so the
/// press is transported as a monotonic counter and a counter advance counts as
/// one press edge. The first counter a fresh instance sees is adopted silently
/// so a reloaded worker does not replay old presses (ADR 010).
///
/// The baseline is tracked separately from the counter: folding the two
/// together makes a fresh worker mistake the operator's *first* real press for
/// its initial sight of the counter and swallow it.
#[derive(Debug, Default, Clone, Copy)]
struct PressLatch {
    counter: u64,
    seen: Option<u64>,
}

impl PressLatch {
    /// Interprets a settings write to this button; returns true on a press edge.
    fn accept(&mut self, value: &Value) -> bool {
        if value.as_bool() == Some(true) {
            self.counter += 1;
            self.seen = Some(self.counter);
            return true;
        }
        let Some(incoming) = value.as_u64() else {
            return false;
        };
        match self.seen {
            None => {
                self.seen = Some(incoming);
                self.counter = self.counter.max(incoming);
                false
            }
            Some(seen) if incoming > seen => {
                self.seen = Some(incoming);
                self.counter = self.counter.max(incoming);
                true
            }
            Some(_) => false,
        }
    }

    fn value(&self) -> Value {
        json!(self.counter)
    }
}

impl CalibrationSweep {
    fn total(&self) -> usize {
        self.steps.len()
    }

    fn current(&self) -> Option<(u16, calibration::Direction)> {
        self.steps.get(self.index).copied()
    }
}

pub struct StageAModulationPlugin {
    enabled: bool,
    runtime_role: PluginRuntimeRole,
    effects_allowed: bool,
    owner_instance: OwnerInstanceId,
    lease: Option<ControlLease>,
    deferred_release_request: Option<stage_a_plugin_contract::RequestId>,
    deferred_release_ack_published: bool,
    request_cache: VecDeque<(PluginServiceRequest, PluginServiceReply)>,
    link: Option<DeviceLink>,
    shared: Arc<SharedLink>,
    // -- settings (every accepted change is sent immediately) --
    connect_requested: bool,
    port_hint: String,
    max_level: i64,
    level: i64,
    min_level: i64,
    method: DriveMethod,
    mode: Mode,
    frequency_hz: f64,
    // -- optical drive inversion (OPTICAL_* modes) --
    /// Requested optical log-modulation depth `a = ln(I_max / I_min)`.
    depth_a: f64,
    /// The operator's armed `depth_a`, parked while a lease drives the optical
    /// depth (A1's amplitude sweep) and restored by [`Self::end_lease`].
    armed_depth_a: Option<f64>,
    /// The operator's armed `frequency_hz`, parked while a lease drives the
    /// frequency (A1's frequency sweep) and restored by [`Self::end_lease`].
    armed_frequency_hz: Option<f64>,
    /// The operator's armed `operating_point`, parked while a lease drives it
    /// (A1's `I_k` sweep) and restored by [`Self::end_lease`].
    armed_operating_point: Option<f64>,
    /// Dimensionless, floor-subtracted **cycle-mean** lobe coordinate
    /// `ū ∈ (0,1]`; not the physical A1 flux point `I_k`.
    operating_point: f64,
    /// DAC code at the excitation minimum of one monotonic Pockels lobe.
    v_null_dac: i64,
    /// DAC code at the excitation maximum of that lobe. An absolute code like
    /// `v_null_dac`, not a distance: the half-wave span is derived from the
    /// pair (see [`waveform::LobeInversion::resolve`]).
    v_peak_dac: i64,
    // -- measured transfer calibration --
    /// Bench detector geometry. Not inferable from a sweep — see
    /// [`calibration::DetectorGeometry`].
    detector_geometry: calibration::DetectorGeometry,
    /// Sweep in flight, ticked from `process_control`.
    sweep: Option<CalibrationSweep>,
    /// Last completed fit, awaiting review and an explicit apply.
    fit: Option<calibration::TransferFit>,
    /// Set once a fit has been applied to `v_null_dac`/`v_peak_dac`; published on
    /// the contract so a consumer's sidecar can cite the inversion in use.
    calibration_id: Option<String>,
    /// Highest [`APPLIED_LOBE`] generation this instance has taken on. Below
    /// the published one, its `v_null_dac`/`v_peak_dac` are stale and must not
    /// be exported into the settings snapshot.
    applied_lobe_generation: u64,
    /// Directory for the archived calibration record; empty means "apply the
    /// fit but do not archive it".
    calibration_dir: String,
    /// Operator-visible outcome of the last sweep or apply.
    calibration_status: String,
    /// Momentary calibration buttons, forwarded mirror → worker (ADR 010).
    press_measure: PressLatch,
    press_apply: PressLatch,
    last_error: Option<String>,
    /// Last automatic reconnect attempt after the device thread died, for the
    /// watchdog backoff in `apply_execution_context`.
    last_reconnect_ms: u64,
}

/// What the configured lobe and DAC ceiling can express right now. Shown to
/// the operator and used to clamp edits instead of refusing them.
#[derive(Debug, Clone, Copy)]
struct Achievable {
    /// Highest normalised intensity the DAC ceiling leaves reachable.
    u_max: f64,
    /// Where the current drive actually peaks, against that ceiling.
    peak_u: f64,
    /// Deepest `a` at the current operating point.
    max_depth_a: f64,
    /// Brightest operating point at the current `a`.
    max_mean_u: f64,
}

/// Which of the two coupled optical controls the operator just moved. The one
/// they touched is the one [`StageAModulationPlugin::reconcile_drive`] limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveKnob {
    Depth,
    Mean,
    /// The lobe, the ceiling or the mode moved, so neither control has priority.
    Lobe,
}

#[derive(Clone)]
struct ControlLease {
    lease_id: LeaseId,
    holder: ClientId,
    run_id: Option<RunId>,
    expires_at_unix_ms: u64,
}

impl Default for StageAModulationPlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            runtime_role: PluginRuntimeRole::UiMirror,
            effects_allowed: false,
            owner_instance: OwnerInstanceId::new(format!(
                "modulation-{}-{}",
                std::process::id(),
                now_unix_ms()
            )),
            lease: None,
            deferred_release_request: None,
            deferred_release_ack_published: false,
            request_cache: VecDeque::new(),
            link: None,
            shared: Arc::new(SharedLink::new()),
            connect_requested: false,
            port_hint: "auto".into(),
            max_level: MAX_DAC_CODE,
            level: 0,
            min_level: 0,
            method: DriveMethod::Manual,
            mode: Mode::Const,
            frequency_hz: 10.0,
            depth_a: 0.5,
            armed_depth_a: None,
            armed_frequency_hz: None,
            armed_operating_point: None,
            operating_point: 0.5,
            v_null_dac: 0,
            v_peak_dac: 2_048,
            detector_geometry: calibration::DetectorGeometry::RejectedComplement,
            sweep: None,
            fit: None,
            calibration_id: None,
            // Deliberately zero rather than the current generation: a mirror
            // built after a calibration — a plugin reload, say — has to pick
            // the applied lobe up, not assume it is already current.
            applied_lobe_generation: 0,
            calibration_dir: String::new(),
            calibration_status: String::new(),
            press_measure: PressLatch::default(),
            press_apply: PressLatch::default(),
            last_error: None,
            last_reconnect_ms: 0,
        }
    }
}

impl StageAModulationPlugin {
    fn connect(&mut self) {
        if self.link.is_some() {
            return;
        }
        if self.runtime_role != PluginRuntimeRole::LiveWorker || !self.effects_allowed {
            self.last_error = Some("connection deferred: hardware effects are not allowed".into());
            return;
        }
        self.last_error = None;
        *self.shared.state.lock().expect("device state lock") = DeviceState::default();
        *self.shared.pending.lock().expect("pending lock") = None;
        *self.shared.priority.lock().expect("priority lock") = None;
        self.shared.stop.store(false, Ordering::Relaxed);
        self.shared
            .fail_closed_on_stop
            .store(self.lease.is_some(), Ordering::Relaxed);
        self.shared.bump();

        let shared = Arc::clone(&self.shared);
        let spawn = |name: &str, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name.to_owned())
                .spawn(f)
                .expect("spawning the device thread must succeed")
        };
        if self.port_hint == "mock" {
            let (mock, client) = MockService::spawn();
            let join = spawn(
                "stage-a-modulation-device",
                Box::new(move || run_device(client, shared)),
            );
            self.link = Some(DeviceLink {
                shared: Arc::clone(&self.shared),
                join: Some(join),
                _mock: Some(mock),
            });
        } else {
            match open_serial(&self.port_hint) {
                Ok(client) => {
                    let join = spawn(
                        "stage-a-modulation-device",
                        Box::new(move || run_device(client, shared)),
                    );
                    self.link = Some(DeviceLink {
                        shared: Arc::clone(&self.shared),
                        join: Some(join),
                        _mock: None,
                    });
                }
                Err(err) => {
                    self.last_error = Some(err);
                    self.connect_requested = false;
                }
            }
        }
        // Connecting never drives the output (set-and-hold firmware); only
        // changes made while connected are transferred.
    }

    fn disconnect(&mut self) {
        self.link = None; // Drop stops and joins the device thread.
        self.shared.bump();
    }

    /// The lobe the two configured codes name, or why they name none.
    ///
    /// Resolved against the **DAC's** range, not the operator's `max_level`
    /// ceiling: where the crystal nulls and peaks is a fact about the bench, and
    /// a ceiling that cuts the lobe short still leaves the codes below it
    /// perfectly drivable (a MANUAL band inside the ceiling must keep working).
    /// What the ceiling constrains is the codes actually emitted, which
    /// [`Self::dac_band`] checks.
    fn resolved_lobe(&self) -> Result<waveform::ResolvedLobe, String> {
        waveform::LobeInversion::resolve(
            self.v_null_dac as f64,
            self.v_peak_dac as f64,
            MAX_DAC_CODE as f64,
        )
        .map_err(|error| error.to_string())
    }

    fn lobe_inversion(&self) -> Result<waveform::LobeInversion, String> {
        self.resolved_lobe().map(|lobe| lobe.inversion)
    }

    /// Takes on a lobe the live worker applied. Idempotent and cheap: an atomic
    /// load unless there is something new to adopt.
    ///
    /// Only the UI mirror adopts, and only the live worker publishes. That is
    /// the direction the problem actually has — the worker is the instance with
    /// the photodiode and the fit, the mirror is the one whose stale codes were
    /// overwriting it — and it keeps an instance from ever reading back its own
    /// publication.
    fn adopt_applied_lobe(&mut self) -> bool {
        if self.runtime_role != PluginRuntimeRole::UiMirror {
            return false;
        }
        let Some(applied) = applied_lobe_after(self.applied_lobe_generation) else {
            return false;
        };
        self.v_null_dac = applied.v_null_dac;
        self.v_peak_dac = applied.v_peak_dac;
        self.calibration_id = Some(applied.calibration_id);
        self.applied_lobe_generation = applied.generation;
        true
    }

    /// The lobe codes to export, without needing `&mut self`.
    ///
    /// `get_setting` and `settings_schema` are `&self`, and the UI mirror only
    /// reaches [`Self::adopt_applied_lobe`] when the operator next edits
    /// something. Reading through here means a freshly applied lobe shows up in
    /// the panel — and in the snapshot pushed to the worker — on the very next
    /// repaint instead of being overwritten by the stale pair.
    fn effective_lobe(&self) -> (i64, i64) {
        if self.runtime_role != PluginRuntimeRole::UiMirror {
            return (self.v_null_dac, self.v_peak_dac);
        }
        match applied_lobe_after(self.applied_lobe_generation) {
            Some(applied) => (applied.v_null_dac, applied.v_peak_dac),
            None => (self.v_null_dac, self.v_peak_dac),
        }
    }

    /// What the current lobe and DAC ceiling can actually produce for the
    /// current mode. `None` for the manual method (where the band is entered
    /// directly) or when the two endpoint codes name no drivable lobe.
    fn achievable(&self) -> Option<Achievable> {
        if self.method != DriveMethod::Calibrated {
            return None;
        }
        let law = self.mode.peak_law();
        let inversion = self.lobe_inversion().ok()?;
        let u_max = inversion.peak_intensity_ceiling(self.max_level.clamp(0, MAX_DAC_CODE) as f64);
        Some(Achievable {
            u_max,
            peak_u: law.peak(self.operating_point, self.depth_a),
            max_depth_a: law.max_depth_for_mean(self.operating_point, u_max),
            max_mean_u: law.max_mean_for_depth(self.depth_a, u_max),
        })
    }

    /// Brings `a` and `ū` back inside what the bench can actually produce, then
    /// re-arms the drive.
    ///
    /// Nothing here rejects the operator's edit. Refusing a setting and
    /// snapping the control back is what made the mode dropdown feel broken: an
    /// `a` left over from a different lobe made an optical mode simply
    /// unselectable, with no indication of *which* value was in the way.
    /// Clamping moves the drive to the nearest thing the lobe can do, and the
    /// achievable range is on the status line either way.
    ///
    /// Only the knob the operator just touched is clamped — dragging `a` up
    /// means "more depth", so `a` is what gets limited, and the operating point
    /// stays where it was put. A lobe change has no such preference, so it
    /// settles the brightness first and then the depth that fits under it.
    fn reconcile_drive(&mut self, edited: DriveKnob) {
        if self.method == DriveMethod::Calibrated {
            if let Ok(inversion) = self.lobe_inversion() {
                let law = self.mode.peak_law();
                let u_max =
                    inversion.peak_intensity_ceiling(self.max_level.clamp(0, MAX_DAC_CODE) as f64);
                let clamp_mean = |plugin: &mut Self| {
                    let max = law
                        .max_mean_for_depth(plugin.depth_a, u_max)
                        .max(waveform::MEAN_U_MIN);
                    plugin.operating_point =
                        plugin.operating_point.clamp(waveform::MEAN_U_MIN, max);
                };
                let clamp_depth = |plugin: &mut Self| {
                    let max = law
                        .max_depth_for_mean(plugin.operating_point, u_max)
                        .max(waveform::DEPTH_A_MIN);
                    plugin.depth_a = plugin.depth_a.clamp(waveform::DEPTH_A_MIN, max);
                };
                match edited {
                    DriveKnob::Depth => clamp_depth(self),
                    DriveKnob::Mean => clamp_mean(self),
                    DriveKnob::Lobe => {
                        clamp_mean(self);
                        clamp_depth(self);
                    }
                }
            }
        }
        // An edit made while a lease drives the depth is withheld from the
        // board (`send_modulation` is guarded), so it has to land in the parked
        // value or it would be lost when the lease ends.
        if self.armed_depth_a.is_some() {
            self.armed_depth_a = Some(self.depth_a);
        }
        // Re-judge the drive whether or not it can be sent right now. A stale
        // "drive rejected" left over from an earlier combination would
        // otherwise outlive the edit that fixed it — and with no link attached
        // `send_modulation` never reaches the point where it clears one.
        self.last_error = match self.drive_command() {
            Ok(_) => None,
            Err(error) => Some(format!("drive not sent: {error}")),
        };
        self.send_modulation();
    }

    /// Resolves the calibrated UI setting (cycle-mean normalized lobe
    /// coordinate) to the target law's internal pedestal/centre.
    fn periodic_lobe_point(&self) -> f64 {
        let point = if self.mode == Mode::OpticalLogSine {
            waveform::log_sine_geometric_pedestal(self.operating_point, self.depth_a)
        } else {
            self.operating_point
        };
        if matches!(self.mode, Mode::OpticalLogSine | Mode::OpticalLinearSine) {
            // The firmware contract carries this coordinate in milli-units.
            // Validate and preview the value the board will actually rebuild,
            // not a higher-precision local table.
            (point * 1_000.0).round() / 1_000.0
        } else {
            point
        }
    }

    /// Resolves the selected method into the DAC band used by every waveform.
    /// The third value is the constant-mode operating code.
    fn dac_band(&self) -> Result<(i64, i64, i64), String> {
        if self.method == DriveMethod::Manual {
            let hi = self.level.clamp(0, self.max_level);
            return Ok((self.min_level.clamp(0, hi), hi, hi));
        }

        let mean_u = self.operating_point;
        let a = self.depth_a;
        if !mean_u.is_finite() || mean_u <= 0.0 || mean_u > 1.0 {
            return Err("operating point must be in (0, 1]".into());
        }
        let inversion = self.lobe_inversion()?;

        // Every code the inversion emits lies between the two endpoints, and
        // `resolve` has already placed both inside the DAC range — so the floor
        // can no longer be breached and only the operator's ceiling is left to
        // check. One guard, naming the settings that actually exist.
        let under_ceiling = |code: i64, what: &str| -> Result<i64, String> {
            if code > self.max_level {
                return Err(format!(
                    "{what} needs DAC code {code}, above the max limit {}; raise the max limit \
                     or lower u / a",
                    self.max_level
                ));
            }
            Ok(code)
        };

        // A constant hold modulates nothing: no ±a/2 headroom applies, so the
        // full (0, 1] range of u is expressible (u = 1 holds exactly at
        // V_peak). Requiring the modulated band here silently froze the drive
        // at the last accepted code whenever u·e^{a/2} exceeded 1.
        if self.mode == Mode::Const {
            let hold = under_ceiling(
                inversion.dac_for_u(mean_u).round() as i64,
                "the constant hold",
            )?;
            return Ok((hold, hold, hold));
        }

        if !a.is_finite() || a <= 0.0 {
            return Err("optical depth a must be finite and positive".into());
        }
        let target_u = self.periodic_lobe_point();
        let (u_lo, u_hi) = if self.mode == Mode::OpticalLinearSine {
            let m = (0.5 * a).tanh();
            (target_u * (1.0 - m), target_u * (1.0 + m))
        } else {
            (target_u * (-0.5 * a).exp(), target_u * (0.5 * a).exp())
        };
        if u_hi > 1.0 {
            return Err(format!(
                "calibrated optical peak u = {u_hi:.3} exceeds the lobe ceiling; lower a or u"
            ));
        }

        let lo = inversion.dac_for_u(u_lo).round() as i64;
        let hi = under_ceiling(
            inversion.dac_for_u(u_hi).round() as i64,
            "the modulation peak",
        )?;
        let hold = inversion.dac_for_u(target_u).round() as i64;
        Ok((lo, hi, hold))
    }

    fn optical_drive(
        &self,
        target: waveform::OpticalTarget,
    ) -> Result<waveform::OpticalDrive, String> {
        let inversion = self.lobe_inversion()?;
        Ok(match self.method {
            DriveMethod::Manual => {
                let hi = self.level.clamp(0, self.max_level);
                let lo = self.min_level.clamp(0, hi);
                waveform::OpticalDrive::from_dac_band(target, inversion, lo as f64, hi as f64)
            }
            DriveMethod::Calibrated => waveform::OpticalDrive {
                target,
                depth_a: self.depth_a,
                operating_point: self.periodic_lobe_point(),
                inversion,
            },
        })
    }

    fn optical_drive_state(&self) -> Option<OpticalDriveStateV1> {
        if self.method != DriveMethod::Calibrated {
            return None;
        }
        let (target, contract_target) = match self.mode {
            Mode::OpticalLogSine => (waveform::OpticalTarget::LogSine, OpticalTargetV1::LogSine),
            Mode::OpticalLinearSine => (
                waveform::OpticalTarget::LinearSine,
                OpticalTargetV1::LinearSine,
            ),
            _ => return None,
        };
        self.dac_band().ok()?;
        let drive = self.optical_drive(target).ok()?;
        Some(OpticalDriveStateV1 {
            target: contract_target,
            requested_mean_u_milli: (self.operating_point * 1_000.0).round() as u32,
            resolved_mean_u_milli: (match target {
                waveform::OpticalTarget::LogSine => {
                    waveform::log_sine_cycle_mean(drive.operating_point, self.depth_a)
                }
                waveform::OpticalTarget::LinearSine => drive.operating_point,
            } * 1_000.0)
                .round() as u32,
            internal_u_milli: (drive.operating_point * 1_000.0).round() as u32,
            depth_a_milli: (self.depth_a * 1_000.0).round() as u32,
            v_null_dac: u16::try_from(self.v_null_dac).ok()?,
            v_peak_dac: u16::try_from(drive.inversion.v_peak_dac().round() as i64).ok()?,
        })
    }

    /// Publishes the applied calibration independently of the currently armed
    /// mode and point. A protocol runner owns its measurement points and needs
    /// only these calibrated lobe endpoints to construct them.
    fn optical_lobe_state(&self) -> Option<OpticalLobeStateV1> {
        let calibration_id = self.calibration_id.clone()?;
        let inversion = self.resolved_lobe().ok()?.inversion;
        Some(OpticalLobeStateV1 {
            calibration_id,
            v_null_dac: u16::try_from(inversion.dac_for_u(0.0).round() as i64).ok()?,
            v_peak_dac: u16::try_from(inversion.dac_for_u(1.0).round() as i64).ok()?,
        })
    }

    /// Builds the single MOD command carrying the complete current drive
    /// settings (mode, method, band, frequency). Shared by the operator path
    /// (`send_modulation`) and the leased `SetOpticalDepth` service command.
    fn drive_command(&self) -> Result<Command, String> {
        let (lo, hi, hold) = self.dac_band()?;
        let freq_mhz = (self.frequency_hz.clamp(
            DRIVE_FREQUENCY_MIN_MILLIHZ as f64 / 1_000.0,
            DRIVE_FREQUENCY_MAX_MILLIHZ as f64 / 1_000.0,
        ) * 1_000.0)
            .round() as i64;
        Ok(match self.mode {
            Mode::Const => Command::new("MOD")
                .field("wave", "CONST")
                .field("level", hold),
            Mode::Sine | Mode::Square => Command::new("MOD")
                .field("wave", self.mode.wire_wave())
                .field("level", hi)
                .field("min", lo)
                .field("freq_mhz", freq_mhz),
            Mode::OpticalLogSine | Mode::OpticalLinearSine => {
                let target = self
                    .mode
                    .optical_target()
                    .expect("optical modes have a target");
                // Validate the drive locally; the firmware rebuilds the same
                // table from compact parameters because a full table does not
                // fit on one command line.
                self.optical_warp_table(target)
                    .map_err(|error| format!("optical drive: {error}"))?;
                let drive = self.optical_drive(target)?;
                // The firmware rebuilds the table from `v_null` and the *quarter
                // wave*, so the derived distance goes on the wire, not the peak
                // code the operator configures.
                let inversion = drive.inversion;
                Command::new("MOD")
                    .field("wave", "WARP")
                    .field("freq_mhz", freq_mhz)
                    .field("target", optical_target_token(target))
                    .field("a_milli", (drive.depth_a * 1_000.0).round() as i64)
                    .field(
                        "u_k_milli",
                        (drive.operating_point * 1_000.0).round() as i64,
                    )
                    .field("v_null", inversion.v_null_dac.round() as i64)
                    .field("v_pi", inversion.v_pi_dac.round() as i64)
            }
        })
    }

    /// Queues one MOD command carrying the complete current drive settings;
    /// newer changes overwrite queued ones (drag coalescing).
    ///
    /// Silent while another owner holds the DAC: an automation lease, a
    /// calibration sweep. The host re-applies the
    /// *whole* settings snapshot on every sync, and most handlers here call
    /// this unconditionally, so without the guard every sync would re-arm the
    /// operator's drive on top of the code the current owner just commanded —
    /// the sweep would measure the armed waveform instead of its own
    /// staircase.
    fn send_modulation(&mut self) {
        if self.link.is_none() || self.lease.is_some() || self.sweep.is_some() {
            return;
        }
        let command = match self.drive_command() {
            Ok(command) => command,
            Err(error) => {
                self.last_error = Some(format!("drive rejected: {error}"));
                return;
            }
        };
        self.last_error = None;
        *self.shared.pending.lock().expect("pending lock") = Some(PendingOperation {
            commands: vec![command],
            purpose: "MOD",
            meta: None,
        });
    }

    /// Builds the DAC warp table for the current optical drive settings. The
    /// max limit is the hard ceiling; absolute lobe codes cannot be rescaled
    /// without distorting the target, so an over-limit drive is refused.
    fn optical_warp_table(&self, target: waveform::OpticalTarget) -> Result<Vec<u16>, String> {
        let table = self
            .optical_drive(target)?
            .warp_table()
            .map_err(|error| error.to_string())?;
        let peak = table.iter().copied().max().unwrap_or(0);
        if i64::from(peak) > self.max_level {
            return Err(format!(
                "optical peak {peak} exceeds the max limit {}; raise the max limit or lower the \
                 operating band, a or ū",
                self.max_level
            ));
        }
        Ok(table)
    }

    // ---- measured transfer calibration ----

    /// Whether the calibration buttons can be offered, from **mirrored**
    /// settings only.
    ///
    /// `settings_schema()` is rendered by the UI mirror, which never owns the
    /// device link, a lease, a sweep, or a fit — those live on the live worker.
    /// Gating `enabled` on any of them disables the button permanently. So the
    /// affordance uses the one prerequisite the mirror does know (the operator
    /// asked to connect) and the authoritative interlocks stay worker-side in
    /// [`Self::calibration_blocker`], reported through the status entries the
    /// host takes from the worker.
    fn calibration_offered(&self) -> bool {
        self.connect_requested
    }

    /// Why a sweep cannot start right now, if it cannot.
    fn calibration_blocker(&self) -> Option<String> {
        if self.runtime_role != PluginRuntimeRole::LiveWorker || !self.effects_allowed {
            return Some("hardware effects are not allowed on this instance".into());
        }
        if self.link.is_none() {
            return Some("connect the command port first".into());
        }
        if self.lease.is_some() {
            // A1 owns the drive under a lease; two owners stepping the same DAC
            // would interleave silently.
            return Some("the drive is leased by an automation client".into());
        }
        None
    }

    /// Starts a sweep, remembering the drive to restore afterwards.
    fn start_calibration_sweep(&mut self) {
        if let Some(blocker) = self.calibration_blocker() {
            self.calibration_status = format!("sweep refused: {blocker}");
            return;
        }
        let max_code = self.max_level.clamp(0, MAX_DAC_CODE) as u16;
        if max_code < 2 {
            self.calibration_status = "sweep refused: the max limit leaves no range".into();
            return;
        }
        self.sweep = Some(CalibrationSweep {
            steps: calibration::sweep_codes(max_code, SWEEP_POINTS_PER_PASS, true),
            index: 0,
            points: Vec::new(),
            commanded_at_sample: None,
            point_started: Instant::now(),
            // Restoring the drive the operator had armed is part of the
            // measurement contract: a sweep must leave the bench as it found it.
            restore: self.drive_command().ok(),
            max_code,
        });
        self.fit = None;
        self.calibration_status = "sweep starting…".into();
    }

    /// Ends the sweep and hands the DAC back to the armed drive.
    fn finish_calibration_sweep(&mut self, status: String) {
        // Clear the sweep first: it is what silences `send_modulation`.
        let restore = self.sweep.take().and_then(|sweep| sweep.restore);
        self.calibration_status = status;
        if self.link.is_some() {
            // Prefer the *current* settings — drive changes made during the
            // sweep were withheld from the board, and this is where they land.
            // The command captured at the start is the fallback for settings
            // that no longer form a valid drive.
            if self.drive_command().is_ok() {
                self.send_modulation();
            } else if let Some(command) = restore {
                *self.shared.pending.lock().expect("pending lock") = Some(PendingOperation {
                    commands: vec![command],
                    purpose: "MOD",
                    meta: None,
                });
            }
        }
        self.shared.bump();
    }

    /// Queues one settled `CONST` code, bypassing the drive builder: a sweep
    /// deliberately visits codes the armed drive would refuse.
    fn command_sweep_code(&mut self, code: u16) {
        *self.shared.pending.lock().expect("pending lock") = Some(PendingOperation {
            commands: vec![Command::new("MOD")
                .field("wave", "CONST")
                .field("level", i64::from(code))],
            purpose: "MOD",
            meta: None,
        });
    }

    /// Settle window in samples for a stream running at `sample_rate_hz`.
    /// [`SETTLE_SECONDS`] is the physical quantity; the rate only converts it.
    fn settle_samples(sample_rate_hz: Option<u32>) -> u64 {
        match sample_rate_hz {
            Some(rate) if rate > 0 => (f64::from(rate) * SETTLE_SECONDS).round() as u64,
            _ => SETTLE_SAMPLES,
        }
    }

    /// One tick of the sweep. `level` is the newest photodiode reading, if any,
    /// and `sample_rate_hz` the stream it was measured on.
    fn drive_calibration(&mut self, level: Option<PhotodiodeLevelV1>, sample_rate_hz: Option<u32>) {
        if self.sweep.is_none() {
            return;
        }
        if let Some(blocker) = self.calibration_blocker() {
            self.finish_calibration_sweep(format!("sweep aborted: {blocker}"));
            return;
        }
        let Some(level) = level else {
            if self
                .sweep
                .as_ref()
                .is_some_and(|sweep| sweep.point_started.elapsed() > POINT_TIMEOUT)
            {
                self.finish_calibration_sweep(
                    "sweep aborted: no photodiode level (connect the photodiode plugin)".into(),
                );
            }
            return;
        };

        let Some((code, direction)) = self.sweep.as_ref().and_then(CalibrationSweep::current)
        else {
            self.complete_calibration_sweep();
            return;
        };

        // Command the point once, then wait for a window that began after it.
        let commanded_at = match self.sweep.as_ref().expect("sweep").commanded_at_sample {
            Some(sample) => sample,
            None => {
                self.command_sweep_code(code);
                let sweep = self.sweep.as_mut().expect("sweep");
                sweep.commanded_at_sample = Some(level.end_sample_index);
                sweep.point_started = Instant::now();
                self.calibration_status = format!(
                    "sweeping {}/{}…",
                    self.sweep.as_ref().expect("sweep").index + 1,
                    self.sweep.as_ref().expect("sweep").total()
                );
                return;
            }
        };

        let window_start = level.end_sample_index.saturating_sub(level.sample_count);
        if window_start < commanded_at + Self::settle_samples(sample_rate_hz) {
            if self.sweep.as_ref().expect("sweep").point_started.elapsed() > POINT_TIMEOUT {
                self.finish_calibration_sweep(
                    "sweep aborted: the photodiode stream stalled".into(),
                );
            }
            return;
        }

        let sweep = self.sweep.as_mut().expect("sweep");
        sweep.points.push(calibration::SweepPoint {
            code,
            direction,
            volts: level.mean_volts,
            peak_to_peak_volts: level.peak_to_peak_volts,
            clipped: level.clipped,
        });
        sweep.index += 1;
        sweep.commanded_at_sample = None;
        if sweep.index >= sweep.steps.len() {
            self.complete_calibration_sweep();
        }
    }

    /// Fits the collected points and leaves the result awaiting an explicit
    /// apply — a bad fit silently retargeting the drive is the dangerous case.
    fn complete_calibration_sweep(&mut self) {
        let Some(sweep) = self.sweep.as_ref() else {
            return;
        };
        let points = sweep.points.clone();
        let max_code = f64::from(sweep.max_code);
        match calibration::fit_transfer(&points, max_code, self.detector_geometry) {
            Ok(fit) => {
                let status = format!(
                    "V_null {:.0}  V_peak {:.0}  span {:.3} V  residual {:.1}%{}{}  ({:.1} lobes)",
                    fit.v_null_dac,
                    fit.v_pi_dac,
                    fit.span_volts.abs(),
                    fit.quality * 100.0,
                    fit.hysteresis
                        .map(|value| format!("  hysteresis {:.1}%", value * 100.0))
                        .unwrap_or_default(),
                    if fit.rejected_points > 0 {
                        format!("  {} dropped", fit.rejected_points)
                    } else {
                        String::new()
                    },
                    fit.lobe_coverage,
                );
                self.fit = Some(fit);
                self.finish_calibration_sweep(status);
            }
            Err(error) => {
                self.fit = None;
                self.finish_calibration_sweep(format!("fit failed: {error}"));
            }
        }
    }

    /// Things worth the operator's attention before trusting a fit. Compare
    /// them against the transfer-curve plot.
    ///
    /// Deliberately warnings and not blocks. The only condition that makes a
    /// fit meaningless — no full lobe inside the commandable range — is already
    /// refused by [`calibration::fit_transfer`] itself, so there is no second
    /// fit to reject here. Everything below is a judgement the operator makes
    /// against the plot: a single stray sample can push the residual past any
    /// threshold while `Vπ` stays accurate to a few codes, so blocking on it
    /// would withhold a good calibration for a bad reason.
    fn fit_warnings(&self) -> Vec<String> {
        let Some(fit) = self.fit.as_ref() else {
            return Vec::new();
        };
        let mut warnings = Vec::new();
        if fit.quality > WARN_QUALITY {
            warnings.push(format!(
                "residual is {:.1}% of the detector span — check the fit against the points \
                 in the transfer-curve plot before trusting the fit",
                fit.quality * 100.0
            ));
        }
        if fit.rejected_points > 0 {
            warnings.push(format!(
                "{} of {} points were wild and left out of the fit",
                fit.rejected_points,
                fit.points.len()
            ));
        }
        // Two independently noisy passes over the same curve already differ by
        // `1.128 σ` on average, so a raw 5 % cut reports point scatter as cell
        // drift on any bench whose points are not far quieter than that. Both
        // tests have to pass: the disagreement must be large enough to matter
        // *and* systematic rather than scatter.
        let hysteresis = fit
            .hysteresis
            .filter(|value| *value > WARN_HYSTERESIS && fit.hysteresis_is_systematic());
        if let Some(hysteresis) = hysteresis {
            warnings.push(format!(
                "up and down passes differ by {:.1}% of the span, past the {:.1}% the point \
                 scatter alone explains — the cell is drifting or the settle time is too short",
                hysteresis * 100.0,
                fit.hysteresis_noise_floor() * 100.0
            ));
        }
        let clipped = fit.points.iter().filter(|point| point.clipped).count();
        if clipped > 0 {
            // Naming what it actually costs. Clipping truncates the reported
            // detector extrema (and with them the I_tot lower bound); V_null and
            // Vπ come from the *shape*, which a truncated extremum barely moves.
            // The old text advised attenuation, which is backwards for the
            // reject-port detector — there it is the dark end that reaches the
            // bottom rail, and the fix is more gain, not less light.
            warnings.push(format!(
                "{clipped} of {} points reached an end of the detector's range — V_null and V_peak \
                 are unaffected, but the reported detector extrema (and the I_tot lower bound) \
                 are truncated there; change the detector gain if you need them",
                fit.points.len()
            ));
        }
        warnings
    }

    /// Applies the reviewed fit to the `V_null`/`V_peak` settings and archives
    /// the fitted internal `Vπ` span.
    fn apply_calibration_fit(&mut self) {
        let Some(fit) = self.fit.clone() else {
            self.calibration_status = "nothing to apply: measure a transfer curve first".into();
            return;
        };
        let previous = (self.v_null_dac, self.v_peak_dac);
        self.v_null_dac = fit.v_null_dac.round().clamp(0.0, MAX_DAC_CODE as f64) as i64;
        // The fit reports the half-wave span; the settings hold the peak code
        // the operator can see on the plot.
        self.v_peak_dac = fit.v_peak_dac().round().clamp(0.0, MAX_DAC_CODE as f64) as i64;
        // The lobe itself has to be resolvable — two codes that name no
        // monotonic branch are not a calibration. Whether the *drive* fits
        // under the current a/ū is not this button's business: those clamp
        // themselves to the new lobe rather than blocking the measurement the
        // operator just took.
        if let Err(error) = self.lobe_inversion() {
            self.v_null_dac = previous.0;
            self.v_peak_dac = previous.1;
            self.calibration_status = format!("not applied: {error}");
            return;
        }
        let calibration_id = format!("pockels-{}", timestamp_slug());
        let archived = match self.archive_calibration(&calibration_id, &fit) {
            Ok(Some(path)) => format!(", archived to {path}"),
            Ok(None) => ", not archived (no calibration folder set)".into(),
            Err(error) => format!(", archive failed: {error}"),
        };
        self.calibration_id = Some(calibration_id.clone());
        // Hand the lobe to the UI mirror before anything else can overwrite it
        // — see [`APPLIED_LOBE`]. Only the live worker ever gets this far: it
        // is the instance the sweep and the fit live on.
        if self.runtime_role == PluginRuntimeRole::LiveWorker {
            self.applied_lobe_generation =
                publish_applied_lobe(self.v_null_dac, self.v_peak_dac, calibration_id);
        }
        self.calibration_status = format!(
            "applied V_null {} / V_peak {} (span {} codes){archived}",
            self.v_null_dac,
            self.v_peak_dac,
            self.v_peak_dac - self.v_null_dac
        );
        self.reconcile_drive(DriveKnob::Lobe);
        self.shared.bump();
    }

    /// Writes the calibration record. A calibration is named and never
    /// silently overwritten (knowledge base §4.6).
    fn archive_calibration(
        &self,
        calibration_id: &str,
        fit: &calibration::TransferFit,
    ) -> Result<Option<String>, String> {
        if self.calibration_dir.trim().is_empty() {
            return Ok(None);
        }
        let directory = std::path::Path::new(self.calibration_dir.trim());
        std::fs::create_dir_all(directory)
            .map_err(|error| format!("creating {}: {error}", directory.display()))?;
        let path = directory.join(format!("{calibration_id}.json"));
        let record = json!({
            "calibration_id": calibration_id,
            "port": self.port_hint,
            "max_level": self.max_level,
            "detector_geometry": fit.geometry.name(),
            "v_null_dac": fit.v_null_dac,
            "v_peak_dac": fit.v_peak_dac(),
            "half_wave_span_dac": fit.v_pi_dac,
            "detector_volts_at_null": fit.detector_volts_at_null(),
            "detector_volts_at_peak": fit.detector_volts_at_peak(),
            "span_volts": fit.span_volts,
            "rms_residual_volts": fit.rms_residual_volts,
            "quality": fit.quality,
            "hysteresis": fit.hysteresis,
            "lobe_coverage": fit.lobe_coverage,
            "rejected_points": fit.rejected_points,
            "anchor_note": "detector_volts_at_null is a lower bound on the total-power \
                            anchor I_tot, not the anchor: on the reject port the residual \
                            transmitted floor is not separable from it",
            "points": fit
                .points
                .iter()
                .map(|point| json!({
                    "code": point.code,
                    "direction": point.direction.label(),
                    "volts": point.volts,
                    "peak_to_peak_volts": point.peak_to_peak_volts,
                    "clipped": point.clipped,
                }))
                .collect::<Vec<Value>>(),
        });
        let encoded = serde_json::to_vec_pretty(&record)
            .map_err(|error| format!("encoding the calibration record: {error}"))?;
        std::fs::write(&path, encoded)
            .map_err(|error| format!("writing {}: {error}", path.display()))?;
        Ok(Some(path.display().to_string()))
    }

    fn lease_snapshot(&self) -> Option<LeaseSnapshotV1> {
        self.lease.as_ref().map(|lease| LeaseSnapshotV1 {
            lease_id: lease.lease_id.clone(),
            holder: lease.holder.clone(),
            expires_at_unix_ms: lease.expires_at_unix_ms,
            run_id: lease.run_id.clone(),
        })
    }

    fn require_lease(&self, request: &ModulationRequestV1) -> Result<(), ServiceErrorV1> {
        let lease = self.lease.as_ref().ok_or_else(|| {
            service_error(
                ServiceErrorCodeV1::LeaseRequired,
                "the modulation owner requires an active automation lease",
                false,
            )
        })?;
        if now_unix_ms() > lease.expires_at_unix_ms {
            return Err(service_error(
                ServiceErrorCodeV1::LeaseExpired,
                "the modulation automation lease expired",
                false,
            ));
        }
        if request.lease_id.as_ref() != Some(&lease.lease_id) || request.requester != lease.holder {
            return Err(service_error(
                ServiceErrorCodeV1::LeaseMismatch,
                "request lease/holder does not match the active lease",
                false,
            ));
        }
        if request.run_id != lease.run_id {
            return Err(service_error(
                ServiceErrorCodeV1::LeaseMismatch,
                "request run does not match the leased run",
                false,
            ));
        }
        Ok(())
    }

    fn requested_revision(
        &self,
        request: &ModulationRequestV1,
    ) -> Result<SemanticRevision, ServiceErrorV1> {
        let revision = request.requested_revision.ok_or_else(|| {
            service_error(
                ServiceErrorCodeV1::InvalidCommand,
                "state-changing modulation commands require requested_revision",
                false,
            )
        })?;
        let current = self
            .shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.requested.as_ref().map(|target| target.revision));
        if current.is_some_and(|current| revision <= current) {
            return Err(service_error(
                ServiceErrorCodeV1::StaleRequest,
                "requested_revision must be newer than the current requested state",
                false,
            ));
        }
        Ok(revision)
    }

    fn base_target(&self, revision: SemanticRevision) -> ModulationTargetV1 {
        let state = self.shared.state.lock().expect("device state lock");
        let mut target = state
            .requested
            .clone()
            .or_else(|| state.acknowledged.clone())
            .unwrap_or(ModulationTargetV1 {
                revision,
                waveform: None,
                a2_configuration: None,
                acquisition_running: false,
                board_dac_code: None,
                firmware_configuration_revision: None,
            });
        target.revision = revision;
        target.board_dac_code = None;
        target.firmware_configuration_revision = None;
        target
    }

    /// The controller's own acquisition settings, as it last reported them.
    ///
    /// A mode change restates them, so a controller that has not reported yet
    /// is refused rather than reconfigured from a guess.
    fn controller_acquisition(&self) -> Result<ControllerAcquisition, ServiceErrorV1> {
        let missing = || {
            service_error(
                ServiceErrorCodeV1::InvalidCommand,
                "the controller has not reported its acquisition settings yet — connect and let \
                 it answer STATUS first",
                true,
            )
        };
        let state = self.shared.state.lock().expect("device state lock");
        Ok(ControllerAcquisition {
            rate_hz: state.controller_rate_hz.ok_or_else(missing)?,
            block_samples: state.controller_block_samples.ok_or_else(missing)?,
            emit_raw: state.controller_emit_raw.unwrap_or(true),
            emit_summary: state.controller_emit_summary.unwrap_or(true),
        })
    }

    fn queue_service_operation(
        &mut self,
        request: &ModulationRequestV1,
        target: ModulationTargetV1,
        commands: Vec<Command>,
        purpose: &'static str,
        priority: bool,
    ) -> Result<ModulationResponseV1, ServiceErrorV1> {
        if self.link.is_none() {
            return Err(service_error(
                ServiceErrorCodeV1::NotConnected,
                "the Teensy command port is not connected",
                true,
            ));
        }
        let mut automation = self.shared.automation.lock().expect("automation lock");
        if !priority && automation.len() >= 64 {
            return Err(service_error(
                ServiceErrorCodeV1::InvalidCommand,
                "controller command queue is full",
                true,
            ));
        }
        let revision = target.revision;
        let meta = OperationMeta {
            request_id: request.request_id,
            requester: request.requester.clone(),
            run_id: request.run_id.clone(),
            requested_revision: revision,
            target: target.clone(),
            owner_instance: self.owner_instance.clone(),
        };
        {
            let mut state = self.shared.state.lock().expect("device state lock");
            state.requested = Some(target);
        }
        let operation = PendingOperation {
            commands,
            purpose,
            meta: Some(meta),
        };
        if priority {
            automation.clear();
            *self.shared.pending.lock().expect("pending lock") = None;
            *self.shared.priority.lock().expect("priority lock") = Some(operation);
        } else {
            automation.push_back(operation);
        }
        drop(automation);
        self.shared.bump();
        Ok(ModulationResponseV1 {
            marker_diagnostics: None,
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: request.request_id,
                owner_instance: self.owner_instance.clone(),
                run_id: request.run_id.clone(),
                requested_revision: Some(revision),
                acknowledged_revision: self
                    .shared
                    .state
                    .lock()
                    .ok()
                    .and_then(|state| state.acknowledged.as_ref().map(|value| value.revision)),
                outcome: RequestOutcomeV1::InProgress,
                completed_at_unix_ms: None,
                error: None,
            },
            controller_state: self
                .shared
                .state
                .lock()
                .map(|state| state.controller_state)
                .unwrap_or(ControllerStateV1::Unknown),
            acknowledged_target: None,
        })
    }

    fn handle_modulation_command(
        &mut self,
        request: &ModulationRequestV1,
    ) -> Result<ModulationResponseV1, ServiceErrorV1> {
        match &request.command {
            ModulationCommandV1::Connect => {
                if self.lease.is_some() {
                    return Err(service_error(
                        ServiceErrorCodeV1::LeaseBusy,
                        "connection cannot be changed while leased",
                        false,
                    ));
                }
                self.connect_requested = true;
                self.connect();
                self.immediate_response(request, RequestOutcomeV1::Applied, None)
            }
            ModulationCommandV1::Disconnect { safe_off, reason } => {
                if self.lease.is_some() {
                    return Err(service_error(
                        ServiceErrorCodeV1::LeaseBusy,
                        "use ReleaseLease while the owner is leased",
                        false,
                    ));
                }
                if *safe_off && self.link.is_some() {
                    self.shared
                        .fail_closed_on_stop
                        .store(true, Ordering::Relaxed);
                }
                self.connect_requested = false;
                self.disconnect();
                self.last_error = Some(format!("disconnected by service: {reason}"));
                self.immediate_response(request, RequestOutcomeV1::Applied, None)
            }
            ModulationCommandV1::AcquireLease { ttl_ms } => {
                let lease_id = request.lease_id.clone().ok_or_else(|| {
                    service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "AcquireLease requires lease_id",
                        false,
                    )
                })?;
                if let Some(active) = &self.lease {
                    if active.lease_id != lease_id || active.holder != request.requester {
                        return Err(service_error(
                            ServiceErrorCodeV1::LeaseBusy,
                            "the modulation owner is already leased",
                            true,
                        ));
                    }
                }
                self.lease = Some(ControlLease {
                    lease_id,
                    holder: request.requester.clone(),
                    run_id: request.run_id.clone(),
                    expires_at_unix_ms: lease_deadline(*ttl_ms),
                });
                self.shared
                    .fail_closed_on_stop
                    .store(true, Ordering::Relaxed);
                self.immediate_response(request, RequestOutcomeV1::Applied, None)
            }
            ModulationCommandV1::RenewLease { ttl_ms } => {
                self.require_lease(request)?;
                if let Some(lease) = &mut self.lease {
                    lease.expires_at_unix_ms = lease_deadline(*ttl_ms);
                }
                self.immediate_response(request, RequestOutcomeV1::Applied, None)
            }
            ModulationCommandV1::ReleaseLease { safe_off, reason } => {
                self.require_lease(request)?;
                if *safe_off {
                    let revision = request.requested_revision.unwrap_or_else(|| {
                        let current = self
                            .shared
                            .state
                            .lock()
                            .ok()
                            .and_then(|state| {
                                state.requested.as_ref().map(|value| value.revision.0)
                            })
                            .unwrap_or(0);
                        SemanticRevision(current.saturating_add(1))
                    });
                    let mut target = self.base_target(revision);
                    target.waveform = Some(WaveformV1::Off);
                    target.acquisition_running = false;
                    let response = self.queue_service_operation(
                        request,
                        target,
                        vec![
                            Command::new("STOP").field("reason", reason.replace(' ', "_")),
                            Command::new("MOD").field("wave", "OFF"),
                        ],
                        "SAFE_OFF",
                        true,
                    )?;
                    self.deferred_release_request = Some(request.request_id);
                    self.deferred_release_ack_published = false;
                    return Ok(response);
                }
                self.shared
                    .fail_closed_on_stop
                    .store(false, Ordering::Relaxed);
                self.end_lease();
                self.deferred_release_request = None;
                self.immediate_response(request, RequestOutcomeV1::Applied, None)
            }
            ModulationCommandV1::SafeOff { reason } => {
                let revision = request.requested_revision.unwrap_or_else(|| {
                    let current = self
                        .shared
                        .state
                        .lock()
                        .ok()
                        .and_then(|state| state.requested.as_ref().map(|value| value.revision.0))
                        .unwrap_or(0);
                    SemanticRevision(current.saturating_add(1))
                });
                let mut target = self.base_target(revision);
                target.waveform = Some(WaveformV1::Off);
                target.acquisition_running = false;
                self.shared
                    .fail_closed_on_stop
                    .store(true, Ordering::Relaxed);
                self.queue_service_operation(
                    request,
                    target,
                    vec![
                        Command::new("STOP").field("reason", reason.replace(' ', "_")),
                        Command::new("MOD").field("wave", "OFF"),
                    ],
                    "SAFE_OFF",
                    true,
                )
            }
            ModulationCommandV1::SetWaveform { waveform } => {
                self.require_lease(request)?;
                let revision = self.requested_revision(request)?;
                let mut target = self.base_target(revision);
                target.waveform = Some(waveform.clone());
                self.queue_service_operation(
                    request,
                    target,
                    vec![waveform_command(waveform)],
                    "SET_WAVEFORM",
                    false,
                )
            }
            ModulationCommandV1::SetOpticalDepth { depth_a_milli } => {
                self.require_lease(request)?;
                if self.link.is_none() {
                    return Err(service_error(
                        ServiceErrorCodeV1::NotConnected,
                        "the modulation owner is not connected to the device",
                        false,
                    ));
                }
                let depth_a = f64::from(*depth_a_milli) / 1_000.0;
                if !(0.01..=6.0).contains(&depth_a) {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        format!("optical depth a={depth_a:.3} outside the supported 0.01..=6.0"),
                        false,
                    ));
                }
                // A1's depth command has one scientific meaning: a calibrated
                // log-intensity sine. Reject every other mode instead of
                // silently sweeping a DAC or linear-intensity waveform.
                if self.method != DriveMethod::Calibrated
                    || self.mode != Mode::OpticalLogSine
                    || self.calibration_id.is_none()
                {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "apply a calibration and arm OPTICAL_LOG_SINE in the modulation plugin \
                         before sweeping optical depth a",
                        false,
                    ));
                }
                let previous = self.depth_a;
                self.depth_a = depth_a;
                let command = match self.drive_command() {
                    Ok(command) => command,
                    Err(error) => {
                        self.depth_a = previous;
                        return Err(service_error(
                            ServiceErrorCodeV1::DeviceRejected,
                            format!("optical depth a={depth_a:.3} rejected: {error}"),
                            false,
                        ));
                    }
                };
                // Remember what the operator had armed before the first
                // sweep point, so `end_lease` can hand it back. Only the
                // first one: later points must not overwrite the original.
                self.armed_depth_a.get_or_insert(previous);
                let revision = self
                    .shared
                    .state
                    .lock()
                    .expect("device state lock")
                    .requested
                    .as_ref()
                    .map_or(SemanticRevision(1), |target| {
                        SemanticRevision(target.revision.0 + 1)
                    });
                let target = self.base_target(revision);
                self.queue_service_operation(request, target, vec![command], "MOD", false)
            }
            ModulationCommandV1::SetDriveFrequency { frequency_millihz } => {
                self.require_lease(request)?;
                if self.link.is_none() {
                    return Err(service_error(
                        ServiceErrorCodeV1::NotConnected,
                        "the modulation owner is not connected to the device",
                        false,
                    ));
                }
                let frequency_hz = *frequency_millihz as f64 / 1_000.0;
                // The same band `drive_command` clamps to; refuse rather than
                // silently record a different frequency than the one asked for.
                if !stage_a_plugin_contract::drive_frequency_supported(*frequency_millihz) {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        format!(
                            "frequency {frequency_hz:.3} Hz outside the supported {:.2}..={} Hz",
                            DRIVE_FREQUENCY_MIN_MILLIHZ as f64 / 1_000.0,
                            DRIVE_FREQUENCY_MAX_MILLIHZ / 1_000
                        ),
                        false,
                    ));
                }
                // A constant hold has no frequency, and the manual DAC band is
                // not the calibrated drive this path retargets.
                if self.method == DriveMethod::Manual || self.mode == Mode::Const {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "arm a calibrated periodic/optical drive in the modulation plugin \
                         before sweeping the frequency",
                        false,
                    ));
                }
                let previous = self.frequency_hz;
                self.frequency_hz = frequency_hz;
                let command = match self.drive_command() {
                    Ok(command) => command,
                    Err(error) => {
                        self.frequency_hz = previous;
                        return Err(service_error(
                            ServiceErrorCodeV1::DeviceRejected,
                            format!("frequency {frequency_hz:.3} Hz rejected: {error}"),
                            false,
                        ));
                    }
                };
                // As for the depth: park the operator's own frequency on the
                // first retarget only, so `end_lease` hands back what they
                // armed rather than the sweep's last point.
                self.armed_frequency_hz.get_or_insert(previous);
                let revision = self
                    .shared
                    .state
                    .lock()
                    .expect("device state lock")
                    .requested
                    .as_ref()
                    .map_or(SemanticRevision(1), |target| {
                        SemanticRevision(target.revision.0 + 1)
                    });
                let target = self.base_target(revision);
                self.queue_service_operation(request, target, vec![command], "MOD", false)
            }
            ModulationCommandV1::SetOperatingPoint { mean_u_milli } => {
                self.require_lease(request)?;
                if self.link.is_none() {
                    return Err(service_error(
                        ServiceErrorCodeV1::NotConnected,
                        "the modulation owner is not connected to the device",
                        false,
                    ));
                }
                let mean_u = f64::from(*mean_u_milli) / 1_000.0;
                if !(waveform::MEAN_U_MIN..=1.0).contains(&mean_u) {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        format!(
                            "operating point ū={mean_u:.3} outside the supported {:.2}..=1.0",
                            waveform::MEAN_U_MIN
                        ),
                        false,
                    ));
                }
                // `ū` only means anything against a measured lobe: on the
                // manual method the band is the operator's two DAC codes and
                // there is no normalized coordinate to retarget.
                if self.method != DriveMethod::Calibrated {
                    return Err(service_error(
                        ServiceErrorCodeV1::InvalidCommand,
                        "arm the calibrated drive method in the modulation plugin before \
                         sweeping the operating point",
                        false,
                    ));
                }
                let previous = self.operating_point;
                self.operating_point = mean_u;
                let command = match self.drive_command() {
                    Ok(command) => command,
                    Err(error) => {
                        // Unlike an interactive edit this is *not* clamped: a
                        // protocol asked for a specific brightness, and quietly
                        // recording a different one would put the wrong `ū` in
                        // every sidecar of that block.
                        self.operating_point = previous;
                        return Err(service_error(
                            ServiceErrorCodeV1::DeviceRejected,
                            format!("operating point ū={mean_u:.3} rejected: {error}"),
                            false,
                        ));
                    }
                };
                // As for depth and frequency: park the operator's own value on
                // the first retarget only, so `end_lease` hands back what they
                // armed rather than the sweep's last point.
                self.armed_operating_point.get_or_insert(previous);
                let revision = self
                    .shared
                    .state
                    .lock()
                    .expect("device state lock")
                    .requested
                    .as_ref()
                    .map_or(SemanticRevision(1), |target| {
                        SemanticRevision(target.revision.0 + 1)
                    });
                let target = self.base_target(revision);
                self.queue_service_operation(request, target, vec![command], "MOD", false)
            }
            ModulationCommandV1::PrepareA1 => {
                self.require_lease(request)?;
                let settings = self.controller_acquisition()?;
                let revision = self.requested_revision(request)?;
                let mut target = self.base_target(revision);
                target.a2_configuration = None;
                target.acquisition_running = true;
                self.queue_service_operation(
                    request,
                    target,
                    a1_prepare_commands(&settings),
                    "PREPARE_A1",
                    false,
                )
            }
            ModulationCommandV1::PrepareA2 { configuration } => {
                self.require_lease(request)?;
                validate_a2_configuration(configuration)?;
                let revision = self.requested_revision(request)?;
                let mut target = self.base_target(revision);
                target.a2_configuration = Some(configuration.clone());
                target.acquisition_running = false;
                self.queue_service_operation(
                    request,
                    target,
                    a2_prepare_commands(configuration)?,
                    "PREPARE_A2",
                    false,
                )
            }
            ModulationCommandV1::StartAcquisition => {
                self.require_lease(request)?;
                let revision = self.requested_revision(request)?;
                let mut target = self.base_target(revision);
                target.acquisition_running = true;
                self.queue_service_operation(
                    request,
                    target,
                    vec![Command::new("START")],
                    "START",
                    false,
                )
            }
            ModulationCommandV1::StopAcquisition { reason } => {
                self.require_lease(request)?;
                let revision = self.requested_revision(request)?;
                let mut target = self.base_target(revision);
                target.acquisition_running = false;
                self.queue_service_operation(
                    request,
                    target,
                    vec![Command::new("STOP").field("reason", reason.replace(' ', "_"))],
                    "STOP",
                    false,
                )
            }
        }
    }

    fn immediate_response(
        &mut self,
        request: &ModulationRequestV1,
        outcome: RequestOutcomeV1,
        error: Option<ServiceErrorV1>,
    ) -> Result<ModulationResponseV1, ServiceErrorV1> {
        let state = self.shared.state.lock().expect("device state lock");
        let response = ModulationResponseV1 {
            marker_diagnostics: state.marker_diagnostics,
            common: ResponseCommonV1 {
                contract_version: CONTRACT_VERSION_V1,
                request_id: request.request_id,
                owner_instance: self.owner_instance.clone(),
                run_id: request.run_id.clone(),
                requested_revision: request.requested_revision,
                acknowledged_revision: state.acknowledged.as_ref().map(|value| value.revision),
                outcome,
                completed_at_unix_ms: Some(now_unix_ms()),
                error,
            },
            controller_state: state.controller_state,
            acknowledged_target: state.acknowledged.clone(),
        };
        drop(state);
        self.shared
            .state
            .lock()
            .expect("device state lock")
            .last_response = Some(response.clone());
        self.shared.bump();
        Ok(response)
    }

    fn control_state(&self) -> ModulationStateV1 {
        let state = self.shared.state.lock().expect("device state lock");
        let connection = if state.connected {
            ConnectionStateV1::Connected {
                port_label: self.port_hint.clone(),
                firmware_version: Some(state.firmware.clone()),
            }
        } else if let Some(error) = state.last_error.clone().or_else(|| self.last_error.clone()) {
            ConnectionStateV1::Faulted { message: error }
        } else if self.connect_requested {
            ConnectionStateV1::Connecting
        } else {
            ConnectionStateV1::Disconnected
        };
        let synchronization = match (
            self.lease.as_ref().and_then(|lease| lease.run_id.clone()),
            state.requested.as_ref(),
            state.acknowledged.as_ref(),
        ) {
            (Some(run_id), Some(requested), Some(acknowledged))
                if requested.revision == acknowledged.revision =>
            {
                SynchronizationV1::Synced {
                    run_id,
                    acknowledged_revision: acknowledged.revision,
                    stream_epoch: None,
                }
            }
            (None, _, _) => SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::NoLease,
                detail: None,
            },
            _ => SynchronizationV1::Unsynced {
                reason: UnsyncedReasonV1::RequestedRevisionNotAcknowledged,
                detail: None,
            },
        };
        ModulationStateV1 {
            contract_version: CONTRACT_VERSION_V1,
            owner_instance: self.owner_instance.clone(),
            service_revision: self.shared.generation.load(Ordering::Relaxed),
            connection,
            capabilities: state.capabilities.clone(),
            lease: self.lease_snapshot(),
            controller_state: state.controller_state,
            controller_mode: state.controller_mode.clone(),
            active_run_id: self.lease.as_ref().and_then(|lease| lease.run_id.clone()),
            requested: state.requested.clone(),
            // Service-path acknowledgements win; otherwise expose the
            // board-echoed operator-armed drive (revision 0) so consumers
            // like A1 can read the modulation frequency without a lease ever
            // having existed.
            acknowledged: state
                .acknowledged
                .clone()
                .or_else(|| state.board_echo_target()),
            synchronization,
            last_response: state.last_response.clone(),
            freshness: FreshnessV1 {
                observed_at_unix_ms: if state.last_device_update_unix_ms == 0 {
                    now_unix_ms()
                } else {
                    state.last_device_update_unix_ms
                },
                valid_for_ms: 1_500,
            },
            calibration_id: self.calibration_id.clone(),
            optical_lobe: self.optical_lobe_state(),
            optical_drive: self.optical_drive_state(),
        }
    }

    /// Ends the current lease and gives the operator their armed drive back.
    ///
    /// A leased `SetOpticalDepth` (A1's amplitude sweep) writes straight into
    /// `depth_a`. Without this the modulation UI kept showing — and the board
    /// kept holding — the last sweep point's depth after the sweep finished,
    /// rather than what the operator had armed. The calibration sweep already
    /// restores through `Sweep::restore`; this is the leased equivalent.
    fn end_lease(&mut self) {
        self.shared
            .automation
            .lock()
            .expect("automation lock")
            .clear();
        self.lease = None;
        let depth = self.armed_depth_a.take();
        let frequency = self.armed_frequency_hz.take();
        let operating_point = self.armed_operating_point.take();
        if let Some(depth) = depth {
            self.depth_a = depth;
        }
        if let Some(frequency) = frequency {
            self.frequency_hz = frequency;
        }
        if let Some(operating_point) = operating_point {
            self.operating_point = operating_point;
        }
        if (depth.is_some() || frequency.is_some() || operating_point.is_some())
            && !self.shared.fail_closed_on_stop.load(Ordering::Relaxed)
        {
            // Re-arm the board only if nobody else now owns the DAC;
            // `send_modulation` is itself guarded.
            self.send_modulation();
        }
    }

    fn expire_lease_if_needed(&mut self) {
        let expired = self
            .lease
            .as_ref()
            .is_some_and(|lease| now_unix_ms() > lease.expires_at_unix_ms);
        if !expired {
            return;
        }
        self.shared
            .fail_closed_on_stop
            .store(true, Ordering::Relaxed);
        self.shared
            .automation
            .lock()
            .expect("automation lock")
            .clear();
        *self.shared.pending.lock().expect("pending lock") = None;
        *self.shared.priority.lock().expect("priority lock") = Some(PendingOperation {
            commands: vec![
                Command::new("STOP").field("reason", "lease_expired"),
                Command::new("MOD").field("wave", "OFF"),
            ],
            purpose: "LEASE_EXPIRED_SAFE_OFF",
            meta: None,
        });
        self.end_lease();
        self.last_error = Some("automation lease expired; queued STOP + output off".into());
        self.shared.bump();
    }

    fn advance_deferred_release(&mut self) {
        let Some(request_id) = self.deferred_release_request else {
            return;
        };
        let terminal_applied = self
            .shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.last_response.clone())
            .is_some_and(|response| {
                response.common.request_id == request_id
                    && response.common.outcome == RequestOutcomeV1::Applied
            });
        if !terminal_applied {
            return;
        }
        if self.deferred_release_ack_published {
            self.end_lease();
            self.deferred_release_request = None;
            self.deferred_release_ack_published = false;
            self.shared
                .fail_closed_on_stop
                .store(false, Ordering::Relaxed);
            self.shared.bump();
        } else {
            // Preserve the lease for one complete snapshot publication so
            // the orchestrator can consume the terminal ACK before the owner
            // advertises the release.
            self.deferred_release_ack_published = true;
        }
    }

    fn apply_execution_context(&mut self, execution: &augur_plugin_api::ExecutionContext) {
        let allowed = self.runtime_role == PluginRuntimeRole::LiveWorker
            && execution.hardware_effects_allowed();
        self.effects_allowed = allowed;
        if !allowed {
            if self.link.is_some() {
                self.shared
                    .fail_closed_on_stop
                    .store(self.lease.is_some(), Ordering::Relaxed);
                self.disconnect();
            }
            self.end_lease();
            self.deferred_release_request = None;
            self.deferred_release_ack_published = false;
            return;
        }
        self.expire_lease_if_needed();
        self.advance_deferred_release();
        // Reap a dead device thread (failed HELLO, wedged serial): a finished
        // thread leaves `link` occupied, which both swallows every queued
        // command (the settings UI keeps responding while the board holds the
        // old waveform) and blocks the auto-reconnect below.
        if self
            .link
            .as_ref()
            .and_then(|link| link.join.as_ref())
            .is_some_and(JoinHandle::is_finished)
        {
            self.link = None;
        }
        if self.connect_requested && self.link.is_none() {
            let now_ms = now_unix_ms();
            if now_ms.saturating_sub(self.last_reconnect_ms) >= RECONNECT_BACKOFF_MS {
                self.last_reconnect_ms = now_ms;
                self.connect();
            }
        }
    }

    #[cfg(test)]
    fn device_connected(&self) -> bool {
        self.shared
            .state
            .lock()
            .map(|state| state.connected)
            .unwrap_or(false)
    }

    fn commanded_summary(&self) -> String {
        match self.dac_band() {
            Ok((lo, hi, hold)) if self.mode == Mode::Const => format!(
                "{} {} hold={} (band {}..{})",
                self.method.name(),
                self.mode.name(),
                hold,
                lo,
                hi
            ),
            Ok((lo, hi, _)) => format!(
                "{} {} {}..{} @ {:.3} Hz",
                self.method.name(),
                self.mode.name(),
                lo,
                hi,
                self.frequency_hz
            ),
            Err(error) => format!(
                "{} {} invalid: {error}",
                self.method.name(),
                self.mode.name()
            ),
        }
    }

    /// The transfer curve the operator reasons about. Before any sweep it shows
    /// the lobe the configured `V_null`/`V_peak` endpoints claim, on a
    /// normalised axis, so
    /// the two numbers are legible with no hardware attached; after a fit it
    /// shows what was actually measured, in detector volts.
    fn curve_dataset(&self) -> Series1dV1 {
        let max_code = self.max_level.clamp(1, MAX_DAC_CODE) as f64;
        let sample_curve = |scale: f64, offset: f64, inversion: waveform::LobeInversion| {
            (0..=256)
                .map(|step| {
                    let code = max_code * f64::from(step) / 256.0;
                    Series1dPoint {
                        x: code,
                        y: offset + scale * inversion.u_for_dac(code),
                    }
                })
                .collect::<Vec<Series1dPoint>>()
        };
        // Two-point verticals mark the lobe endpoints on whatever y-range the
        // rest of the plot spans.
        let marker = |name: &str, code: f64, lo: f64, hi: f64| Series1dLine {
            name: name.to_owned(),
            points: vec![
                Series1dPoint { x: code, y: lo },
                Series1dPoint { x: code, y: hi },
            ],
        };

        let Some(fit) = self.fit.as_ref() else {
            let mut lines = Vec::new();
            if let Ok(inversion) = self.lobe_inversion() {
                lines.push(Series1dLine {
                    name: "configured lobe".into(),
                    points: sample_curve(1.0, 0.0, inversion),
                });
                lines.push(marker("V_null", inversion.v_null_dac, 0.0, 1.0));
                lines.push(marker("V_peak", inversion.v_peak_dac(), 0.0, 1.0));
            }
            return Series1dV1 {
                x_label: "DAC code".into(),
                y_label: "normalised transmission u (not yet measured)".into(),
                lines,
            };
        };

        let point_line = |direction: calibration::Direction| Series1dLine {
            name: format!("measured {}", direction.label()),
            points: fit
                .points
                .iter()
                .filter(|point| point.direction == direction)
                .map(|point| Series1dPoint {
                    x: f64::from(point.code),
                    y: point.volts,
                })
                .collect(),
        };
        let (lo, hi) = fit
            .points
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), point| {
                (lo.min(point.volts), hi.max(point.volts))
            });
        let mut lines = vec![
            point_line(calibration::Direction::Ascending),
            point_line(calibration::Direction::Descending),
            Series1dLine {
                name: "fit".into(),
                points: sample_curve(fit.span_volts, fit.offset_volts, fit.inversion()),
            },
        ];
        // The configured lobe on the fit's own scale: after applying they
        // coincide, and any divergence is the un-applied difference.
        if let Ok(configured) = self.lobe_inversion() {
            if configured != fit.inversion() {
                lines.push(Series1dLine {
                    name: "configured lobe".into(),
                    points: sample_curve(fit.span_volts, fit.offset_volts, configured),
                });
            }
        }
        lines.push(marker("V_null", fit.v_null_dac, lo, hi));
        lines.push(marker("V_peak", fit.v_peak_dac(), lo, hi));
        Series1dV1 {
            x_label: "DAC code".into(),
            y_label: "photodiode [V]".into(),
            lines,
        }
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let state = self.shared.state.lock().expect("device state lock");
        let connection = if state.connected {
            format!("connected ({})", state.firmware)
        } else if self.connect_requested {
            "connecting…".into()
        } else {
            "disconnected".into()
        };
        let board_code = state
            .board_code
            .map_or_else(|| "—".into(), |code| code.to_string());
        let error = state
            .last_error
            .clone()
            .or_else(|| self.last_error.clone())
            .unwrap_or_default();
        let board_mod = if state.board_mod.is_empty() {
            "—".to_owned()
        } else {
            state.board_mod.clone()
        };
        drop(state);
        let text_column = |id: &str, value: String| TableColumnData {
            column_id: id.to_owned(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                text_column("state", connection),
                text_column("commanded", self.commanded_summary()),
                text_column("board_mod", board_mod),
                text_column("board_code", board_code),
                text_column("error", error),
            ],
        }
    }

    fn status_schema(&self) -> TableSchema {
        let column = |id: &str, title: &str| TableColumn {
            id: id.to_owned(),
            title: title.to_owned(),
            value_type: TableValueType::String,
        };
        TableSchema {
            columns: vec![
                column("state", "State"),
                column("commanded", "Commanded drive"),
                column("board_mod", "Board modulation"),
                column("board_code", "Board DAC code"),
                column("error", "Last error"),
            ],
            ..TableSchema::default()
        }
    }
}

/// `YYYYmmdd-HHMMSS` in UTC, from the wall clock alone (no chrono dependency).
fn timestamp_slug() -> String {
    let seconds = now_unix_ms() / 1_000;
    let (days, time) = (seconds / 86_400, seconds % 86_400);
    // Civil-from-days, Howard Hinnant's algorithm, shifted to a 0000-03-01 era.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        time / 3_600,
        (time % 3_600) / 60,
        time % 60
    )
}

fn open_serial(port_hint: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    if port_hint == "auto" {
        // The dual-serial Teensy enumerates two ports and only the command
        // port answers HELLO — probe until one does.
        let candidates = serial_ports();
        if candidates.is_empty() {
            return Err(stage_a_io::transport::no_candidate_ports_message());
        }
        let mut failures = Vec::new();
        for path in &candidates {
            match probe_command_port(path) {
                // Restore the client's default reply timeout after probing.
                Ok(client) => return Ok(client.with_reply_timeout(Duration::from_millis(500))),
                Err(err) => failures.push(format!("{path}: {err}")),
            }
        }
        return Err(format!(
            "no Teensy command port answered HELLO ({})",
            failures.join("; ")
        ));
    }
    open_path(port_hint)
}

fn service_error(
    code: ServiceErrorCodeV1,
    message: impl Into<String>,
    retryable: bool,
) -> ServiceErrorV1 {
    ServiceErrorV1 {
        code,
        message: message.into(),
        retryable,
    }
}

fn lease_deadline(ttl_ms: u64) -> u64 {
    now_unix_ms().saturating_add(ttl_ms.clamp(MIN_LEASE_TTL_MS, MAX_LEASE_TTL_MS))
}

/// Wire token for the optical target on the `MOD wave=WARP` command.
fn optical_target_token(target: waveform::OpticalTarget) -> &'static str {
    match target {
        waveform::OpticalTarget::LogSine => "LOG_SINE",
        waveform::OpticalTarget::LinearSine => "LINEAR_SINE",
    }
}

fn waveform_command(waveform: &WaveformV1) -> Command {
    match waveform {
        WaveformV1::Off => Command::new("MOD").field("wave", "OFF"),
        WaveformV1::Constant { level_dac } => Command::new("MOD")
            .field("wave", "CONST")
            .field("level", *level_dac),
        WaveformV1::Periodic {
            waveform,
            min_dac,
            max_dac,
            frequency_millihz,
        } => Command::new("MOD")
            .field(
                "wave",
                match waveform {
                    stage_a_plugin_contract::PeriodicWaveformV1::Sine => "SINE",
                    stage_a_plugin_contract::PeriodicWaveformV1::Square => "SQUARE",
                },
            )
            .field("level", *max_dac)
            .field("min", *min_dac)
            .field("freq_mhz", *frequency_millihz),
    }
}

/// `CONFIG mode=A1` — the trigger policy an A1 run needs, with the
/// controller's own acquisition settings restated unchanged.
///
/// In A1 the firmware parks the comparator and stamps a phase-0 marker on the
/// photodiode stream at every cycle; in A2 the comparator drives the camera
/// trigger instead and no phase-0 marker is written. `CONFIG` accepts only
/// `mode`, `rate_hz`, `block_samples`, `raw` and `summary` — anything else is
/// answered with `SYNTAX unknown_config_field` — and it requires a mode *and* a
/// rate, so the mode cannot be changed without restating the rate.
/// The whole A1 preparation sequence, sent every time preparation is asked for.
///
/// A controller that *reports* `A1` has not necessarily been configured for A1:
/// A2's drive-synchronized capture configures A1 mode itself, at A2's own
/// sample rate, and the firmware releases an armed comparator only when a
/// `CONFIG` leaves A2. Restating the configuration costs one stream restart per
/// run and leaves the caller nothing to inherit. `CONFIG` is refused while the
/// acquisition runs, so `STOP` comes first; the closing `STATUS` is what the
/// readback check reads mode, state, trigger source and comparator from.
fn a1_prepare_commands(settings: &ControllerAcquisition) -> Vec<Command> {
    vec![
        Command::new("STOP").field("reason", "prepare_a1"),
        a1_config_command(settings),
        Command::new("START"),
        Command::new("STATUS"),
    ]
}

fn a1_config_command(settings: &ControllerAcquisition) -> Command {
    Command::new("CONFIG")
        .field("mode", "A1")
        .field("rate_hz", settings.rate_hz)
        .field("block_samples", settings.block_samples)
        .field("raw", u8::from(settings.emit_raw))
        .field("summary", u8::from(settings.emit_summary))
}

/// The acquisition settings a mode change has to carry over unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ControllerAcquisition {
    rate_hz: u32,
    block_samples: u32,
    emit_raw: bool,
    emit_summary: bool,
}

fn validate_a2_configuration(configuration: &A2AcquisitionConfigV1) -> Result<(), ServiceErrorV1> {
    let invalid = |message: &str| service_error(ServiceErrorCodeV1::InvalidCommand, message, false);
    if !(1..=1_000).contains(&configuration.mean_u_milli) {
        return Err(invalid("A2 mean_u_milli must be in 1..=1000"));
    }
    if configuration.depth_a_milli == 0 {
        return Err(invalid("A2 depth_a_milli must be positive"));
    }
    if !drive_frequency_supported(configuration.frequency_millihz) {
        return Err(invalid("A2 frequency is outside the firmware drive range"));
    }
    let half_us = 500_000_000_u64 / configuration.frequency_millihz;
    if half_us < u64::from(configuration.min_half_us) {
        return Err(invalid("A2 half-period is below min_half_us"));
    }
    if configuration.v_peak_dac <= configuration.v_null_dac {
        return Err(invalid("A2 v_peak_dac must be greater than v_null_dac"));
    }
    if configuration.timing_reference == A2TimingReferenceV1::Comparator
        && !(1..=4_095).contains(&configuration.comparator_threshold_dac)
    {
        return Err(invalid("A2 comparator threshold must be in 1..=4095"));
    }
    if configuration.comparator_hysteresis > 3 {
        return Err(invalid("A2 comparator hysteresis must be in 0..=3"));
    }
    if !(100..=500_000).contains(&configuration.sample_rate_hz) {
        return Err(invalid("A2 sample_rate_hz must be in 100..=500000"));
    }
    if configuration.block_samples == 0 {
        return Err(invalid("A2 block_samples must be positive"));
    }
    if !configuration.emit_raw_samples && !configuration.emit_summary {
        return Err(invalid("A2 must enable raw samples or summaries"));
    }
    Ok(())
}

fn verify_a2_prepared(
    fields: &BTreeMap<String, String>,
    reference: A2TimingReferenceV1,
) -> Result<(), String> {
    let source = fields.get("trigger_source").map(String::as_str);
    let wave = fields.get("mod_wave").map(String::as_str);
    let comparator = fields.get("cmp_armed").map(String::as_str);
    let valid = match reference {
        A2TimingReferenceV1::Comparator => {
            matches!(source, Some("PD_COMPARATOR" | "comparator"))
                && comparator == Some("1")
                && wave == Some("LOG_SQUARE")
        }
        A2TimingReferenceV1::DriveSync => {
            source == Some("J24_PHASE0") && comparator == Some("0") && wave == Some("SQUARE")
        }
    };
    if valid {
        Ok(())
    } else {
        Err(format!("A2 {:?} preparation not confirmed: trigger_source={source:?}, cmp_armed={comparator:?}, mod_wave={wave:?}", reference))
    }
}

fn a2_prepare_commands(c: &A2AcquisitionConfigV1) -> Result<Vec<Command>, ServiceErrorV1> {
    let mut commands = vec![
        Command::new("STOP").field("reason", "prepare_a2"),
        a2_config_command(c),
    ];
    match c.timing_reference {
        A2TimingReferenceV1::Comparator => {
            commands.push(a2_comparator_command(c));
            commands.push(a2_log_square_command(c));
        }
        A2TimingReferenceV1::DriveSync => {
            let mean = f64::from(c.mean_u_milli) / 1000.0;
            let depth = f64::from(c.depth_a_milli) / 1000.0;
            let high = mean * (0.5 * depth).exp();
            let low = mean * (-0.5 * depth).exp();
            if !high.is_finite() || high > 1.0 {
                return Err(service_error(
                    ServiceErrorCodeV1::InvalidCommand,
                    "A2 square exceeds the calibrated optical lobe",
                    false,
                ));
            }
            let code = |u: f64| {
                (f64::from(c.v_null_dac)
                    + 2.0 * f64::from(c.v_peak_dac - c.v_null_dac) / std::f64::consts::PI
                        * u.sqrt().asin())
                .round() as u16
            };
            let (low, high) = (code(low), code(high));
            if low == high || high > 4095 {
                return Err(service_error(
                    ServiceErrorCodeV1::InvalidCommand,
                    "A2 square is not representable by the stimulus DAC",
                    false,
                ));
            }
            commands.push(
                Command::new("MOD")
                    .field("wave", "SQUARE")
                    .field("min", low)
                    .field("level", high)
                    .field("freq_mhz", c.frequency_millihz),
            );
        }
    }
    Ok(commands)
}

fn a2_config_command(configuration: &A2AcquisitionConfigV1) -> Command {
    Command::new("CONFIG")
        .field(
            "mode",
            if configuration.timing_reference == A2TimingReferenceV1::Comparator {
                "A2"
            } else {
                "A1"
            },
        )
        .field("rate_hz", 20_000)
        .field("block_samples", configuration.block_samples)
        .field("raw", u8::from(configuration.emit_raw_samples))
        .field("summary", u8::from(configuration.emit_summary))
}

fn a2_comparator_command(configuration: &A2AcquisitionConfigV1) -> Command {
    Command::new("CMP")
        .field("thr", configuration.comparator_threshold_dac)
        .field("hyst", configuration.comparator_hysteresis)
        .field("invert", u8::from(configuration.comparator_invert))
}

fn a2_log_square_command(configuration: &A2AcquisitionConfigV1) -> Command {
    Command::new("MOD")
        .field("wave", "LOG_SQUARE")
        .field("a_milli", configuration.depth_a_milli)
        .field("u_k_milli", configuration.mean_u_milli)
        .field("v_null", configuration.v_null_dac)
        .field("v_pi", configuration.v_peak_dac - configuration.v_null_dac)
        .field("freq_mhz", configuration.frequency_millihz)
        .field("min_half_us", configuration.min_half_us)
}

fn accepted_service_reply(
    request: &PluginServiceRequest,
    response: &ModulationResponseV1,
) -> PluginServiceReply {
    PluginServiceReply {
        request_id: request.request_id,
        source_plugin_id: request.source_plugin_id.clone(),
        target_plugin_id: request.target_plugin_id.clone(),
        service: request.service.clone(),
        outcome: PluginServiceOutcome::Accepted {
            payload: serde_json::to_value(response).unwrap_or(Value::Null),
        },
    }
}

fn rejected_service_reply(
    request: &PluginServiceRequest,
    code: &str,
    message: impl Into<String>,
) -> PluginServiceReply {
    PluginServiceReply {
        request_id: request.request_id,
        source_plugin_id: request.source_plugin_id.clone(),
        target_plugin_id: request.target_plugin_id.clone(),
        service: request.service.clone(),
        outcome: PluginServiceOutcome::Rejected {
            code: code.into(),
            message: message.into(),
        },
    }
}

fn open_path(path: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let transport =
        stage_a_io::SerialTransport::open(path, 115_200, std::time::Duration::from_millis(20))
            .map_err(|err| err.to_string())?;
    Ok(StageAClient::new(transport))
}

/// Opens `path` and sends HELLO with a short timeout: only the Teensy
/// command port replies (the photodiode stream port never answers).
fn probe_command_port(path: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let mut client = open_path(path)?.with_reply_timeout(Duration::from_millis(300));
    client
        .request(&Command::new("HELLO").field("protocol", 1))
        .map_err(|err| err.to_string())?;
    Ok(client)
}

fn serial_ports() -> Vec<String> {
    stage_a_io::transport::candidate_ports()
        .into_iter()
        .map(|port| port.name)
        .collect()
}

/// The exact variant list the settings schema shows for the port enum — the
/// host exchanges enum settings as indices into this list. Real ports carry
/// their USB label (e.g. "(Teensyduino Dual Serial)") for recognisability;
/// only the leading path is the value.
fn port_variants() -> Vec<String> {
    let mut variants = vec!["auto".to_owned(), "mock".to_owned()];
    variants.extend(
        stage_a_io::transport::candidate_ports()
            .iter()
            .map(stage_a_io::transport::PortInfo::variant),
    );
    variants
}

/// The path part of a port variant; the parenthesised USB label is display-only.
fn variant_path(variant: &str) -> &str {
    variant.split_whitespace().next().unwrap_or(variant)
}

/// Host enum widgets send the selected index; string names are also accepted
/// (tests, saved configs).
fn enum_choice(value: &Value, variants: &[String]) -> Result<String, String> {
    if let Some(index) = value.as_u64() {
        return variants
            .get(usize::try_from(index).map_err(|_| "index out of range".to_owned())?)
            .cloned()
            .ok_or_else(|| format!("enum index {index} out of range"));
    }
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "expected an enum index or name".to_owned())
}

impl Plugin for StageAModulationPlugin {
    fn name(&self) -> &'static str {
        "Stage-A Modulation"
    }

    fn description(&self) -> &'static str {
        "Laser modulation control on the Teensy command port: capped power slider, constant/sine/square with frequency, applied immediately; shows the DAC code the board reports."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.connect_requested = false;
            self.shared
                .fail_closed_on_stop
                .store(self.lease.is_some(), Ordering::Relaxed);
            self.disconnect();
            self.end_lease();
            self.deferred_release_request = None;
        }
    }

    fn set_runtime_role(&mut self, role: PluginRuntimeRole) {
        self.runtime_role = role;
        if role != PluginRuntimeRole::LiveWorker {
            self.effects_allowed = false;
            if self.link.is_some() {
                self.shared
                    .fail_closed_on_stop
                    .store(self.lease.is_some(), Ordering::Relaxed);
                self.disconnect();
            }
            self.end_lease();
            self.deferred_release_request = None;
            self.deferred_release_ack_published = false;
        }
    }

    fn reset(&mut self) {}

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        // Control is settings-driven and works without camera frames. The
        // only frame-pass policy: replaying a recording must never keep a
        // hardware connection alive.
        if context.execution().mode == ExecutionMode::Replay && self.link.is_some() {
            self.connect_requested = false;
            self.shared
                .fail_closed_on_stop
                .store(self.lease.is_some(), Ordering::Relaxed);
            self.disconnect();
            self.end_lease();
            self.last_error = Some("disconnected: replay mode".into());
        }
    }

    fn process_control(&mut self, context: &mut PluginControlContext<'_>) {
        let execution = context.execution();
        self.apply_execution_context(&execution);
        // The photodiode owner broadcasts its summary to every plugin's inbox,
        // so a calibration sweep reads the light with no lease and no request.
        let level = context
            .inbox()
            .snapshots
            .iter()
            .find(|snapshot| {
                snapshot.plugin_id == PLUGIN_ID_STAGE_A_PHOTODIODE
                    && snapshot.topic == CTX_STAGE_A_PHOTODIODE_SUMMARY_V1
            })
            .and_then(|snapshot| {
                serde_json::from_value::<PhotodiodeSummaryV1>(snapshot.payload.clone()).ok()
            })
            .map(|summary| (summary.stream.level, summary.stream.sample_rate_hz));
        let (level, sample_rate_hz) = level.unwrap_or((None, None));
        self.drive_calibration(level, sample_rate_hz);
    }

    fn handle_service_request(
        &mut self,
        request: &PluginServiceRequest,
        execution: &augur_plugin_api::ExecutionContext,
    ) -> PluginServiceReply {
        if let Some(index) = self.request_cache.iter().position(|(previous, _)| {
            previous.source_plugin_id == request.source_plugin_id
                && previous.request_id == request.request_id
        }) {
            let (previous, cached_reply) = self.request_cache[index].clone();
            if previous != *request {
                return rejected_service_reply(
                    request,
                    "request_id_conflict",
                    "request ID was reused for different modulation payload",
                );
            }
            let cached_in_progress = match &cached_reply.outcome {
                PluginServiceOutcome::Accepted { payload } => serde_json::from_value::<
                    ModulationResponseV1,
                >(payload.clone())
                .is_ok_and(|response| response.common.outcome == RequestOutcomeV1::InProgress),
                PluginServiceOutcome::Rejected { .. } => false,
            };
            if cached_in_progress {
                let terminal = self.shared.completions.lock().ok().and_then(|responses| {
                    responses
                        .iter()
                        .rev()
                        .find(|(client, response)| {
                            client.as_str() == request.source_plugin_id
                                && response.common.request_id.0 == request.request_id
                        })
                        .map(|(_, response)| response.clone())
                });
                if let Some(terminal) = terminal {
                    let upgraded = accepted_service_reply(request, &terminal);
                    self.request_cache[index].1 = upgraded.clone();
                    return upgraded;
                }
            }
            return cached_reply;
        }

        let reply = if request.target_plugin_id != PLUGIN_ID_STAGE_A_MODULATION {
            rejected_service_reply(request, "wrong_target", "wrong modulation owner target")
        } else if request.service != SERVICE_STAGE_A_MODULATION_CONTROL_V1 {
            rejected_service_reply(
                request,
                "unsupported_service",
                format!("unsupported modulation service '{}'", request.service),
            )
        } else if self.runtime_role != PluginRuntimeRole::LiveWorker
            || !execution.hardware_effects_allowed()
        {
            rejected_service_reply(
                request,
                "effects_not_allowed",
                "modulation effects are allowed only on the active live worker",
            )
        } else {
            self.effects_allowed = true;
            match serde_json::from_value::<ModulationRequestV1>(request.payload.clone()) {
                Err(err) => rejected_service_reply(
                    request,
                    "invalid_payload",
                    format!("invalid modulation request: {err}"),
                ),
                Ok(payload)
                    if payload.contract_version != CONTRACT_VERSION_V1
                        || payload.request_id.0 != request.request_id
                        || payload.requester.as_str() != request.source_plugin_id
                        || payload
                            .target_owner_instance
                            .as_ref()
                            .is_some_and(|owner| owner != &self.owner_instance) =>
                {
                    rejected_service_reply(
                        request,
                        "identity_mismatch",
                        "contract version, request, requester, or owner instance mismatch",
                    )
                }
                Ok(payload)
                    if payload.issued_at_unix_ms != 0
                        && (now_unix_ms().saturating_sub(payload.issued_at_unix_ms) > 120_000
                            || payload.issued_at_unix_ms.saturating_sub(now_unix_ms())
                                > 30_000) =>
                {
                    rejected_service_reply(request, "stale_request", "request timestamp is stale")
                }
                Ok(payload) => match self.handle_modulation_command(&payload) {
                    Ok(response) => accepted_service_reply(request, &response),
                    Err(error) => rejected_service_reply(
                        request,
                        &format!("{:?}", error.code).to_ascii_lowercase(),
                        error.message,
                    ),
                },
            }
        };
        self.request_cache
            .push_back((request.clone(), reply.clone()));
        while self.request_cache.len() > REQUEST_CACHE_LIMIT {
            self.request_cache.pop_front();
        }
        reply
    }

    fn control_snapshots(&self) -> Vec<PluginControlSnapshot> {
        vec![PluginControlSnapshot {
            plugin_id: PLUGIN_ID_STAGE_A_MODULATION.into(),
            topic: CTX_STAGE_A_MODULATION_STATE_V1.into(),
            revision: self.shared.generation.load(Ordering::Relaxed).max(1),
            payload: serde_json::to_value(self.control_state()).unwrap_or(Value::Null),
        }]
    }

    fn settings_schema(&self) -> SettingsSchema {
        let port_variants = port_variants();
        let port_default = port_variants
            .iter()
            .position(|p| variant_path(p) == self.port_hint)
            .unwrap_or(0);
        let method_variants: Vec<String> = DriveMethod::VARIANTS
            .iter()
            .map(|method| method.name().to_owned())
            .collect();
        let method_default = DriveMethod::VARIANTS
            .iter()
            .position(|method| *method == self.method)
            .unwrap_or(0);
        let mode_variants: Vec<String> =
            Mode::VARIANTS.iter().map(|m| m.name().to_owned()).collect();
        let mode_default = Mode::VARIANTS
            .iter()
            .position(|m| *m == self.mode)
            .unwrap_or(0);
        let mut modulation_items = vec![
            SettingItem {
                key: "port".into(),
                label: "Port".into(),
                tooltip: Some(
                    "auto (recommended) probes the attached USB serial ports and picks \
                     the one that answers HELLO — the Teensy command port; \
                     mock = in-process simulated controller"
                        .into(),
                ),
                kind: SettingKind::Enum {
                    variants: port_variants,
                    default: port_default,
                },
            },
            SettingItem {
                key: "connect".into(),
                label: "Connect".into(),
                tooltip: Some(
                    "Opens/closes the command port. Connecting never changes the \
                     output; disconnecting leaves it held (set-and-hold firmware)."
                        .into(),
                ),
                kind: SettingKind::Bool {
                    default: self.connect_requested,
                },
            },
            SettingItem {
                key: "max_level".into(),
                label: "Max limit (DAC code)".into(),
                tooltip: Some(
                    "Hard ceiling for every drive. No manual or calibrated waveform may \
                     produce a DAC code above this value at J23."
                        .into(),
                ),
                kind: SettingKind::I64Drag {
                    min: 0,
                    max: MAX_DAC_CODE,
                    default: self.max_level,
                },
            },
            SettingItem {
                key: "method".into(),
                label: "Drive method".into(),
                tooltip: Some(
                    "MANUAL defines the DAC band with Power and Min threshold. CALIBRATED \
                     derives it from V_null, V_peak, the mean lobe point ū and the depth a."
                        .into(),
                ),
                kind: SettingKind::Enum {
                    variants: method_variants,
                    default: method_default,
                },
            },
            SettingItem {
                key: "mode".into(),
                label: "Mode".into(),
                tooltip: Some(
                    "Selects the waveform that fills the method-defined operating band. \
                     All five modes are available with both drive methods."
                        .into(),
                ),
                kind: SettingKind::Enum {
                    variants: mode_variants,
                    default: mode_default,
                },
            },
            SettingItem {
                key: "frequency_hz".into(),
                label: "Frequency".into(),
                tooltip: Some(format!(
                    "Periodic-waveform frequency, {:.2}–{} Hz",
                    DRIVE_FREQUENCY_MIN_MILLIHZ as f64 / 1_000.0,
                    DRIVE_FREQUENCY_MAX_MILLIHZ / 1_000
                )),
                kind: SettingKind::F64Drag {
                    min: DRIVE_FREQUENCY_MIN_MILLIHZ as f64 / 1_000.0,
                    max: DRIVE_FREQUENCY_MAX_MILLIHZ as f64 / 1_000.0,
                    speed: 1.0,
                    default: self.frequency_hz,
                },
            },
        ];
        match self.method {
            DriveMethod::Manual => {
                modulation_items.push(SettingItem {
                    key: "level".into(),
                    label: "Power (DAC code)".into(),
                    tooltip: Some(
                        "Manual peak/operating DAC code. CONST holds this value; periodic \
                         modes use it as the upper end of the manual band."
                            .into(),
                    ),
                    kind: SettingKind::I64Slider {
                        min: 0,
                        max: self.max_level,
                        default: self.level,
                        suffix: None,
                    },
                });
                modulation_items.push(SettingItem {
                    key: "min_level".into(),
                    label: "Min threshold (DAC code)".into(),
                    tooltip: Some(
                        "Lower end of the manual DAC band. Ignored by CONST, which holds Power."
                            .into(),
                    ),
                    kind: SettingKind::I64Slider {
                        min: 0,
                        max: self.max_level,
                        default: self.min_level,
                        suffix: None,
                    },
                });
            }
            DriveMethod::Calibrated => {
                let (v_null, v_peak) = self.effective_lobe();
                let range = self.achievable();
                modulation_items.push(SettingItem {
                    key: "v_null_dac".into(),
                    label: "V_null (DAC code at MIN light)".into(),
                    tooltip: Some(
                        "DAC code where the excitation light bottoms out (sin² = 0) on one \
                         monotonic Pockels lobe. Read it off a sweep — Measure transfer curve \
                         below fills both codes in for you."
                            .into(),
                    ),
                    kind: SettingKind::I64Drag {
                        min: 0,
                        max: MAX_DAC_CODE,
                        default: v_null,
                    },
                });
                modulation_items.push(SettingItem {
                    key: "v_peak_dac".into(),
                    label: "V_peak (DAC code at MAX light)".into(),
                    tooltip: Some(
                        "DAC code where the excitation light is brightest, on the same lobe as \
                         V_null. Both fields are codes you can point at on the transfer curve; \
                         the half-wave span between them is derived, never typed. u = 1 holds \
                         exactly here and u = 0 at V_null."
                            .into(),
                    ),
                    kind: SettingKind::I64Drag {
                        min: 0,
                        max: MAX_DAC_CODE,
                        default: v_peak,
                    },
                });
                modulation_items.push(SettingItem {
                    key: "operating_point".into(),
                    label: match range {
                        Some(range) => {
                            format!(
                                "Mean lobe point ū (0..{:.2} at a={:.2})",
                                range.max_mean_u, self.depth_a
                            )
                        }
                        None => "Mean lobe point ū (0..1)".into(),
                    },
                    tooltip: Some(
                        "How bright the light sits on average, as a fraction of the lobe's \
                         maximum. Dimensionless — not the physical flux I_k.\n\n\
                         The label shows how far up you can go at the depth a currently set; \
                         past that the peak of the swing would run off the top of the lobe. \
                         Dragging beyond it stops at the limit instead of refusing the edit."
                            .into(),
                    ),
                    kind: SettingKind::F64Drag {
                        min: waveform::MEAN_U_MIN,
                        max: 1.0,
                        speed: 0.01,
                        default: self.operating_point,
                    },
                });
                modulation_items.push(SettingItem {
                    key: "depth_a".into(),
                    label: match range {
                        Some(range) => format!(
                            "Optical depth a (0..{:.2} at ū={:.2})",
                            range.max_depth_a, self.operating_point
                        ),
                        None => "Optical depth a".into(),
                    },
                    tooltip: Some(
                        "Peak-to-trough log contrast a = ln(I_max/I_min) of the light.\n\n\
                         The label shows the deepest a the lobe can reach at the mean point \
                         currently set — lower ū to get more depth. Dragging beyond it stops at \
                         the limit instead of refusing the edit."
                            .into(),
                    ),
                    kind: SettingKind::F64Drag {
                        min: waveform::DEPTH_A_MIN,
                        max: waveform::DEPTH_A_MAX,
                        speed: 0.01,
                        default: self.depth_a,
                    },
                });
            }
        }
        let geometry_variants: Vec<String> = calibration::DetectorGeometry::VARIANTS
            .iter()
            .map(|geometry| geometry.name().to_owned())
            .collect();
        let geometry_default = calibration::DetectorGeometry::VARIANTS
            .iter()
            .position(|geometry| *geometry == self.detector_geometry)
            .unwrap_or(0);
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Laser modulation".into(),
                    description: Some(
                        "Tick Connect, then every change is sent to the Teensy immediately — no \
                         camera required. Method selects the operating band; Mode selects its \
                         waveform. Max limit is the hard ceiling. The firmware holds the output \
                         when disconnected."
                            .into(),
                    ),
                    default_open: true,
                    items: modulation_items,
                },
                SettingsSection {
                    label: "Calibration".into(),
                    description: Some(
                        "Measures the Pockels/PBS transfer curve: steps settled CONST DAC codes \
                         across the range while reading the photodiode, then fits the lobe. \
                         Needs the photodiode plugin connected. The sweep restores your armed \
                         drive when it finishes, and the fit is never applied without your \
                         confirmation. Watch the transfer-curve view."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "detector_geometry".into(),
                            label: "Detector port".into(),
                            tooltip: Some(
                                "Which way the photodiode moves when the light reaching the \
                                 sample gets brighter. Stage-A's photodiode sits on the PBS \
                                 reject port and reads the leftover light, I_pd = I_tot − I_exc, \
                                 so it goes DOWN as the sample gets brighter — that is REJECT \
                                 PORT, the default. Pick DIRECT only for a detector that watches \
                                 the sample beam itself. The sweep cannot work this out: a bright \
                                 and a dark extremum fit the measured curve equally well, and \
                                 only the optics say which one is zero light on the sample. \
                                 Choosing wrong puts V_null one half-wave-voltage span off."
                                    .into(),
                            ),
                            kind: SettingKind::Enum {
                                variants: geometry_variants,
                                default: geometry_default,
                            },
                        },
                        SettingItem {
                            key: "calibrate".into(),
                            label: "Measure transfer curve".into(),
                            tooltip: Some(
                                "Sweeps the full range up and back down (~20 s), then fits the \
                                 lobe. Press again to abort; the armed drive is restored either \
                                 way. Progress and the result appear in the status lines below."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: self.calibration_offered(),
                            },
                        },
                        SettingItem {
                            key: "calibrate_apply".into(),
                            label: "Apply to V_null / V_peak".into(),
                            tooltip: Some(
                                "Writes the measured lobe into V_null and V_peak above. Residual, \
                                 dropped points, hysteresis and clipping stay visible as warnings \
                                 — applying only refuses a pair that names no monotonic lobe at \
                                 all. If the depth or mean point no longer fit the new lobe they \
                                 move to its nearest reachable value rather than blocking this."
                                    .into(),
                            ),
                            kind: SettingKind::Button {
                                enabled: self.calibration_offered(),
                            },
                        },
                        SettingItem {
                            key: "calibration_dir".into(),
                            label: "Calibration folder (optional)".into(),
                            tooltip: Some(
                                "Where each applied calibration is archived, as one named JSON \
                                 file per apply. It holds every swept point (code, direction, \
                                 volts, whether it clipped), the fitted V_null and V_peak, the \
                                 residual and hysteresis, and the detector reading at the \
                                 excitation null — which is the total power I_tot the photodiode \
                                 plugin measures its depth against. That makes an old run's \
                                 inversion reproducible after the bench has been touched.\n\n\
                                 Leave it empty to apply the fit without keeping a record."
                                    .into(),
                            ),
                            kind: SettingKind::Path {
                                dialog: PathDialogKind::Directory,
                                default: self.calibration_dir.clone(),
                            },
                        },
                    ],
                },
            ],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            // Enum settings are exchanged as indices into the schema's
            // variant list (see the host settings UI).
            "port" => {
                let index = port_variants()
                    .iter()
                    .position(|p| variant_path(p) == self.port_hint)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "connect" => Some(json!(self.connect_requested)),
            "level" => Some(json!(self.level)),
            "max_level" => Some(json!(self.max_level)),
            "method" => {
                let index = DriveMethod::VARIANTS
                    .iter()
                    .position(|method| *method == self.method)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "mode" => {
                let index = Mode::VARIANTS
                    .iter()
                    .position(|m| *m == self.mode)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "frequency_hz" => Some(json!(self.frequency_hz)),
            "min_level" => Some(json!(self.min_level)),
            "depth_a" => Some(json!(self.depth_a)),
            "operating_point" => Some(json!(self.operating_point)),
            // Through `effective_lobe`, so a lobe another instance just
            // applied is what goes into the settings snapshot — not the stale
            // pair that would overwrite it (see APPLIED_LOBE).
            "v_null_dac" => Some(json!(self.effective_lobe().0)),
            "v_peak_dac" => Some(json!(self.effective_lobe().1)),
            "detector_geometry" => {
                let index = calibration::DetectorGeometry::VARIANTS
                    .iter()
                    .position(|geometry| *geometry == self.detector_geometry)
                    .unwrap_or(0);
                Some(json!(index))
            }
            "calibration_dir" => Some(json!(self.calibration_dir)),
            // Momentary buttons export a monotonic press counter so a press on
            // the UI mirror reaches the live worker through the settings
            // snapshot (ADR 010).
            "calibrate" => Some(self.press_measure.value()),
            "calibrate_apply" => Some(self.press_apply.value()),
            // The live worker reports the actual run state; the UI mirror
            // reports the operator's request so the settings snapshot can
            // transport the start to the worker (which owns the device link).
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        // Catch up on a lobe applied elsewhere in this process before reading
        // any of it, so an incoming echo of the old codes cannot land on top.
        if self.adopt_applied_lobe() {
            self.reconcile_drive(DriveKnob::Lobe);
        }
        if self.lease.is_some() {
            return Err(format!(
                "setting '{key}' is locked while automation holds the modulation lease"
            ));
        }
        match key {
            "port" => {
                self.port_hint = variant_path(&enum_choice(&value, &port_variants())?).to_owned();
                Ok(())
            }
            "connect" => {
                let requested = value.as_bool().ok_or("connect must be a boolean")?;
                self.connect_requested = requested;
                if requested {
                    self.connect();
                } else {
                    self.disconnect();
                }
                Ok(())
            }
            "level" => {
                self.level = value
                    .as_i64()
                    .ok_or("level must be an integer")?
                    .clamp(0, self.max_level);
                if self.min_level > self.level {
                    self.min_level = self.level;
                }
                if self.method == DriveMethod::Manual {
                    self.send_modulation();
                }
                Ok(())
            }
            "max_level" => {
                self.max_level = value
                    .as_i64()
                    .ok_or("max_level must be an integer")?
                    .clamp(0, MAX_DAC_CODE);
                // Lowering the ceiling below the manual peak lowers that peak.
                if self.level > self.max_level {
                    self.level = self.max_level;
                }
                if self.min_level > self.max_level {
                    self.min_level = self.max_level;
                }
                // The ceiling caps the reachable intensity, so it moves the
                // optical range too.
                self.reconcile_drive(DriveKnob::Lobe);
                Ok(())
            }
            "method" => {
                let method_names: Vec<String> = DriveMethod::VARIANTS
                    .iter()
                    .map(|method| method.name().to_owned())
                    .collect();
                let name = enum_choice(&value, &method_names)?;
                self.method = DriveMethod::from_name(&name)
                    .ok_or_else(|| format!("unknown drive method: {name}"))?;
                self.reconcile_drive(DriveKnob::Lobe);
                Ok(())
            }
            "mode" => {
                let mode_names: Vec<String> =
                    Mode::VARIANTS.iter().map(|m| m.name().to_owned()).collect();
                let name = enum_choice(&value, &mode_names)?;
                // Always accepted. Selecting a mode whose parameters do not
                // fit yet used to snap the dropdown back with an error about a
                // value the operator was not editing; now the mode takes and
                // the parameters follow it.
                self.mode =
                    Mode::from_name(&name).ok_or_else(|| format!("unknown mode: {name}"))?;
                self.reconcile_drive(DriveKnob::Lobe);
                Ok(())
            }
            "frequency_hz" => {
                let hz = value.as_f64().ok_or("frequency_hz must be a number")?;
                self.frequency_hz = hz.clamp(
                    DRIVE_FREQUENCY_MIN_MILLIHZ as f64 / 1_000.0,
                    DRIVE_FREQUENCY_MAX_MILLIHZ as f64 / 1_000.0,
                );
                if self.mode.is_periodic() {
                    self.send_modulation();
                }
                Ok(())
            }
            "min_level" => {
                self.min_level = value
                    .as_i64()
                    .ok_or("min_level must be an integer")?
                    .clamp(0, self.level);
                if self.method == DriveMethod::Manual {
                    self.send_modulation();
                }
                Ok(())
            }
            "depth_a" => {
                let depth_a = value
                    .as_f64()
                    .ok_or("depth_a must be a number")?
                    .clamp(waveform::DEPTH_A_MIN, waveform::DEPTH_A_MAX);
                self.depth_a = depth_a;
                self.reconcile_drive(DriveKnob::Depth);
                Ok(())
            }
            "operating_point" => {
                self.operating_point = value
                    .as_f64()
                    .ok_or("operating_point must be a number")?
                    .clamp(waveform::MEAN_U_MIN, 1.0);
                self.reconcile_drive(DriveKnob::Mean);
                Ok(())
            }
            "v_null_dac" => {
                self.v_null_dac = value
                    .as_i64()
                    .ok_or("v_null_dac must be an integer")?
                    .clamp(0, MAX_DAC_CODE);
                self.reconcile_drive(DriveKnob::Lobe);
                Ok(())
            }
            // `v_pi_dac` is the pre-endpoint key: a *distance* from V_null. Kept
            // settable so a stored config still loads, converted on the way in.
            // It is deliberately absent from `settings_schema`, so nothing new
            // can be authored against the form that caused the mix-up.
            "v_peak_dac" | "v_pi_dac" => {
                let entered = value
                    .as_i64()
                    .ok_or("v_peak_dac must be an integer")?
                    .clamp(0, MAX_DAC_CODE);
                let v_peak_dac = if key == "v_pi_dac" {
                    (self.v_null_dac + entered).clamp(0, MAX_DAC_CODE)
                } else {
                    entered
                };
                self.v_peak_dac = v_peak_dac;
                self.reconcile_drive(DriveKnob::Lobe);
                Ok(())
            }
            "detector_geometry" => {
                let variants: Vec<String> = calibration::DetectorGeometry::VARIANTS
                    .iter()
                    .map(|geometry| geometry.name().to_owned())
                    .collect();
                let chosen = enum_choice(&value, &variants)?;
                self.detector_geometry = calibration::DetectorGeometry::from_name(&chosen)
                    .ok_or("unknown detector geometry")?;
                // The stored fit was resolved against the old geometry; re-fit
                // rather than leave a V_null that is now one half-wave-voltage span out.
                if let Some(fit) = self.fit.take() {
                    match calibration::fit_transfer(
                        &fit.points,
                        f64::from(self.max_level.clamp(1, MAX_DAC_CODE) as u16),
                        self.detector_geometry,
                    ) {
                        Ok(refitted) => self.fit = Some(refitted),
                        Err(error) => self.calibration_status = format!("re-fit failed: {error}"),
                    }
                }
                Ok(())
            }
            "calibration_dir" => {
                self.calibration_dir = value
                    .as_str()
                    .ok_or("calibration_dir must be a string")?
                    .to_owned();
                Ok(())
            }
            "calibrate" => {
                if !self.press_measure.accept(&value) {
                    return Ok(());
                }
                if self.sweep.is_some() {
                    self.finish_calibration_sweep("sweep stopped".into());
                } else {
                    self.start_calibration_sweep();
                }
                self.shared.bump();
                Ok(())
            }
            "calibrate_apply" => {
                if !self.press_apply.accept(&value) {
                    return Ok(());
                }
                self.apply_calibration_fit();
                Ok(())
            }
            _ => Err(format!("unknown setting: {key}")),
        }
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        let state = self.shared.state.lock().expect("device state lock");
        entries.push(StatusEntry::Text(if state.connected {
            format!("Modulation: connected ({})", state.firmware)
        } else if self.connect_requested {
            "Modulation: connecting…".into()
        } else {
            "Modulation: disconnected".into()
        }));
        if let Some(code) = state.board_code {
            entries.push(StatusEntry::Text(format!(
                "Board: code={code} ({})",
                state.board_mod
            )));
        }
        entries.push(StatusEntry::Text(format!(
            "Drive: method={}, mode={}",
            self.method.name(),
            self.mode.name()
        )));
        if self.method == DriveMethod::Calibrated || self.mode.optical_target().is_some() {
            // Spell the lobe out in the operator's own units. A wrong endpoint
            // shows up here immediately — the code u = 1 maps to is the code
            // where the light should be brightest, and nothing in between may
            // overshoot it.
            entries.push(StatusEntry::Text(match self.resolved_lobe() {
                Ok(lobe) => {
                    let inversion = lobe.inversion;
                    format!(
                        "Lobe: half-wave span {:.0} codes — u 0 → {:.0} (min light), 0.5 → {:.0}, \
                         1 → {:.0} (max light){}",
                        inversion.v_pi_dac,
                        inversion.dac_for_u(0.0),
                        inversion.dac_for_u(0.5),
                        inversion.dac_for_u(1.0),
                        if lobe.folded {
                            format!(
                                ", folded onto the ascending branch V_null {:.0} → V_peak {:.0} \
                                 (the pair was entered running downward in code)",
                                inversion.v_null_dac,
                                inversion.v_peak_dac()
                            )
                        } else {
                            String::new()
                        }
                    )
                }
                Err(error) => format!("Lobe invalid: {error}"),
            }));
        }
        match self.dac_band() {
            Ok((lo, hi, hold)) => entries.push(StatusEntry::Text(format!(
                "Resolved DAC band: {lo}..{hi} (hold {hold}, {} codes peak-to-peak)",
                hi.saturating_sub(lo)
            ))),
            Err(error) => entries.push(StatusEntry::Text(format!(
                "Resolved DAC band invalid: {error}"
            ))),
        }
        if let Some(target) = self.mode.optical_target() {
            match (self.optical_warp_table(target), self.optical_drive(target)) {
                (Ok(_), Ok(drive)) => {
                    entries.push(StatusEntry::Text(format!(
                        "{}: a={:.2}, ū={:.2}, internal u={:.2}, V_null={:.0}, V_peak={:.0} @ {:.3} Hz",
                        self.mode.name(),
                        drive.depth_a,
                        self.operating_point,
                        drive.operating_point,
                        drive.inversion.v_null_dac,
                        drive.inversion.v_peak_dac(),
                        self.frequency_hz,
                    )));
                }
                (Err(error), _) | (_, Err(error)) => entries.push(StatusEntry::Text(format!(
                    "Optical drive not sent: {error}"
                ))),
            }
            // The two optical controls are coupled through one ceiling, so the
            // useful readout is not "that value is invalid" but where the
            // boundary actually is. Edits clamp against exactly these numbers.
            if let Some(range) = self.achievable() {
                entries.push(StatusEntry::Text(format!(
                    "Achievable now: a ≤ {:.2} at ū={:.2}  ·  ū ≤ {:.2} at a={:.2}  \
                     (peaks at u={:.2} of {:.2}; max limit {})",
                    range.max_depth_a,
                    self.operating_point,
                    range.max_mean_u,
                    self.depth_a,
                    range.peak_u,
                    range.u_max,
                    self.max_level,
                )));
            }
        }
        if let Some(sweep) = self.sweep.as_ref() {
            entries.push(StatusEntry::Text(format!(
                "Calibration: sweeping {}/{}",
                sweep.index + 1,
                sweep.total()
            )));
        } else if !self.calibration_status.is_empty() {
            entries.push(StatusEntry::Text(format!(
                "Calibration: {}",
                self.calibration_status
            )));
        }
        if let Some(fit) = self.fit.as_ref() {
            // The reject-port extremum bounds the anchor from below but is not
            // the anchor: the residual transmitted floor is not separable here
            // (knowledge base `pockels-waveform-linearisation.md` §4.4).
            entries.push(StatusEntry::Text(format!(
                "Detector at null: {:.3} V — lower bound on the total-power anchor I_tot, \
                 not the anchor itself",
                fit.detector_volts_at_null()
            )));
            for warning in self.fit_warnings() {
                entries.push(StatusEntry::Text(format!("Check: {warning}")));
            }
        }
        if let Some(calibration_id) = self.calibration_id.as_ref() {
            entries.push(StatusEntry::Text(format!(
                "Calibration in use: {calibration_id}"
            )));
        }
        if let Some(error) = state.last_error.clone().or_else(|| self.last_error.clone()) {
            entries.push(StatusEntry::Text(format!("Error: {error}")));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        HostViewRegistry {
            datasets: vec![
                HostDatasetDescriptor {
                    id: STATUS_DATASET_ID.into(),
                    title: "Laser modulation".into(),
                    kind: HostDatasetKind::TableV1(self.status_schema()),
                    empty_message: "Modulation control idle.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: CURVE_DATASET_ID.into(),
                    title: "Pockels transfer curve".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "Set V_null and V_peak, or measure a transfer curve.".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: STATUS_VIEW_ID.into(),
                    title: "Laser modulation".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
                HostViewDescriptor {
                    id: CURVE_VIEW_ID.into(),
                    title: "Pockels transfer curve".into(),
                    dataset_id: CURVE_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::LineSeriesWindow,
                },
            ],
            actions: Vec::new(),
        }
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            CURVE_DATASET_ID => serde_json::to_vec(&self.curve_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            STATUS_DATASET_ID | CURVE_DATASET_ID => {
                self.shared.generation.load(Ordering::Relaxed).max(1)
            }
            _ => 0,
        }
    }
}

impl Drop for StageAModulationPlugin {
    fn drop(&mut self) {
        self.disconnect();
    }
}

export_plugin!(StageAModulationPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use augur_plugin_api::{ExecutionContext, ExecutionMode};

    fn live_execution() -> ExecutionContext {
        ExecutionContext {
            mode: ExecutionMode::LiveCapture,
            effects_allowed: true,
            session_id: Some("test".into()),
        }
    }

    fn service_request(
        plugin: &StageAModulationPlugin,
        id: u64,
        requester: &str,
        command: ModulationCommandV1,
        revision: Option<u64>,
    ) -> PluginServiceRequest {
        let mut payload = ModulationRequestV1::new(
            stage_a_plugin_contract::RequestId(id),
            ClientId::from(requester),
            command,
        );
        payload.target_owner_instance = Some(plugin.owner_instance.clone());
        payload.run_id = Some(RunId::from("run-a"));
        payload.lease_id = Some(LeaseId::from("lease-a"));
        payload.requested_revision = revision.map(SemanticRevision);
        payload.issued_at_unix_ms = now_unix_ms();
        PluginServiceRequest {
            request_id: id,
            source_plugin_id: requester.into(),
            target_plugin_id: PLUGIN_ID_STAGE_A_MODULATION.into(),
            service: SERVICE_STAGE_A_MODULATION_CONTROL_V1.into(),
            payload: serde_json::to_value(payload).unwrap(),
        }
    }

    fn live_plugin() -> StageAModulationPlugin {
        let mut plugin = StageAModulationPlugin::default();
        plugin.set_runtime_role(PluginRuntimeRole::LiveWorker);
        plugin.effects_allowed = true;
        plugin
    }

    fn wait_until<F: Fn(&StageAModulationPlugin) -> bool>(
        plugin: &StageAModulationPlugin,
        timeout: Duration,
        done: F,
    ) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if done(plugin) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("condition not reached within {timeout:?}");
    }

    fn board_code(plugin: &StageAModulationPlugin) -> Option<i64> {
        plugin.shared.state.lock().unwrap().board_code
    }

    /// Connect checkbox → slider change → MOD sent by the device thread →
    /// board echoes the code. No process_frame involved anywhere.
    #[test]
    fn level_change_transfers_without_frames() {
        let mut plugin = live_plugin();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        assert_eq!(
            plugin.shared.state.lock().unwrap().firmware,
            "0.3.0-mock".to_owned()
        );

        plugin.set_setting("level", json!(1234)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(1234)
        });
        assert!(plugin.shared.state.lock().unwrap().last_error.is_none());

        plugin.set_setting("connect", json!(false)).unwrap();
        assert!(!plugin.device_connected());
    }

    /// The max cap bounds the slider, and lowering it re-sends a lower level.
    #[test]
    fn max_level_caps_the_slider() {
        let mut plugin = live_plugin();
        plugin.set_setting("max_level", json!(1000)).unwrap();
        plugin.set_setting("level", json!(4095)).unwrap();
        assert_eq!(plugin.level, 1000, "slider clamps to the cap");

        plugin.set_setting("max_level", json!(500)).unwrap();
        assert_eq!(plugin.level, 500, "lowering the cap lowers the level");

        let schema = plugin.settings_schema();
        let level_item = schema.sections[0]
            .items
            .iter()
            .find(|item| item.key == "level")
            .expect("level setting exists");
        match &level_item.kind {
            SettingKind::I64Slider { max, .. } => assert_eq!(*max, 500),
            other => panic!("level must stay a slider, got {other:?}"),
        }
    }

    /// Square drive with min threshold reaches the mock and starts at min;
    /// slider to 0 drives the output to 0.
    #[test]
    fn square_with_min_threshold_round_trips() {
        let mut plugin = live_plugin();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());

        plugin.set_setting("level", json!(2000)).unwrap();
        plugin.set_setting("frequency_hz", json!(10.0)).unwrap();
        plugin.set_setting("min_level", json!(500)).unwrap();
        plugin.set_setting("mode", json!("SQUARE")).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(500)
        });
        assert!(plugin
            .shared
            .state
            .lock()
            .unwrap()
            .board_mod
            .contains("SQUARE 500..2000"));

        plugin.set_setting("mode", json!("CONST")).unwrap();
        plugin.set_setting("level", json!(0)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(0)
        });
        plugin.set_setting("connect", json!(false)).unwrap();
    }

    /// The host settings UI exchanges enum values as indices into the
    /// schema's variant list (radio buttons send `json!(index)`).
    #[test]
    fn enum_settings_round_trip_as_indices() {
        let mut plugin = live_plugin();
        // Drive method: index 1 = CALIBRATED.
        plugin
            .set_setting("method", json!(1))
            .expect("method index accepted");
        assert_eq!(plugin.method, DriveMethod::Calibrated);
        assert_eq!(plugin.get_setting("method"), Some(json!(1)));
        // Mode: index 2 = SQUARE in the schema's variant order.
        plugin
            .set_setting("mode", json!(2))
            .expect("index accepted");
        assert_eq!(plugin.mode, Mode::Square);
        assert_eq!(plugin.get_setting("mode"), Some(json!(2)));
        // Port: index 1 = "mock" (variants start with auto, mock).
        plugin
            .set_setting("port", json!(1))
            .expect("index accepted");
        assert_eq!(plugin.port_hint, "mock");
        assert_eq!(plugin.get_setting("port"), Some(json!(1)));
        // Out-of-range indices are visible errors, not silent no-ops.
        assert!(plugin.set_setting("mode", json!(99)).is_err());
        assert!(plugin.set_setting("method", json!(99)).is_err());
        // String names keep working (tests, saved configs).
        plugin
            .set_setting("mode", json!("SINE"))
            .expect("name accepted");
        assert_eq!(plugin.mode, Mode::Sine);
        plugin
            .set_setting("method", json!("MANUAL"))
            .expect("method name accepted");
        assert_eq!(plugin.method, DriveMethod::Manual);
    }

    #[test]
    fn method_switches_only_its_settings_block() {
        let mut plugin = live_plugin();
        let keys = |plugin: &StageAModulationPlugin| {
            plugin.settings_schema().sections[0]
                .items
                .iter()
                .map(|item| item.key.clone())
                .collect::<Vec<_>>()
        };

        let manual = keys(&plugin);
        assert_eq!(
            &manual[..6],
            [
                "port",
                "connect",
                "max_level",
                "method",
                "mode",
                "frequency_hz"
            ]
        );
        assert!(manual.iter().any(|key| key == "level"));
        assert!(manual.iter().any(|key| key == "min_level"));
        assert!(!manual.iter().any(|key| key == "depth_a"));
        assert!(!manual.iter().any(|key| key == "operating_point"));
        assert!(!manual.iter().any(|key| key == "v_null_dac"));
        assert!(!manual.iter().any(|key| key == "v_peak_dac"));

        let schema = plugin.settings_schema();
        let mode = schema.sections[0]
            .items
            .iter()
            .find(|item| item.key == "mode")
            .expect("mode setting");
        match &mode.kind {
            SettingKind::Enum { variants, .. } => {
                assert_eq!(variants.len(), 5, "all modes stay available");
            }
            other => panic!("mode must be an enum, got {other:?}"),
        }

        plugin.last_error = Some("stale".into());
        plugin.set_setting("method", json!(1)).unwrap();
        assert!(
            plugin.last_error.is_none(),
            "method change clears stale errors"
        );
        let calibrated = keys(&plugin);
        assert_eq!(
            &calibrated[..6],
            [
                "port",
                "connect",
                "max_level",
                "method",
                "mode",
                "frequency_hz"
            ]
        );
        assert!(!calibrated.iter().any(|key| key == "level"));
        assert!(!calibrated.iter().any(|key| key == "min_level"));
        assert!(calibrated.iter().any(|key| key == "depth_a"));
        assert!(calibrated.iter().any(|key| key == "operating_point"));
        assert!(calibrated.iter().any(|key| key == "v_null_dac"));
        assert!(calibrated.iter().any(|key| key == "v_peak_dac"));
    }

    #[test]
    fn drive_method_resolves_manual_and_calibrated_bands() {
        let mut plugin = live_plugin();
        plugin.level = 1_500;
        plugin.min_level = 600;
        assert_eq!(plugin.dac_band().unwrap(), (600, 1_500, 1_500));

        plugin.method = DriveMethod::Calibrated;
        // The ±a/2 band only exists for modulating modes (Const resolves to a
        // pure hold since the full-lobe fix).
        plugin.mode = Mode::Sine;
        plugin.v_null_dac = 200;
        plugin.v_peak_dac = 1_800;
        plugin.operating_point = 0.4;
        plugin.depth_a = 0.8;
        let inversion = plugin.lobe_inversion().expect("a real lobe");
        let expected_lo = inversion
            .dac_for_u(plugin.operating_point * (-0.5 * plugin.depth_a).exp())
            .round() as i64;
        let expected_hi = inversion
            .dac_for_u(plugin.operating_point * (0.5 * plugin.depth_a).exp())
            .round() as i64;
        let expected_hold = inversion.dac_for_u(plugin.operating_point).round() as i64;
        assert_eq!(
            plugin.dac_band().unwrap(),
            (expected_lo, expected_hi, expected_hold)
        );

        // The ceiling still bites the emitted codes — but the lobe itself stays
        // valid, so a MANUAL band under the ceiling keeps working.
        plugin.max_level = expected_hi - 1;
        assert!(plugin
            .dac_band()
            .unwrap_err()
            .contains("above the max limit"));
        plugin.method = DriveMethod::Manual;
        plugin.level = plugin.max_level;
        assert!(
            plugin.dac_band().is_ok(),
            "a manual band inside the ceiling"
        );
    }

    #[test]
    fn manual_optical_drive_is_derived_from_the_slider_band() {
        let mut plugin = live_plugin();
        plugin.v_null_dac = 200;
        plugin.v_peak_dac = 1_800;
        plugin.min_level = 600;
        plugin.level = 1_500;
        plugin.depth_a = 5.0;
        plugin.operating_point = 0.9;

        for target in [
            waveform::OpticalTarget::LogSine,
            waveform::OpticalTarget::LinearSine,
        ] {
            let drive = plugin.optical_drive(target).expect("a real lobe");
            let table = plugin
                .optical_warp_table(target)
                .expect("valid manual band");
            let min = table.iter().copied().min().unwrap();
            let max = table.iter().copied().max().unwrap();
            assert!((i64::from(min) - plugin.min_level).abs() <= 1);
            assert!((i64::from(max) - plugin.level).abs() <= 1);
            assert_ne!(drive.depth_a, plugin.depth_a);
            assert_ne!(drive.operating_point, plugin.operating_point);
        }
    }

    #[test]
    fn every_mode_drives_under_both_methods() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });
        plugin.max_level = MAX_DAC_CODE;
        plugin.min_level = 600;
        plugin.level = 1_500;
        plugin.v_null_dac = 200;
        plugin.v_peak_dac = 1_800;
        plugin.operating_point = 0.4;
        plugin.depth_a = 0.8;

        for method in DriveMethod::VARIANTS {
            plugin.method = method;
            for mode in Mode::VARIANTS {
                plugin.mode = mode;
                plugin.shared.state.lock().unwrap().board_mod.clear();
                plugin.send_modulation();
                assert!(
                    plugin.last_error.is_none(),
                    "{} {}: {:?}",
                    method.name(),
                    mode.name(),
                    plugin.last_error
                );
                let expected_wave = match mode {
                    Mode::Const => "CONST",
                    Mode::Sine => "SINE",
                    Mode::Square => "SQUARE",
                    Mode::OpticalLogSine | Mode::OpticalLinearSine => "WARP",
                };
                wait_until(&plugin, Duration::from_secs(2), |owner| {
                    owner
                        .shared
                        .state
                        .lock()
                        .unwrap()
                        .board_mod
                        .starts_with(expected_wave)
                });
                let board_mod = plugin.shared.state.lock().unwrap().board_mod.clone();
                assert!(
                    board_mod.starts_with(expected_wave),
                    "{} {} produced {board_mod}",
                    method.name(),
                    mode.name()
                );
            }
        }
        plugin.disconnect();
    }

    /// min_level can never exceed the level.
    #[test]
    fn min_threshold_is_clamped_to_level() {
        let mut plugin = live_plugin();
        plugin.set_setting("level", json!(1000)).unwrap();
        plugin.set_setting("min_level", json!(3000)).unwrap();
        assert_eq!(plugin.min_level, 1000);
        plugin.set_setting("level", json!(200)).unwrap();
        assert_eq!(plugin.min_level, 200, "lowering level drags min down");
    }

    #[test]
    fn ui_mirror_never_opens_the_command_port() {
        let mut plugin = StageAModulationPlugin::default();
        plugin.port_hint = "mock".into();
        plugin.set_setting("connect", json!(true)).unwrap();
        assert!(plugin.link.is_none());
        assert!(!plugin.device_connected());
        assert!(matches!(
            plugin
                .handle_service_request(
                    &service_request(
                        &plugin,
                        1,
                        "workflow-a",
                        ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
                        None,
                    ),
                    &live_execution(),
                )
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));
    }

    /// Runs the sweep to completion against the mock board, synthesizing the
    /// light the reject-port photodiode *would* report for whatever code the
    /// board is actually holding. The fit must then recover the synthetic
    /// lobe, which makes this a ground-truth check of the whole loop:
    /// commanding, settle gating, point collection, and the fit.
    fn run_sweep_against_mock(plugin: &mut StageAModulationPlugin, v_null: f64, v_pi: f64) {
        let mut sample_index = 0_u64;
        for _ in 0..4_000 {
            let Some((code, _)) = plugin.sweep.as_ref().and_then(CalibrationSweep::current) else {
                break;
            };
            // Only report light once the board actually holds the commanded
            // code. On the bench the settle margin covers the serial
            // round-trip; here it is asserted, so a level can never be
            // attributed to a code the board had not reached.
            if plugin
                .sweep
                .as_ref()
                .is_some_and(|sweep| sweep.commanded_at_sample.is_some())
            {
                wait_until(plugin, Duration::from_secs(2), |p| {
                    board_code(p) == Some(i64::from(code))
                });
            }
            let held = board_code(plugin).unwrap_or(0) as f64;
            let u = (std::f64::consts::PI * (held - v_null) / (2.0 * v_pi))
                .sin()
                .powi(2);
            sample_index += SETTLE_SAMPLES;
            plugin.drive_calibration(
                Some(PhotodiodeLevelV1 {
                    // Reject port: brightest at the excitation null.
                    mean_volts: 2.4 - 2.2 * u,
                    peak_to_peak_volts: 0.001,
                    sample_count: SETTLE_SAMPLES,
                    end_sample_index: sample_index,
                    clipped: false,
                }),
                None,
            );
        }
    }

    #[test]
    fn sweep_recovers_a_synthetic_lobe_and_restores_the_armed_drive() {
        let mut plugin = live_plugin();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        // Arm a drive the sweep must put back afterwards.
        plugin.set_setting("level", json!(1_234)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(1_234)
        });

        plugin.set_setting("calibrate", json!(true)).unwrap();
        assert!(plugin.sweep.is_some(), "sweep started");
        run_sweep_against_mock(&mut plugin, 300.0, 1_600.0);

        assert!(plugin.sweep.is_none(), "sweep ran to completion");
        let fit = plugin.fit.as_ref().expect("produced a fit");
        assert!(
            (fit.v_null_dac - 300.0).abs() < 5.0,
            "V_null {}",
            fit.v_null_dac
        );
        assert!((fit.v_pi_dac - 1_600.0).abs() < 10.0, "Vπ {}", fit.v_pi_dac);
        assert_eq!(fit.points.len(), SWEEP_POINTS_PER_PASS * 2);

        // The armed drive is back on the board: a calibration sweep must leave
        // the bench as it found it.
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p) == Some(1_234)
        });

        // Applying writes the lobe through and publishes a calibration id.
        assert!(
            plugin.fit_warnings().is_empty(),
            "{:?}",
            plugin.fit_warnings()
        );
        plugin.set_setting("calibrate_apply", json!(true)).unwrap();
        assert_eq!(plugin.v_null_dac, 300);
        assert!((plugin.v_peak_dac - plugin.v_null_dac - 1_600).abs() <= 10);
        assert!(plugin.calibration_id.is_some());
        assert!(plugin.control_state().calibration_id.is_some());

        // And the UI mirror — the instance the host actually collects the
        // settings snapshot from — reports the applied lobe rather than its own
        // stale codes. Without this the next sync wrote the old pair straight
        // back onto the worker and the button looked like it did nothing.
        let mut mirror = StageAModulationPlugin::default();
        mirror.set_runtime_role(PluginRuntimeRole::UiMirror);
        assert_eq!(
            mirror.get_setting("v_null_dac"),
            Some(json!(plugin.v_null_dac)),
            "the mirror kept exporting a pre-calibration V_null"
        );
        assert_eq!(
            mirror.get_setting("v_peak_dac"),
            Some(json!(plugin.v_peak_dac))
        );
        // Adoption is one-way and settles: once taken on, the mirror is free to
        // be edited again without the applied lobe snapping back.
        mirror.set_setting("v_null_dac", json!(111)).unwrap();
        assert_eq!(mirror.get_setting("v_null_dac"), Some(json!(111)));
    }

    /// The host re-applies the **whole** settings snapshot on every sync, and
    /// most drive handlers push to the board unconditionally. Without a guard
    /// each sync re-arms the operator's waveform on top of the code the sweep
    /// just commanded, so every point measures the armed drive instead of the
    /// staircase and the fit sees a flat curve.
    #[test]
    fn a_settings_sync_during_a_sweep_does_not_re_arm_the_operator_drive() {
        let mut plugin = live_plugin();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        // Arm a periodic drive, as an operator would before calibrating.
        let sine = Mode::VARIANTS
            .iter()
            .position(|m| *m == Mode::Sine)
            .unwrap();
        plugin.set_setting("level", json!(3_000)).unwrap();
        plugin.set_setting("min_level", json!(1_000)).unwrap();
        plugin.set_setting("mode", json!(sine)).unwrap();
        // Let the device thread drain the armed drive, so anything still queued
        // below is something the sync put there.
        wait_until(&plugin, Duration::from_secs(2), |p| {
            p.shared.pending.lock().unwrap().is_none()
        });

        plugin.set_setting("calibrate", json!(true)).unwrap();
        assert!(plugin.sweep.is_some());

        // Exactly what `apply_live_plugin_snapshot` does: write every key back.
        let resync = |plugin: &mut StageAModulationPlugin| {
            for key in [
                "frequency_hz",
                "level",
                "max_level",
                "method",
                "min_level",
                "mode",
                "v_null_dac",
                "v_peak_dac",
            ] {
                let value = plugin.get_setting(key).expect("exported");
                plugin.set_setting(key, value).expect("re-applies");
            }
        };
        resync(&mut plugin);
        assert!(
            plugin.shared.pending.lock().unwrap().is_none(),
            "a settings sync queued a drive while the sweep owned the DAC"
        );

        // With the sync fighting it on every tick, the sweep must still see the
        // staircase and produce a usable fit.
        let mut sample_index = 0_u64;
        for _ in 0..4_000 {
            let Some((code, _)) = plugin.sweep.as_ref().and_then(CalibrationSweep::current) else {
                break;
            };
            resync(&mut plugin);
            if plugin
                .sweep
                .as_ref()
                .is_some_and(|sweep| sweep.commanded_at_sample.is_some())
            {
                wait_until(&plugin, Duration::from_secs(2), |p| {
                    board_code(p) == Some(i64::from(code))
                });
            }
            let held = board_code(&plugin).unwrap_or(0) as f64;
            let u = (std::f64::consts::PI * (held - 300.0) / 3_200.0)
                .sin()
                .powi(2);
            sample_index += SETTLE_SAMPLES;
            plugin.drive_calibration(
                Some(PhotodiodeLevelV1 {
                    mean_volts: 2.4 - 2.2 * u,
                    peak_to_peak_volts: 0.001,
                    sample_count: SETTLE_SAMPLES,
                    end_sample_index: sample_index,
                    clipped: false,
                }),
                None,
            );
        }

        let fit = plugin
            .fit
            .as_ref()
            .unwrap_or_else(|| panic!("no fit: {}", plugin.calibration_status));
        assert!(
            (fit.v_null_dac - 300.0).abs() < 5.0,
            "V_null {}",
            fit.v_null_dac
        );

        // The armed sine comes back once the sweep releases the DAC.
        wait_until(&plugin, Duration::from_secs(2), |p| {
            board_code(p).is_some_and(|code| code != 0)
        });
        assert_eq!(plugin.mode, Mode::Sine);
    }

    #[test]
    fn sweep_waits_for_a_window_measured_after_the_code_was_commanded() {
        let mut plugin = live_plugin();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        plugin.set_setting("calibrate", json!(true)).unwrap();

        let stale = |end_sample_index| PhotodiodeLevelV1 {
            mean_volts: 1.0,
            peak_to_peak_volts: 0.001,
            sample_count: 100,
            end_sample_index,
            clipped: false,
        };
        // First tick commands the point and adopts the sample index.
        plugin.drive_calibration(Some(stale(10_000)), None);
        assert_eq!(plugin.sweep.as_ref().unwrap().points.len(), 0);
        // A window that began before the command must not be accepted, however
        // many times it arrives — this is what makes settling provable.
        for _ in 0..5 {
            plugin.drive_calibration(Some(stale(10_050)), None);
        }
        assert_eq!(plugin.sweep.as_ref().unwrap().points.len(), 0);
        // Once the window starts past the settle margin the point is taken.
        plugin.drive_calibration(Some(stale(10_000 + SETTLE_SAMPLES + 100)), None);
        assert_eq!(plugin.sweep.as_ref().unwrap().points.len(), 1);
    }

    #[test]
    fn sweep_is_refused_while_automation_holds_the_lease() {
        let mut plugin = live_plugin();
        plugin.set_setting("port", json!("mock")).unwrap();
        plugin.set_setting("connect", json!(true)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        plugin.handle_service_request(
            &service_request(
                &plugin,
                1,
                "stage-a-a1",
                ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
                None,
            ),
            &live_execution(),
        );
        assert!(plugin.lease.is_some());
        // Two owners stepping the same DAC would interleave silently.
        plugin.start_calibration_sweep();
        assert!(plugin.sweep.is_none());
        assert!(
            plugin.calibration_status.contains("leased"),
            "{}",
            plugin.calibration_status
        );
    }

    fn synthetic_fit(
        span: f64,
        max_code: u16,
        edit: impl Fn(&mut calibration::SweepPoint, usize),
    ) -> calibration::TransferFit {
        let points: Vec<calibration::SweepPoint> = calibration::sweep_codes(max_code, 49, false)
            .into_iter()
            .enumerate()
            .map(|(index, (code, direction))| {
                let u = (std::f64::consts::PI * (f64::from(code) - 300.0) / 1_720.0)
                    .sin()
                    .powi(2);
                let mut point = calibration::SweepPoint {
                    code,
                    direction,
                    volts: 0.098 + span * u,
                    peak_to_peak_volts: 0.001,
                    clipped: false,
                };
                edit(&mut point, index);
                point
            })
            .collect();
        calibration::fit_transfer(
            &points,
            f64::from(max_code),
            calibration::DetectorGeometry::RejectedComplement,
        )
        .expect("fits")
    }

    /// A stray sample inflates the RMS residual several fold while leaving the
    /// fitted period accurate. Dropping the wild points keeps the reported
    /// residual describing the curve instead of the worst sample.
    #[test]
    fn a_stray_point_is_dropped_instead_of_ruining_the_fit() {
        let clean = synthetic_fit(-0.090, 4_095, |_, _| {});
        let strayed = synthetic_fit(-0.090, 4_095, |point, index| {
            if index == 20 {
                point.volts += 0.09;
            }
        });

        assert_eq!(clean.rejected_points, 0);
        assert_eq!(strayed.rejected_points, 1, "the stray should be dropped");
        assert!(
            (strayed.v_pi_dac - clean.v_pi_dac).abs() < 5.0,
            "Vpi moved from {} to {}",
            clean.v_pi_dac,
            strayed.v_pi_dac
        );
        assert!(
            strayed.quality < 0.02,
            "residual still dominated by the stray: {:.1}%",
            strayed.quality * 100.0
        );
        // The plot still shows every measured point, stray included.
        assert_eq!(strayed.points.len(), 49);
    }

    /// A poor residual is the operator's call, made against the plot — it warns
    /// but never blocks, because a stray sample can inflate it while the fitted
    /// lobe stays good. The one genuinely meaningless case, a lobe that does not
    /// fit inside the commandable range, is refused by the fit itself.
    #[test]
    fn a_scattered_or_clipped_fit_warns_but_still_applies() {
        let mut plugin = live_plugin();
        plugin.fit = Some(synthetic_fit(-0.090, 4_095, |point, index| {
            point.clipped = point.code < 100;
            if index % 7 == 0 {
                point.volts += 0.004;
            }
        }));

        let warnings = plugin.fit_warnings().join(" | ");
        assert!(
            warnings.contains("end of the detector's range"),
            "{warnings}"
        );
        // Rail-touching points are a caveat on the reported extrema, not a
        // verdict on the lobe, and the advice must not send the operator to add
        // attenuation when the detector is against its *bottom* rail.
        assert!(
            warnings.contains("V_null and V_peak are unaffected"),
            "{warnings}"
        );
        assert!(!warnings.contains("attenuation"), "{warnings}");

        plugin.set_setting("calibrate_apply", json!(true)).unwrap();
        assert!(
            plugin.calibration_id.is_some(),
            "{}",
            plugin.calibration_status
        );
        assert!(
            (plugin.v_peak_dac - plugin.v_null_dac - 860).abs() <= 10,
            "Vpi {}",
            plugin.v_peak_dac - plugin.v_null_dac
        );
    }

    /// Noisy points must not be reported as a drifting cell.
    ///
    /// Two independent passes over one curve already differ by `1.128 σ` on
    /// average, so a bare 5 % cut fires on any bench whose points are not far
    /// quieter than that — and it did, on a real recorded sweep whose cell was
    /// not drifting at all.
    #[test]
    fn hysteresis_at_the_noise_floor_is_not_reported_as_drift() {
        // A two-pass sweep of one 90 mV lobe, scattered by an alternating ±4 mV
        // that is noise and nothing else. The raw metric reads well past
        // WARN_HYSTERESIS; the passes still disagree by nothing but their own
        // noise, so there is no drift to report.
        let two_pass = |offset_descending: f64| -> calibration::TransferFit {
            let points: Vec<calibration::SweepPoint> = calibration::sweep_codes(4_095, 49, true)
                .into_iter()
                .map(|(code, direction)| {
                    let u = (std::f64::consts::PI * (f64::from(code) - 300.0) / 1_720.0)
                        .sin()
                        .powi(2);
                    let scatter = 0.012 * calibration::scatter(code, direction);
                    let drift = if direction == calibration::Direction::Descending {
                        offset_descending
                    } else {
                        0.0
                    };
                    calibration::SweepPoint {
                        code,
                        direction,
                        volts: 0.098 - 0.090 * u + scatter + drift,
                        peak_to_peak_volts: 0.001,
                        clipped: false,
                    }
                })
                .collect();
            calibration::fit_transfer(
                &points,
                4_095.0,
                calibration::DetectorGeometry::RejectedComplement,
            )
            .expect("fits")
        };

        let mut plugin = live_plugin();
        let scattered = two_pass(0.0);
        assert!(
            scattered.hysteresis.expect("both directions") > WARN_HYSTERESIS,
            "the raw metric must be past the bare threshold for this test to mean anything"
        );
        plugin.fit = Some(scattered);
        assert!(
            !plugin.fit_warnings().join(" | ").contains("drifting"),
            "{}",
            plugin.fit_warnings().join(" | ")
        );

        // A systematic offset between the passes is real drift and must still
        // be called out.
        plugin.fit = Some(two_pass(-0.030));
        assert!(
            plugin.fit_warnings().join(" | ").contains("drifting"),
            "{}",
            plugin.fit_warnings().join(" | ")
        );
    }

    fn calibration_button(plugin: &StageAModulationPlugin, key: &str) -> SettingKind {
        plugin
            .settings_schema()
            .sections
            .iter()
            .flat_map(|section| section.items.iter())
            .find(|item| item.key == key)
            .unwrap_or_else(|| panic!("{key} is in the schema"))
            .kind
            .clone()
    }

    /// The UI mirror renders the settings schema, and it never owns the device
    /// link, a lease, a sweep, or a fit. Gating `enabled` on any of those
    /// disables the buttons permanently — the operator can never start.
    #[test]
    fn calibration_buttons_are_offered_on_the_ui_mirror() {
        let mut mirror = StageAModulationPlugin::default();
        assert_eq!(mirror.runtime_role, PluginRuntimeRole::UiMirror);
        mirror.port_hint = "mock".into();

        for key in ["calibrate", "calibrate_apply"] {
            assert!(
                matches!(
                    calibration_button(&mirror, key),
                    SettingKind::Button { enabled: false }
                ),
                "{key} should be off before the operator asks to connect"
            );
        }

        mirror.set_setting("connect", json!(true)).unwrap();
        // The mirror deliberately never opens the port...
        assert!(mirror.link.is_none());
        // ...but the buttons must still be pressable, because the worker — not
        // the mirror — owns the link and enforces the real interlocks.
        for key in ["calibrate", "calibrate_apply"] {
            assert!(
                matches!(
                    calibration_button(&mirror, key),
                    SettingKind::Button { enabled: true }
                ),
                "{key} is disabled on the mirror, so it can never be pressed"
            );
        }
    }

    /// A press is transported mirror → worker as a monotonic counter. The
    /// worker adopts the counter it first sees as a baseline so a reload does
    /// not replay old presses — but that baseline must not swallow the
    /// operator's first real press.
    #[test]
    fn a_forwarded_press_reaches_a_freshly_loaded_worker() {
        let mut mirror = StageAModulationPlugin::default();
        let mut worker = live_plugin();
        worker.port_hint = "mock".into();
        worker.set_setting("connect", json!(true)).unwrap();
        wait_until(&worker, Duration::from_secs(2), |p| p.device_connected());

        // The host syncs the settings snapshot before anything is clicked.
        let sync = |worker: &mut StageAModulationPlugin, mirror: &StageAModulationPlugin| {
            let value = mirror.get_setting("calibrate").expect("exported");
            worker.set_setting("calibrate", value).unwrap();
        };
        sync(&mut worker, &mirror);
        assert!(
            worker.sweep.is_none(),
            "a plain sync must not start a sweep"
        );

        // First real click on the mirror, then the next settings sync.
        mirror.set_setting("calibrate", json!(true)).unwrap();
        sync(&mut worker, &mirror);
        assert!(
            worker.sweep.is_some(),
            "the operator's first press never reached the worker"
        );

        // Re-syncing the same counter must not re-trigger.
        sync(&mut worker, &mirror);
        assert!(worker.sweep.is_some());
        // A second click stops it, proving the toggle survives the transport.
        mirror.set_setting("calibrate", json!(true)).unwrap();
        sync(&mut worker, &mirror);
        assert!(
            worker.sweep.is_none(),
            "second press should abort the sweep"
        );
    }

    #[test]
    fn the_curve_view_shows_the_configured_lobe_before_any_measurement() {
        let mut plugin = live_plugin();
        plugin.set_setting("v_null_dac", json!(400)).unwrap();
        plugin.set_setting("v_peak_dac", json!(1_300)).unwrap();
        let curve = plugin.curve_dataset();
        // Normalised until something has actually been measured.
        assert!(curve.y_label.contains("normalised"));
        let names: Vec<&str> = curve.lines.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["configured lobe", "V_null", "V_peak"]);
        let lobe = &curve.lines[0].points;
        // Minimum at V_null, maximum one half-wave-voltage span later.
        let at = |code: f64| {
            lobe.iter()
                .min_by(|a, b| (a.x - code).abs().total_cmp(&(b.x - code).abs()))
                .expect("sampled")
                .y
        };
        assert!(at(400.0) < 0.01, "u at V_null = {}", at(400.0));
        assert!(at(1_300.0) > 0.99, "u at V_null+Vπ = {}", at(1_300.0));
    }

    /// The bench failure of 2026-07-28: brightest light at `u = 0.5` and a
    /// null at `u = 1`, because the *code* of the maximum was entered where a
    /// distance from `V_null` was expected. With both endpoints being codes,
    /// `u` cannot turn over — `u = 1` lands on the measured maximum.
    #[test]
    fn u_rises_all_the_way_to_the_measured_maximum() {
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::Const;
        // Codes read off the bench: dimmest at 1600, brightest at 3200.
        plugin.v_null_dac = 1_600;
        plugin.v_peak_dac = 3_200;

        let truth = waveform::LobeInversion {
            v_null_dac: 1_600.0,
            v_pi_dac: 1_600.0,
        };
        let mut previous = f64::MIN;
        for step in 1..=100 {
            plugin.operating_point = f64::from(step) / 100.0;
            let (_, _, hold) = plugin.dac_band().expect("every u is drivable");
            let light = truth.u_for_dac(hold as f64);
            assert!(
                light >= previous - 1e-6,
                "light turned over at u = {}: {light}",
                plugin.operating_point
            );
            previous = light;
        }
        assert!(previous > 0.999, "u = 1 is not the maximum: {previous}");
        plugin.operating_point = 1.0;
        assert_eq!(plugin.dac_band().unwrap().2, 3_200);
    }

    #[test]
    fn a_stored_quarter_wave_migrates_to_the_peak_code() {
        // Configs written before the endpoint form hold `v_pi_dac`, a distance.
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.v_null_dac = 1_630;
        plugin
            .set_setting("v_pi_dac", json!(860))
            .expect("migrates");
        assert_eq!(plugin.v_peak_dac, 2_490);
        // And it is gone from the schema, so nothing new is authored against it.
        let keys: Vec<String> = plugin
            .settings_schema()
            .sections
            .iter()
            .flat_map(|section| section.items.iter().map(|item| item.key.clone()))
            .collect();
        assert!(!keys.iter().any(|key| key == "v_pi_dac"));
    }

    #[test]
    fn the_status_pane_spells_out_where_u_lands() {
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.v_null_dac = 1_630;
        plugin.v_peak_dac = 2_490;
        let status = plugin
            .status_entries()
            .iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(status.contains("half-wave span 860 codes"), "{status}");
        assert!(status.contains("0 → 1630"), "{status}");
        assert!(status.contains("1 → 2490"), "{status}");

        // A pair entered running downward is reported as folded, not silently
        // driven on a branch the operator did not name.
        plugin.v_null_dac = 3_000;
        plugin.v_peak_dac = 2_000;
        let status = plugin
            .status_entries()
            .iter()
            .filter_map(|entry| match entry {
                StatusEntry::Text(text) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(status.contains("folded"), "{status}");
        assert!(status.contains("1 → 2000"), "{status}");
    }

    #[test]
    fn calibrated_const_hold_spans_the_full_lobe_without_a_headroom() {
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::Const;
        plugin.v_null_dac = 1_630;
        plugin.v_peak_dac = 2_490;
        plugin.depth_a = 0.5; // must be irrelevant for a constant hold

        // u = 1 holds exactly at V_null + Vπ (previously rejected because
        // the modulated band u·e^{a/2} > 1 was demanded even for CONST).
        plugin.operating_point = 1.0;
        let (lo, hi, hold) = plugin.dac_band().expect("full-lobe hold");
        assert_eq!((lo, hi, hold), (2_490, 2_490, 2_490));

        // The user's measured low point: dac_for_u(0.01) ≈ 1685.
        plugin.operating_point = 0.01;
        let (_, _, hold) = plugin.dac_band().expect("low hold");
        assert_eq!(hold, 1_685);

        // Modulating modes still require the ±a/2 headroom.
        plugin.mode = Mode::Sine;
        plugin.operating_point = 1.0;
        assert!(plugin.dac_band().is_err());
    }

    #[test]
    fn optical_linear_sine_uses_linear_not_logarithmic_headroom() {
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::OpticalLinearSine;
        plugin.v_null_dac = 400;
        plugin.v_peak_dac = 1_300;
        plugin.operating_point = 0.4;
        plugin.depth_a = 2.0;

        // Linear target: u_hi = 0.4 * (1 + tanh(1)) ≈ 0.705, which fits.
        // Reusing log-sine endpoints would incorrectly test
        // 0.4 * exp(1) ≈ 1.087 and reject this valid drive.
        let (_, hi, _) = plugin.dac_band().expect("linear target fits the lobe");
        let expected = plugin
            .lobe_inversion()
            .expect("a real lobe")
            .dac_for_u(0.4 * (1.0 + 1.0_f64.tanh()))
            .round() as i64;
        assert_eq!(hi, expected);
    }

    #[test]
    fn control_state_publishes_exact_optical_drive_provenance() {
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::OpticalLogSine;
        plugin.v_null_dac = 400;
        plugin.v_peak_dac = 1_300;
        plugin.operating_point = 0.4;
        plugin.depth_a = 1.0;
        plugin.calibration_id = Some("cal-test".into());

        let state = plugin.control_state();
        let drive = state.optical_drive.expect("optical provenance");
        assert_eq!(drive.target, OpticalTargetV1::LogSine);
        assert_eq!(drive.requested_mean_u_milli, 400);
        assert_eq!(
            drive.internal_u_milli,
            (waveform::log_sine_geometric_pedestal(0.4, 1.0) * 1_000.0).round() as u32
        );
        assert_eq!(
            drive.resolved_mean_u_milli,
            (waveform::log_sine_cycle_mean(f64::from(drive.internal_u_milli) / 1_000.0, 1.0,)
                * 1_000.0)
                .round() as u32
        );
        assert_eq!(drive.depth_a_milli, 1_000);
        assert_eq!(drive.v_null_dac, 400);
        assert_eq!(drive.v_peak_dac, 1_300);
        assert_eq!(state.calibration_id.as_deref(), Some("cal-test"));
    }

    #[test]
    fn applied_lobe_is_published_without_an_armed_optical_point() {
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::Const;
        plugin.v_null_dac = 400;
        plugin.v_peak_dac = 1_300;
        plugin.calibration_id = Some("cal-test".into());

        let state = plugin.control_state();
        assert!(state.optical_drive.is_none());
        let lobe = state.optical_lobe.expect("applied optical lobe");
        assert_eq!(lobe.calibration_id, "cal-test");
        assert_eq!(lobe.v_null_dac, 400);
        assert_eq!(lobe.v_peak_dac, 1_300);
    }

    #[test]
    fn an_out_of_range_operating_point_is_clamped_not_rejected() {
        // Refusing the edit and snapping the control back is what made these
        // two coupled controls feel broken: the operator was told a value they
        // were not editing was wrong, with no indication of where the boundary
        // is. The edit lands on the boundary instead, and the drive stays
        // arm-able the whole time.
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::Sine;
        plugin.v_null_dac = 1_630;
        plugin.v_peak_dac = 2_490;
        plugin.depth_a = 0.5;
        plugin.operating_point = 0.5;

        plugin
            .set_setting("operating_point", json!(1.0))
            .expect("an out-of-range operating point is still accepted");
        let range = plugin.achievable().expect("calibrated range");
        assert!(
            (plugin.operating_point - range.max_mean_u).abs() < 1e-9,
            "ū landed on {} rather than the boundary {}",
            plugin.operating_point,
            range.max_mean_u
        );
        // Clamped, so the drive is buildable — no stale rejection left behind.
        assert!(plugin.drive_command().is_ok());
        assert!(plugin.last_error.is_none(), "{:?}", plugin.last_error);

        // CONST modulates nothing, so the full lobe is reachable again.
        plugin.set_setting("mode", json!(0)).expect("CONST");
        plugin
            .set_setting("operating_point", json!(1.0))
            .expect("CONST maps u directly");
        assert_eq!(plugin.dac_band().unwrap(), (2_490, 2_490, 2_490));
    }

    #[test]
    fn every_mode_stays_selectable_whatever_the_parameters_are() {
        // The reported bug: with a leftover `a` from another lobe, selecting an
        // optical mode snapped the dropdown back. A mode is a statement of
        // intent — it always takes, and the parameters follow it.
        let mut plugin = live_plugin();
        plugin.method = DriveMethod::Calibrated;
        plugin.v_null_dac = 1_630;
        plugin.v_peak_dac = 2_490;
        plugin.operating_point = 1.0;
        plugin.depth_a = 6.0;

        for index in 0..Mode::VARIANTS.len() {
            plugin
                .set_setting("mode", json!(index))
                .unwrap_or_else(|error| panic!("mode {index} refused: {error}"));
            assert_eq!(plugin.mode, Mode::VARIANTS[index]);
            assert!(
                plugin.drive_command().is_ok(),
                "{} left an unbuildable drive: a={} ū={}",
                plugin.mode.name(),
                plugin.depth_a,
                plugin.operating_point
            );
        }
    }

    #[test]
    fn calibrated_const_sends_the_expected_codes_to_the_board() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::Const;
        plugin.v_null_dac = 1_630;
        plugin.v_peak_dac = 2_490;

        plugin
            .set_setting("operating_point", json!(1.0))
            .expect("full lobe");
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.shared.state.lock().unwrap().board_code == Some(2_490)
        });

        plugin
            .set_setting("operating_point", json!(0.01))
            .expect("low point");
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.shared.state.lock().unwrap().board_code == Some(1_685)
        });
        plugin.disconnect();
    }

    #[test]
    fn ui_armed_drive_publishes_a_board_echo_acknowledged_target() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });
        // Arm a sine purely through the operator settings — no lease, no
        // service request. Consumers (A1) must still see the frequency.
        plugin.set_setting("mode", json!(1)).unwrap(); // Sine
        plugin.set_setting("frequency_hz", json!(5.0)).unwrap();
        plugin.set_setting("level", json!(1_000)).unwrap();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .board_mod
                .starts_with("SINE")
        });
        let snapshot = plugin.control_state();
        let target = snapshot.acknowledged.expect("board-echo target");
        assert_eq!(target.revision, SemanticRevision(0));
        match target.waveform.expect("waveform") {
            WaveformV1::Periodic {
                frequency_millihz, ..
            } => assert_eq!(frequency_millihz, 5_000),
            other => panic!("expected periodic waveform, got {other:?}"),
        }
        plugin.disconnect();
    }

    #[test]
    fn ending_a_lease_restores_the_operators_armed_optical_depth() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::OpticalLogSine;
        plugin.calibration_id = Some("cal-test".into());
        plugin.depth_a = 0.4; // what the operator armed

        let acquire = service_request(
            &plugin,
            60,
            "stage-a-a1",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());

        // Two sweep points: only the first must be remembered as "armed".
        for (id, milli) in [(61_u64, 900_u32), (62, 1_250)] {
            let point = service_request(
                &plugin,
                id,
                "stage-a-a1",
                ModulationCommandV1::SetOpticalDepth {
                    depth_a_milli: milli,
                },
                None,
            );
            let reply = plugin.handle_service_request(&point, &live_execution());
            assert!(
                matches!(reply.outcome, PluginServiceOutcome::Accepted { .. }),
                "sweep point {milli} rejected: {:?}",
                reply.outcome
            );
        }
        assert!(
            (plugin.depth_a - 1.25).abs() < 1e-9,
            "sweep drives the depth"
        );

        plugin.end_lease();
        assert!(
            (plugin.depth_a - 0.4).abs() < 1e-9,
            "armed depth not restored: {}",
            plugin.depth_a
        );
        assert!(plugin.armed_depth_a.is_none());
        plugin.disconnect();
    }

    #[test]
    fn a_decimal_frequency_echo_still_yields_millihertz() {
        // Firmware echoing "10000.0" used to parse as u64 -> None, which
        // published frequency_millihz: 0 and cost A1 its fallback period.
        let mut state = DeviceState::default();
        let mut fields = BTreeMap::new();
        fields.insert("mod_wave".to_owned(), "SINE".to_owned());
        fields.insert("mod_level".to_owned(), "2000".to_owned());
        fields.insert("mod_min".to_owned(), "100".to_owned());
        fields.insert("mod_freq_mhz".to_owned(), "10000.0".to_owned());
        apply_reply_fields(&mut state, &fields);
        assert_eq!(state.board_freq_millihz, Some(10_000));

        // The integer form keeps working.
        fields.insert("mod_freq_mhz".to_owned(), "7500".to_owned());
        apply_reply_fields(&mut state, &fields);
        assert_eq!(state.board_freq_millihz, Some(7_500));
    }

    #[test]
    fn set_optical_depth_requires_lease_and_a_calibrated_drive() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::OpticalLogSine;
        plugin.calibration_id = Some("cal-test".into());

        // Without a lease the retarget is refused.
        let unleased = service_request(
            &plugin,
            30,
            "stage-a-a1",
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: 1_250,
            },
            None,
        );
        assert!(matches!(
            plugin
                .handle_service_request(&unleased, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));

        let acquire = service_request(
            &plugin,
            31,
            "stage-a-a1",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());

        let retarget = service_request(
            &plugin,
            32,
            "stage-a-a1",
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: 1_250,
            },
            None,
        );
        let outcome = plugin
            .handle_service_request(&retarget, &live_execution())
            .outcome;
        assert!(
            matches!(outcome, PluginServiceOutcome::Accepted { .. }),
            "retarget outcome: {outcome:?}"
        );
        assert!((plugin.depth_a - 1.25).abs() < 1e-9);
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .board_mod
                .starts_with("WARP")
        });

        // The manual DAC band cannot express an optical depth.
        plugin.method = DriveMethod::Manual;
        let manual = service_request(
            &plugin,
            33,
            "stage-a-a1",
            ModulationCommandV1::SetOpticalDepth {
                depth_a_milli: 1_000,
            },
            None,
        );
        assert!(matches!(
            plugin
                .handle_service_request(&manual, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));
        plugin.disconnect();
    }

    #[test]
    fn lease_acquire_is_idempotent_and_exclusive_without_frames() {
        let mut plugin = live_plugin();
        let acquire = service_request(
            &plugin,
            10,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        let first = plugin.handle_service_request(&acquire, &live_execution());
        let expiry = plugin.lease.as_ref().unwrap().expires_at_unix_ms;
        let duplicate = plugin.handle_service_request(&acquire, &live_execution());
        assert_eq!(first, duplicate);
        assert_eq!(plugin.lease.as_ref().unwrap().expires_at_unix_ms, expiry);
        assert!(plugin.set_setting("level", json!(1)).is_err());

        let conflict = service_request(
            &plugin,
            11,
            "workflow-b",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        assert!(matches!(
            plugin
                .handle_service_request(&conflict, &live_execution())
                .outcome,
            PluginServiceOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn prepare_safe_off_and_release_publish_terminal_ack_before_lease_loss() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });

        let acquire = service_request(
            &plugin,
            20,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());
        {
            // The mode change restates what the controller reported.
            let mut state = plugin.shared.state.lock().expect("device state lock");
            state.controller_rate_hz = Some(20_000);
            state.controller_block_samples = Some(256);
        }
        let prepare = service_request(
            &plugin,
            21,
            "workflow-a",
            ModulationCommandV1::PrepareA1,
            Some(1),
        );
        let initial = plugin.handle_service_request(&prepare, &live_execution());
        let PluginServiceOutcome::Accepted { payload } = initial.outcome else {
            panic!("prepare rejected");
        };
        let response: ModulationResponseV1 = serde_json::from_value(payload).unwrap();
        assert_eq!(response.common.outcome, RequestOutcomeV1::InProgress);
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .last_response
                .as_ref()
                .is_some_and(|response| {
                    response.common.request_id.0 == 21
                        && response.common.outcome == RequestOutcomeV1::Applied
                })
        });
        let terminal = plugin.handle_service_request(&prepare, &live_execution());
        let PluginServiceOutcome::Accepted { payload } = terminal.outcome else {
            panic!("terminal prepare rejected");
        };
        let response: ModulationResponseV1 = serde_json::from_value(payload).unwrap();
        assert_eq!(
            response.common.acknowledged_revision,
            Some(SemanticRevision(1))
        );

        let safe_off = service_request(
            &plugin,
            22,
            "workflow-a",
            ModulationCommandV1::SafeOff {
                reason: "test".into(),
            },
            Some(2),
        );
        plugin.handle_service_request(&safe_off, &live_execution());
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .acknowledged
                .as_ref()
                .is_some_and(|target| {
                    target.revision == SemanticRevision(2)
                        && target.waveform == Some(WaveformV1::Off)
                })
        });

        let release = service_request(
            &plugin,
            23,
            "workflow-a",
            ModulationCommandV1::ReleaseLease {
                safe_off: true,
                reason: "done".into(),
            },
            Some(3),
        );
        plugin.handle_service_request(&release, &live_execution());
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .last_response
                .as_ref()
                .is_some_and(|response| {
                    response.common.request_id.0 == 23
                        && response.common.outcome == RequestOutcomeV1::Applied
                })
        });
        plugin.apply_execution_context(&live_execution());
        let snapshot = plugin.control_state();
        assert!(
            snapshot.lease.is_some(),
            "terminal ACK snapshot retains lease"
        );
        let duplicate = plugin.handle_service_request(&release, &live_execution());
        let PluginServiceOutcome::Accepted { payload } = duplicate.outcome else {
            panic!("release duplicate rejected");
        };
        let response: ModulationResponseV1 = serde_json::from_value(payload).unwrap();
        assert_eq!(response.common.outcome, RequestOutcomeV1::Applied);
        plugin.apply_execution_context(&live_execution());
        assert!(plugin.lease.is_none(), "lease clears after ACK publication");
        plugin.disconnect();
    }

    #[test]
    fn lease_expiry_and_effect_revocation_fail_closed_without_frames() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.device_connected()
        });
        let acquire = service_request(
            &plugin,
            30,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());
        plugin.lease.as_mut().unwrap().expires_at_unix_ms = now_unix_ms().saturating_sub(1);
        plugin.apply_execution_context(&live_execution());
        assert!(plugin.lease.is_none());
        assert!(plugin
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("lease expired")));

        let acquire = service_request(
            &plugin,
            31,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());
        assert!(plugin.lease.is_some());
        plugin.apply_execution_context(&ExecutionContext::fail_closed());
        assert!(plugin.link.is_none());
        assert!(plugin.lease.is_none());
    }

    /// `CONFIG` answers any field it does not know with
    /// `SYNTAX unknown_config_field`, and it needs a mode *and* a rate, so the
    /// mode change restates the controller's own acquisition settings.
    #[test]
    fn a1_config_matches_the_firmware_grammar() {
        // The rate `CONFIG` carries is the controller's own portable-sampler
        // rate from STATUS, not the DMA stream rate the photodiode reads.
        let settings = ControllerAcquisition {
            rate_hz: 20_000,
            block_samples: 256,
            emit_raw: true,
            emit_summary: true,
        };
        assert_eq!(
            String::from_utf8(a1_config_command(&settings).encode(1).unwrap()).unwrap(),
            "@1 CONFIG mode=A1 rate_hz=20000 block_samples=256 raw=1 summary=1\n"
        );
    }

    #[test]
    fn a2_commands_match_the_firmware_grammar_and_use_lobe_span() {
        let config = A2AcquisitionConfigV1 {
            timing_reference: A2TimingReferenceV1::Comparator,
            mean_u_milli: 300,
            depth_a_milli: 450,
            frequency_millihz: 500,
            min_half_us: 100_000,
            v_null_dac: 100,
            v_peak_dac: 1_000,
            comparator_threshold_dac: 1_500,
            comparator_hysteresis: 1,
            comparator_invert: true,
            sample_rate_hz: 500_000,
            block_samples: 256,
            emit_raw_samples: true,
            emit_summary: true,
        };
        validate_a2_configuration(&config).unwrap();
        assert_eq!(
            String::from_utf8(a2_config_command(&config).encode(1).unwrap()).unwrap(),
            "@1 CONFIG mode=A2 rate_hz=20000 block_samples=256 raw=1 summary=1\n"
        );
        assert_eq!(
            String::from_utf8(a2_comparator_command(&config).encode(2).unwrap()).unwrap(),
            "@2 CMP thr=1500 hyst=1 invert=1\n"
        );
        let drive = String::from_utf8(a2_log_square_command(&config).encode(3).unwrap()).unwrap();
        assert!(drive.contains("wave=LOG_SQUARE"));
        assert!(drive.contains("v_null=100 v_pi=900"), "{drive}");
        assert!(drive.contains("min_half_us=100000"), "{drive}");

        let mut unsafe_threshold = config;
        unsafe_threshold.comparator_threshold_dac = 0;
        assert!(validate_a2_configuration(&unsafe_threshold).is_err());
    }
    #[test]
    fn a2_accepts_the_actual_firmware_trigger_names_and_rejects_wrong_modes() {
        let mut fields = BTreeMap::from([
            ("trigger_source".into(), "PD_COMPARATOR".into()),
            ("cmp_armed".into(), "1".into()),
            ("mod_wave".into(), "LOG_SQUARE".into()),
        ]);
        assert!(verify_a2_prepared(&fields, A2TimingReferenceV1::Comparator).is_ok());
        assert!(verify_a2_prepared(&fields, A2TimingReferenceV1::DriveSync).is_err());
        fields.insert("trigger_source".into(), "J24_PHASE0".into());
        fields.insert("cmp_armed".into(), "0".into());
        fields.insert("mod_wave".into(), "SQUARE".into());
        assert!(verify_a2_prepared(&fields, A2TimingReferenceV1::DriveSync).is_ok());
        assert!(verify_a2_prepared(&fields, A2TimingReferenceV1::Comparator).is_err());
    }

    #[test]
    fn drive_sync_a2_prepares_with_real_owner_and_mock_transport_without_start() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |p| p.device_connected());
        let acquire = service_request(
            &plugin,
            80,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&acquire, &live_execution());
        let prepare = service_request(
            &plugin,
            81,
            "workflow-a",
            ModulationCommandV1::PrepareA2 {
                configuration: A2AcquisitionConfigV1 {
                    timing_reference: A2TimingReferenceV1::DriveSync,
                    mean_u_milli: 300,
                    depth_a_milli: 450,
                    frequency_millihz: 500,
                    min_half_us: 0,
                    v_null_dac: 100,
                    v_peak_dac: 1000,
                    comparator_threshold_dac: 0,
                    comparator_hysteresis: 1,
                    comparator_invert: false,
                    sample_rate_hz: 500_000,
                    block_samples: 256,
                    emit_raw_samples: true,
                    emit_summary: true,
                },
            },
            Some(1),
        );
        plugin.handle_service_request(&prepare, &live_execution());
        wait_until(&plugin, Duration::from_secs(2), |p| {
            p.shared
                .state
                .lock()
                .unwrap()
                .last_response
                .as_ref()
                .is_some_and(|r| {
                    r.common.request_id.0 == 81 && r.common.outcome != RequestOutcomeV1::InProgress
                })
        });
        let state = plugin.shared.state.lock().unwrap();
        let response = state.last_response.as_ref().unwrap();
        assert_eq!(
            response.common.outcome,
            RequestOutcomeV1::Applied,
            "{response:?}"
        );
        assert!(response.marker_diagnostics.unwrap().dma_sample_clock);
        assert_eq!(state.controller_state, ControllerStateV1::Configured);
        assert_eq!(state.board_wave.as_deref(), Some("SQUARE"));
    }
    /// A2's drive-synchronized capture configures A1 mode at its own sample
    /// rate, and the firmware keeps an armed comparator until a `CONFIG`
    /// leaves A2 — so "the controller already says A1" proves nothing about
    /// what the acquisition runs with. Preparation always restates it.
    #[test]
    fn a1_preparation_always_restates_the_configuration() {
        let settings = ControllerAcquisition {
            rate_hz: 500_000,
            block_samples: 2_048,
            emit_raw: true,
            emit_summary: true,
        };
        let rendered: Vec<String> = a1_prepare_commands(&settings)
            .iter()
            .map(|command| String::from_utf8(command.encode(1).expect("encodes")).unwrap())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "@1 STOP reason=prepare_a1\n".to_string(),
                "@1 CONFIG mode=A1 rate_hz=500000 block_samples=2048 raw=1 summary=1\n".into(),
                "@1 START\n".into(),
                "@1 STATUS\n".into(),
            ]
        );
    }

    #[test]
    fn queued_mode_and_drive_commands_all_reach_the_controller_and_remain_queryable() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .controller_rate_hz
                .is_some()
        });
        plugin.method = DriveMethod::Calibrated;
        plugin.mode = Mode::OpticalLogSine;
        plugin.calibration_id = Some("test-lobe".into());
        let lease = service_request(
            &plugin,
            200,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&lease, &live_execution());
        let requests = [
            service_request(
                &plugin,
                201,
                "workflow-a",
                ModulationCommandV1::PrepareA1,
                Some(1),
            ),
            service_request(
                &plugin,
                202,
                "workflow-a",
                ModulationCommandV1::SetDriveFrequency {
                    frequency_millihz: 5_000,
                },
                None,
            ),
            service_request(
                &plugin,
                203,
                "workflow-a",
                ModulationCommandV1::SetOpticalDepth { depth_a_milli: 500 },
                None,
            ),
        ];
        for request in &requests {
            let reply = plugin.handle_service_request(request, &live_execution());
            let PluginServiceOutcome::Accepted { payload } = reply.outcome else {
                panic!("queue rejected");
            };
            let response: ModulationResponseV1 = serde_json::from_value(payload).unwrap();
            assert_eq!(response.common.outcome, RequestOutcomeV1::InProgress);
        }
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner.shared.completions.lock().unwrap().len() == 3
        });
        for request in &requests {
            let reply = plugin.handle_service_request(request, &live_execution());
            let PluginServiceOutcome::Accepted { payload } = reply.outcome else {
                panic!("completion rejected");
            };
            let response: ModulationResponseV1 = serde_json::from_value(payload).unwrap();
            assert_eq!(
                response.common.outcome,
                RequestOutcomeV1::Applied,
                "{:?}",
                response.common.error
            );
        }
        let state = plugin.control_state();
        assert_eq!(state.controller_mode.as_deref(), Some("A1"));
        assert_eq!(state.controller_state, ControllerStateV1::Running);
        let target = state.acknowledged.unwrap();
        assert!(matches!(
            target.waveform,
            Some(WaveformV1::Periodic {
                frequency_millihz: 5_000,
                ..
            })
        ));
        plugin.disconnect();
    }

    #[test]
    fn firmware_rejection_is_retained_after_a_later_success() {
        let (mock, mut client) = MockService::spawn();
        let shared = SharedLink::new();
        for (id, command) in [
            (1, Command::new("NOT_A_COMMAND")),
            (2, Command::new("STATUS")),
        ] {
            let operation = PendingOperation {
                commands: vec![command],
                purpose: "TEST",
                meta: Some(OperationMeta {
                    request_id: stage_a_plugin_contract::RequestId(id),
                    requester: ClientId::new("workflow"),
                    run_id: None,
                    requested_revision: SemanticRevision(id),
                    owner_instance: OwnerInstanceId::new("test"),
                    target: ModulationTargetV1 {
                        revision: SemanticRevision(id),
                        waveform: None,
                        a2_configuration: None,
                        acquisition_running: false,
                        board_dac_code: None,
                        firmware_configuration_revision: None,
                    },
                }),
            };
            assert_eq!(execute_operation(&mut client, &shared, operation), id == 2);
        }
        let completions = shared.completions.lock().unwrap();
        assert_eq!(completions[0].1.common.outcome, RequestOutcomeV1::Rejected);
        assert_eq!(completions[1].1.common.outcome, RequestOutcomeV1::Applied);
        drop(mock);
    }
    #[test]
    fn repeated_a2_drive_sync_to_a1_switches_restart_acquisition() {
        let mut plugin = live_plugin();
        plugin.port_hint = "mock".into();
        plugin.connect_requested = true;
        plugin.connect();
        wait_until(&plugin, Duration::from_secs(2), |owner| {
            owner
                .shared
                .state
                .lock()
                .unwrap()
                .controller_rate_hz
                .is_some()
        });
        let lease = service_request(
            &plugin,
            300,
            "workflow-a",
            ModulationCommandV1::AcquireLease { ttl_ms: 10_000 },
            None,
        );
        plugin.handle_service_request(&lease, &live_execution());
        for cycle in 0..5 {
            let configuration = A2AcquisitionConfigV1 {
                timing_reference: A2TimingReferenceV1::DriveSync,
                mean_u_milli: 300,
                depth_a_milli: 450,
                frequency_millihz: 500,
                min_half_us: 100_000,
                v_null_dac: 100,
                v_peak_dac: 1_000,
                comparator_threshold_dac: 0,
                comparator_hysteresis: 1,
                comparator_invert: false,
                sample_rate_hz: 500_000,
                block_samples: 256,
                emit_raw_samples: true,
                emit_summary: true,
            };
            for (offset, command) in [
                (0, ModulationCommandV1::PrepareA2 { configuration }),
                (1, ModulationCommandV1::PrepareA1),
            ] {
                let revision = 1 + cycle * 2 + offset;
                let request = service_request(
                    &plugin,
                    300 + revision,
                    "workflow-a",
                    command,
                    Some(revision),
                );
                plugin.handle_service_request(&request, &live_execution());
                wait_until(&plugin, Duration::from_secs(2), |owner| {
                    owner
                        .shared
                        .completions
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|(_, r)| r.common.request_id.0 == 300 + revision)
                });
                let PluginServiceOutcome::Accepted { payload } = plugin
                    .handle_service_request(&request, &live_execution())
                    .outcome
                else {
                    panic!("command rejected");
                };
                let response: ModulationResponseV1 = serde_json::from_value(payload).unwrap();
                assert_eq!(
                    response.common.outcome,
                    RequestOutcomeV1::Applied,
                    "{:?}",
                    response.common.error
                );
            }
            let state = plugin.control_state();
            assert_eq!(state.controller_state, ControllerStateV1::Running);
            assert!(state.acknowledged.unwrap().a2_configuration.is_none());
        }
        plugin.disconnect();
    }
}
