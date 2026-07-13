//! Stage-A A1 — event-native Bode calibration, minimum-depth method.
//!
//! Measures `a_min(f)`: the smallest optical log-contrast that still
//! produces phase-locked events, per drive frequency. `|H(f)| =
//! C/a_min(f)`, the knee is `f_c(I)`, and the plateau of `a_min` reads out
//! the contrast quantum `C` (knowledge base:
//! `methodology/camera-calibration.md`, A1 protocol).
//!
//! Division of labour:
//! - `analysis` — phase folding, Rayleigh detection, frequency-skew
//!   recovery, phase-locked excess, probit `a_min` fit, hot-pixel mask;
//! - `sweep` — the per-frequency bisection/grid state machine;
//! - this module — device I/O through `stage-a-io` (gated by the ABI v5
//!   execution context), camera-event intake, measurement windows, live
//!   views, and the run sidecar.
//!
//! ON and OFF are measured **separately** (never pooled — the paths are
//! asymmetric); select the polarity in the settings and run each sweep.

mod analysis;
mod sweep;

use std::collections::BTreeMap;
use std::path::PathBuf;

use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostActionDescriptor, HostActionRequestQueue, HostActionScope,
    HostContext, HostDatasetDescriptor, HostDatasetKind, HostOutput, HostViewDescriptor,
    HostViewKind, HostViewPlacement, HostViewRegistry, Plugin, PluginFrame, PluginInput,
    Series1dLine, Series1dPoint, Series1dV1, SettingItem, SettingKind, SettingsSchema,
    SettingsSection, StatusEntry, TableColumn, TableColumnData, TableColumnValues, TableDatasetV1,
    TableSchema, TableValueType, CTX_INVESTIGATION_ACTION_REQUESTS,
};
use serde_json::{json, Value};
use stage_a_io::{
    estimate_contrast, AdcCalibration, Command, DeviceEvent, FrameType, IoWorker, PdqWriter,
    RunSidecar, StageAClient, StreamIntegrity, TriggerSource, WorkerOutput, WorkerRequest,
};

use analysis::{
    detect, fold_phases, fold_phases_with_fiducials, phase_histogram, phase_locked_excess,
    rayleigh_test, refine_frequency, HotPixelMask, PhaseHistogram,
};
use sweep::{Measurement, SweepCommand, SweepEngine, SweepPlan};

const AMIN_DATASET_ID: &str = "stage-a-a1.amin";
const PHASE_DATASET_ID: &str = "stage-a-a1.phase";
const DEPTH_DATASET_ID: &str = "stage-a-a1.depth";
const STATUS_DATASET_ID: &str = "stage-a-a1.status";

const ACTION_ARM: &str = "stage-a-a1.arm";
const ACTION_RUN: &str = "stage-a-a1.run";
const ACTION_STOP: &str = "stage-a-a1.stop";

const PHASE_BINS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Idle,
    Armed,
    Reference,
    Sweeping,
    Finished,
}

/// Camera-side accumulation for the current measurement window.
#[derive(Default)]
struct WindowAccumulator {
    /// Camera timestamps of polarity-selected, hot-pixel-filtered events.
    event_timestamps_us: Vec<u64>,
    /// Rising phase-0 trigger edges (cycle fiducials) inside the window.
    trigger_edges_us: Vec<u64>,
    /// ADC codes streamed by the Teensy during the window.
    adc_codes: Vec<u16>,
    window_start_us: Option<u64>,
    latest_camera_ts_us: u64,
}

impl WindowAccumulator {
    fn clear(&mut self) {
        self.event_timestamps_us.clear();
        self.trigger_edges_us.clear();
        self.adc_codes.clear();
        self.window_start_us = None;
    }

    fn elapsed_us(&self) -> u64 {
        self.window_start_us
            .map(|start| self.latest_camera_ts_us.saturating_sub(start))
            .unwrap_or(0)
    }
}

pub struct StageAA1Plugin {
    enabled: bool,
    state: RunState,
    // device
    worker: Option<IoWorker>,
    next_tag: u64,
    in_flight: BTreeMap<u64, String>,
    firmware: String,
    integrity: StreamIntegrity,
    last_error: Option<String>,
    effects_blocked_reason: Option<String>,
    // configuration (settings)
    port_hint: String,
    polarity_on: bool,
    freq_start_hz: f64,
    freq_stop_hz: f64,
    points_per_decade: i64,
    cycles_per_measurement: i64,
    settle_ms: i64,
    alpha: f64,
    initial_amplitude_dac: i64,
    sample_rate_hz: i64,
    calibration: AdcCalibration,
    // run
    engine: Option<SweepEngine>,
    window: WindowAccumulator,
    settle_until_us: Option<u64>,
    hot_pixels: Option<HotPixelMask>,
    reference_counts: Vec<u32>,
    sensor_size: (u16, u16),
    run_id: String,
    pdq: Option<PdqWriter>,
    current_phase_histogram: Option<PhaseHistogram>,
    used_hardware_fiducial: bool,
    dataset_generation: u64,
    consumed_action_ids: Vec<u64>,
}

impl Default for StageAA1Plugin {
    fn default() -> Self {
        Self {
            enabled: false,
            state: RunState::Idle,
            worker: None,
            next_tag: 1,
            in_flight: BTreeMap::new(),
            firmware: String::new(),
            integrity: StreamIntegrity::default(),
            last_error: None,
            effects_blocked_reason: None,
            port_hint: "auto".into(),
            polarity_on: true,
            freq_start_hz: 100.0,
            freq_stop_hz: 50_000.0,
            points_per_decade: 6,
            cycles_per_measurement: 400,
            settle_ms: 100,
            alpha: 0.001,
            initial_amplitude_dac: 512,
            sample_rate_hz: 20_000,
            calibration: AdcCalibration::default(),
            engine: None,
            window: WindowAccumulator::default(),
            settle_until_us: None,
            hot_pixels: None,
            reference_counts: Vec::new(),
            sensor_size: (0, 0),
            run_id: String::new(),
            pdq: None,
            current_phase_histogram: None,
            used_hardware_fiducial: false,
            dataset_generation: 0,
            consumed_action_ids: Vec::new(),
        }
    }
}

impl StageAA1Plugin {
    fn bump(&mut self) {
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
    }

    fn frequency_grid(&self) -> Vec<f64> {
        let start = self.freq_start_hz.max(1.0);
        let stop = self.freq_stop_hz.max(start * 1.01);
        let per_decade = self.points_per_decade.max(1) as f64;
        let decades = (stop / start).log10();
        let n = (decades * per_decade).ceil() as usize + 1;
        (0..n)
            .map(|i| start * 10f64.powf(i as f64 / per_decade))
            .filter(|&f| f <= stop * 1.0001)
            .collect()
    }

    fn queue_command(&mut self, purpose: &str, command: Command) {
        let Some(worker) = &self.worker else {
            self.last_error = Some(format!("{purpose}: no device connection"));
            return;
        };
        let tag = self.next_tag;
        self.next_tag += 1;
        match worker.try_send(WorkerRequest::Send { tag, command }) {
            Ok(()) => {
                self.in_flight.insert(tag, purpose.to_owned());
            }
            Err(err) => self.last_error = Some(format!("{purpose}: {err}")),
        }
    }

    fn arm(&mut self) {
        if self.worker.is_some() {
            return;
        }
        match open_transport(&self.port_hint) {
            Ok(client) => {
                self.worker = Some(IoWorker::spawn(client));
                self.queue_command("hello", Command::new("HELLO").field("protocol", 1));
                self.state = RunState::Armed;
                self.last_error = None;
            }
            Err(err) => self.last_error = Some(err),
        }
        self.bump();
    }

    fn start_run(&mut self) {
        if self.worker.is_none() {
            self.last_error = Some("run: arm the controller first".into());
            return;
        }
        self.run_id = format!(
            "A1-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
        let pdq_path = run_data_dir().join(format!("{}.pdq", self.run_id));
        match PdqWriter::create(&pdq_path) {
            Ok(writer) => self.pdq = Some(writer),
            Err(err) => {
                self.last_error = Some(format!("pdq: {err}"));
                return;
            }
        }
        let plan = SweepPlan {
            frequencies_hz: self.frequency_grid(),
            initial_amplitude_dac: self.initial_amplitude_dac.clamp(1, 2_047) as u32,
            ..SweepPlan::default()
        };
        self.engine = Some(SweepEngine::new(plan));
        self.reference_counts.clear();
        self.hot_pixels = None;
        self.window.clear();
        self.settle_until_us = None;
        // Reference phase: unmodulated field (amplitude 0) for the
        // hot-pixel mask and the background sanity check.
        self.send_drive(self.freq_start_hz, 0, "reference");
        self.state = RunState::Reference;
        if let Some(worker) = &self.worker {
            let _ = worker.try_send(WorkerRequest::SetPinging(true));
        }
        self.bump();
    }

    fn stop_run(&mut self, reason: &str) {
        self.queue_command("stop", Command::new("STOP").field("reason", reason));
        if let Some(worker) = &self.worker {
            let _ = worker.try_send(WorkerRequest::SetPinging(false));
        }
        self.finish_run();
        self.state = if self.worker.is_some() {
            RunState::Armed
        } else {
            RunState::Idle
        };
        self.bump();
    }

    fn disarm(&mut self, reason: &str) {
        if let Some(worker) = self.worker.take() {
            worker.shutdown(reason);
        }
        self.finish_run();
        self.in_flight.clear();
        self.state = RunState::Idle;
        self.bump();
    }

    fn finish_run(&mut self) {
        if let Some(pdq) = self.pdq.take() {
            match pdq.finish(self.integrity) {
                Ok(summary) => {
                    let mut sidecar = RunSidecar::from_pdq(&self.run_id, "A1", &summary);
                    sidecar.plugin_name = "stage-a-a1".into();
                    sidecar.plugin_version = env!("CARGO_PKG_VERSION").into();
                    sidecar.firmware_version = self.firmware.clone();
                    sidecar.adc_calibration = self.calibration.clone();
                    sidecar.configured_sample_rate_hz = self.sample_rate_hz as u32;
                    sidecar.trigger_source = if self.used_hardware_fiducial {
                        TriggerSource::DrivePhase0
                    } else {
                        TriggerSource::None
                    };
                    sidecar.valid = summary.valid;
                    if let Some(mask) = &self.hot_pixels {
                        sidecar
                            .notes
                            .push(format!("hot pixels masked: {}", mask.masked_count()));
                    }
                    let sidecar_path = run_data_dir().join(format!("{}.json", self.run_id));
                    if let Err(err) = sidecar.write_json(&sidecar_path) {
                        self.last_error = Some(format!("sidecar: {err}"));
                    }
                    if let Some(engine) = &self.engine {
                        let results_path =
                            run_data_dir().join(format!("{}.results.json", self.run_id));
                        let _ = std::fs::write(
                            &results_path,
                            serde_json::to_vec_pretty(&results_json(engine)).unwrap_or_default(),
                        );
                    }
                }
                Err(err) => self.last_error = Some(format!("pdq finish: {err}")),
            }
        }
    }

    fn send_drive(&mut self, frequency_hz: f64, amplitude_dac: u32, purpose: &str) {
        let freq_mhz = (frequency_hz * 1_000.0).round() as i64;
        self.queue_command(
            purpose,
            Command::new("CONFIG")
                .field("mode", "A1")
                .field("wave", "SINE")
                .field("freq_mhz", freq_mhz)
                .field("center_dac", 2_048)
                .field("amplitude_dac", amplitude_dac)
                .field("rate_hz", self.sample_rate_hz)
                .field("block_samples", 256)
                .field("raw", 1)
                .field("summary", 1),
        );
        self.queue_command("start", Command::new("START"));
        self.window.clear();
        self.settle_until_us = None; // set on the first camera frame seen
        self.current_phase_histogram = None;
    }

    fn drain_worker(&mut self) {
        let Some(worker) = &self.worker else {
            return;
        };
        let outputs = worker.drain_outputs();
        let mut stopped = None;
        for output in outputs {
            match output {
                WorkerOutput::Reply { tag, result } => {
                    let purpose = self.in_flight.remove(&tag).unwrap_or_default();
                    match result {
                        Ok(fields) => {
                            if purpose == "hello" {
                                self.firmware = fields
                                    .get("firmware")
                                    .cloned()
                                    .unwrap_or_else(|| "unknown".into());
                            }
                        }
                        Err(err) => self.last_error = Some(format!("{purpose}: {err}")),
                    }
                }
                WorkerOutput::Event(DeviceEvent::Data(frame)) => {
                    if let Some(pdq) = &mut self.pdq {
                        let _ = pdq.write_frame(&frame);
                    }
                    if frame.header.frame_type == FrameType::SamplesU16 {
                        if let Some(codes) = frame.samples() {
                            self.window.adc_codes.extend_from_slice(&codes);
                        }
                    }
                }
                WorkerOutput::Event(DeviceEvent::Async { .. }) => {}
                WorkerOutput::Integrity(integrity) => self.integrity = integrity,
                WorkerOutput::Stopped { reason } => stopped = Some(reason),
            }
        }
        if let Some(reason) = stopped {
            self.worker = None;
            self.last_error = Some(format!("device connection ended: {reason}"));
            self.finish_run();
            self.state = RunState::Idle;
            self.bump();
        }
    }

    fn ingest_camera_frame(&mut self, frame: &PluginFrame<'_>) {
        self.sensor_size = (frame.width(), frame.height());
        self.window.latest_camera_ts_us = frame.window_end_us();
        if self.settle_until_us.is_none() {
            self.settle_until_us =
                Some(frame.window_end_us() + (self.settle_ms.max(0) as u64) * 1_000);
            return;
        }
        let settle_until = self.settle_until_us.unwrap_or(0);
        if frame.window_end_us() < settle_until {
            return;
        }
        self.window
            .window_start_us
            .get_or_insert(frame.window_start_us());

        if self.state == RunState::Reference {
            if self.reference_counts.len() != frame.width() as usize * frame.height() as usize {
                self.reference_counts = vec![0; frame.width() as usize * frame.height() as usize];
            }
            for event in frame.events() {
                let idx = event.y as usize * frame.width() as usize + event.x as usize;
                if let Some(slot) = self.reference_counts.get_mut(idx) {
                    *slot += 1;
                }
            }
        } else {
            let mask = self.hot_pixels.as_ref();
            for event in frame.events() {
                if event.is_on() != self.polarity_on {
                    continue;
                }
                if mask.is_some_and(|m| m.is_masked(event.x, event.y)) {
                    continue;
                }
                self.window.event_timestamps_us.push(event.timestamp_us());
            }
        }
        for trigger in frame.external_triggers() {
            if trigger.is_rising() {
                self.window.trigger_edges_us.push(trigger.timestamp_us);
            }
        }
    }

    fn window_target_us(&self, frequency_hz: f64) -> u64 {
        ((self.cycles_per_measurement.max(10) as f64 / frequency_hz) * 1.0e6) as u64
    }

    fn advance_run(&mut self) {
        match self.state {
            RunState::Reference => {
                // A fixed 0.5 s of unmodulated reference.
                if self.window.elapsed_us() < 500_000 {
                    return;
                }
                let (width, height) = self.sensor_size;
                if width > 0 && !self.reference_counts.is_empty() {
                    self.hot_pixels = Some(HotPixelMask::from_reference_counts(
                        width,
                        height,
                        &self.reference_counts,
                    ));
                }
                self.state = RunState::Sweeping;
                let Some(engine) = &self.engine else {
                    return;
                };
                if let SweepCommand::Measure {
                    frequency_hz,
                    amplitude_dac,
                } = engine.current_command()
                {
                    self.send_drive(frequency_hz, amplitude_dac, "sweep");
                }
                self.bump();
            }
            RunState::Sweeping => {
                let Some(engine) = &self.engine else {
                    return;
                };
                let SweepCommand::Measure {
                    frequency_hz,
                    amplitude_dac,
                } = engine.current_command()
                else {
                    self.state = RunState::Finished;
                    self.finish_run();
                    self.bump();
                    return;
                };
                if self.window.elapsed_us() < self.window_target_us(frequency_hz) {
                    return;
                }
                let measurement = self.evaluate_window(frequency_hz, amplitude_dac);
                let next = {
                    let engine = self.engine.as_mut().expect("engine exists");
                    engine.ingest(measurement)
                };
                match next {
                    SweepCommand::Measure {
                        frequency_hz,
                        amplitude_dac,
                    } => self.send_drive(frequency_hz, amplitude_dac, "sweep"),
                    SweepCommand::Finished => {
                        self.queue_command("stop", Command::new("STOP").field("reason", "done"));
                        self.state = RunState::Finished;
                        self.finish_run();
                    }
                }
                self.bump();
            }
            _ => {}
        }
    }

    fn evaluate_window(&mut self, frequency_hz: f64, amplitude_dac: u32) -> Measurement {
        // Optical contrast from the photodiode trace; any estimator
        // rejection or stream fault invalidates the point.
        let measured_a = if self.integrity.is_clean() {
            estimate_contrast(&self.window.adc_codes, &self.calibration)
                .ok()
                .map(|estimate| estimate.a)
        } else {
            None
        };

        let events = &self.window.event_timestamps_us;
        let observed_cycles = self.window.elapsed_us() as f64 / 1.0e6 * frequency_hz;

        // Cycle fiducial: hardware phase-0 edges when present, otherwise
        // software frequency refinement against the events themselves.
        let (phases, trials) = if self.window.trigger_edges_us.len() >= 2 {
            self.used_hardware_fiducial = true;
            (
                fold_phases_with_fiducials(events, &self.window.trigger_edges_us),
                1,
            )
        } else if let Some(lock) = refine_frequency(events, frequency_hz, 100.0) {
            (
                fold_phases(
                    events.iter().copied(),
                    events.first().copied().unwrap_or(0),
                    lock.frequency_hz,
                ),
                lock.trials,
            )
        } else {
            (Vec::new(), 1)
        };

        let stat = rayleigh_test(&phases);
        let histogram = phase_histogram(&phases, PHASE_BINS);
        let excess = phase_locked_excess(&histogram);
        self.current_phase_histogram = Some(histogram);
        let verdict = detect(stat, excess, observed_cycles, self.alpha, trials);

        Measurement {
            amplitude_dac,
            measured_a,
            events_per_half_cycle: verdict.locked_events_per_cycle,
            detected: verdict.detected,
        }
    }

    fn consume_actions(&mut self, context: &HostContext<'_>) -> Vec<String> {
        let Ok(Some(queue)) =
            context.get::<HostActionRequestQueue>(CTX_INVESTIGATION_ACTION_REQUESTS)
        else {
            return Vec::new();
        };
        let mut consumed = Vec::new();
        for request in queue.requests {
            if self.consumed_action_ids.contains(&request.request_id)
                || !request.action_id.starts_with("stage-a-a1.")
            {
                continue;
            }
            self.consumed_action_ids.push(request.request_id);
            if self.consumed_action_ids.len() > 256 {
                self.consumed_action_ids.remove(0);
            }
            consumed.push(request.action_id);
        }
        consumed
    }

    // -- datasets --------------------------------------------------------

    fn amin_dataset(&self) -> Series1dV1 {
        let mut a_min = Vec::new();
        let mut low = Vec::new();
        let mut high = Vec::new();
        if let Some(engine) = &self.engine {
            for result in &engine.results {
                if let Some(fit) = &result.fit {
                    a_min.push(Series1dPoint {
                        x: result.frequency_hz,
                        y: fit.a_min,
                    });
                    low.push(Series1dPoint {
                        x: result.frequency_hz,
                        y: fit.a_min_low,
                    });
                    high.push(Series1dPoint {
                        x: result.frequency_hz,
                        y: fit.a_min_high,
                    });
                }
            }
        }
        Series1dV1 {
            x_label: "drive frequency [Hz]".into(),
            y_label: "a_min".into(),
            lines: vec![
                Series1dLine {
                    name: "a_min".into(),
                    points: a_min,
                },
                Series1dLine {
                    name: "CI low".into(),
                    points: low,
                },
                Series1dLine {
                    name: "CI high".into(),
                    points: high,
                },
            ],
        }
    }

    fn phase_dataset(&self) -> Series1dV1 {
        let points = self
            .current_phase_histogram
            .as_ref()
            .map(|histogram| {
                histogram
                    .bins
                    .iter()
                    .enumerate()
                    .map(|(i, &count)| Series1dPoint {
                        x: (i as f64 + 0.5) / histogram.bins.len() as f64,
                        y: f64::from(count),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Series1dV1 {
            x_label: "drive phase [cycles]".into(),
            y_label: "events".into(),
            lines: vec![Series1dLine {
                name: if self.polarity_on { "ON" } else { "OFF" }.into(),
                points,
            }],
        }
    }

    fn depth_dataset(&self) -> Series1dV1 {
        let mut points: Vec<Series1dPoint> = self
            .engine
            .as_ref()
            .map(|engine| {
                let mut all: Vec<Series1dPoint> = engine
                    .results
                    .last()
                    .map(|result| {
                        result
                            .points
                            .iter()
                            .map(|p| Series1dPoint {
                                x: p.a,
                                y: p.events_per_half_cycle,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                all.sort_by(|p, q| p.x.total_cmp(&q.x));
                all
            })
            .unwrap_or_default();
        points.dedup_by(|p, q| p.x == q.x);
        Series1dV1 {
            x_label: "measured a".into(),
            y_label: "locked events / half-cycle".into(),
            lines: vec![Series1dLine {
                name: "N(a)".into(),
                points,
            }],
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
                column("progress", "Progress"),
                column("fiducial", "Cycle fiducial"),
                column("hot_pixels", "Hot pixels"),
                column("integrity", "Integrity"),
                column("error", "Last error"),
            ],
            ..TableSchema::default()
        }
    }

    fn status_dataset(&self) -> TableDatasetV1 {
        let state = match (&self.effects_blocked_reason, self.state) {
            (Some(reason), _) => format!("locked ({reason})"),
            (None, RunState::Idle) => "idle".into(),
            (None, RunState::Armed) => format!("armed ({})", self.firmware),
            (None, RunState::Reference) => "reference window (hot-pixel mask)".into(),
            (None, RunState::Sweeping) => "sweeping".into(),
            (None, RunState::Finished) => "finished".into(),
        };
        let progress = self
            .engine
            .as_ref()
            .map(|engine| {
                format!(
                    "{}/{} frequencies",
                    engine.results.len(),
                    engine.results.len() + if engine.is_finished() { 0 } else { 1 }
                )
            })
            .unwrap_or_else(|| "—".into());
        let fiducial = if self.used_hardware_fiducial {
            "EXT_TRIGGER phase-0".to_owned()
        } else {
            "software frequency lock".to_owned()
        };
        let hot = self
            .hot_pixels
            .as_ref()
            .map(|mask| format!("{} masked", mask.masked_count()))
            .unwrap_or_else(|| "—".into());
        let integrity = if self.integrity.is_clean() {
            "clean".to_owned()
        } else {
            format!(
                "crc={} gaps={} overruns={}",
                self.integrity.crc_failures,
                self.integrity.sequence_gaps,
                self.integrity.dropped_samples
            )
        };
        let text_column = |id: &str, value: String| TableColumnData {
            column_id: id.to_owned(),
            values: TableColumnValues::String(vec![value]),
        };
        TableDatasetV1 {
            columns: vec![
                text_column("state", state),
                text_column("progress", progress),
                text_column("fiducial", fiducial),
                text_column("hot_pixels", hot),
                text_column("integrity", integrity),
                text_column("error", self.last_error.clone().unwrap_or_default()),
            ],
        }
    }
}

fn results_json(engine: &SweepEngine) -> Value {
    json!({
        "results": engine
            .results
            .iter()
            .map(|result| {
                json!({
                    "frequency_hz": result.frequency_hz,
                    "exhausted": result.exhausted,
                    "measurements": result.measurements,
                    "fit": result.fit.as_ref().map(|fit| json!({
                        "a_min": fit.a_min,
                        "a_min_low": fit.a_min_low,
                        "a_min_high": fit.a_min_high,
                        "sigma_ln_a": fit.sigma_ln_a,
                        "points_used": fit.points_used,
                    })),
                    "points": result
                        .points
                        .iter()
                        .map(|p| json!({"a": p.a, "events_per_half_cycle": p.events_per_half_cycle}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>(),
    })
}

fn run_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".augur").join("stage-a-runs")
}

fn open_transport(port_hint: &str) -> Result<StageAClient<stage_a_io::SerialTransport>, String> {
    let path = if port_hint == "auto" {
        stage_a_io::transport::available_port_names()
            .into_iter()
            .find(|name| name.contains("usbmodem") || name.contains("ttyACM"))
            .ok_or_else(|| "no USB serial device found".to_owned())?
    } else {
        port_hint.to_owned()
    };
    let transport =
        stage_a_io::SerialTransport::open(&path, 115_200, std::time::Duration::from_millis(20))
            .map_err(|err| err.to_string())?;
    Ok(StageAClient::new(transport))
}

impl Plugin for StageAA1Plugin {
    fn name(&self) -> &'static str {
        "Stage-A A1 Min-Depth"
    }

    fn description(&self) -> &'static str {
        "Event-native Bode calibration: a_min(f) via phase-locked detection, bisection, and probit fitting."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.disarm("plugin disabled");
        }
    }

    fn reset(&mut self) {
        self.window.clear();
        self.current_phase_histogram = None;
        self.bump();
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::RawEvents
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
        let execution = context.execution();
        if !execution.hardware_effects_allowed() {
            self.effects_blocked_reason =
                Some(format!("effects not allowed in {:?}", execution.mode));
            if self.worker.is_some() {
                self.disarm("execution context revoked effects");
            }
            return;
        }
        self.effects_blocked_reason = None;

        for action in self.consume_actions(context) {
            match action.as_str() {
                ACTION_ARM => self.arm(),
                ACTION_RUN => self.start_run(),
                ACTION_STOP => self.stop_run("operator"),
                _ => {}
            }
        }

        self.drain_worker();
        if matches!(self.state, RunState::Reference | RunState::Sweeping) {
            self.ingest_camera_frame(frame);
            self.advance_run();
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Sweep".into(),
                    description: Some(
                        "Frequency grid and statistics. ON and OFF are measured in separate \
                         runs — never pooled."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "freq_start_hz".into(),
                            label: "Start frequency".into(),
                            tooltip: None,
                            kind: SettingKind::F64Drag {
                                min: 1.0,
                                max: 1.0e6,
                                speed: 10.0,
                                default: self.freq_start_hz,
                            },
                        },
                        SettingItem {
                            key: "freq_stop_hz".into(),
                            label: "Stop frequency".into(),
                            tooltip: None,
                            kind: SettingKind::F64Drag {
                                min: 1.0,
                                max: 1.0e6,
                                speed: 100.0,
                                default: self.freq_stop_hz,
                            },
                        },
                        SettingItem {
                            key: "points_per_decade".into(),
                            label: "Points per decade".into(),
                            tooltip: None,
                            kind: SettingKind::I64Slider {
                                min: 2,
                                max: 12,
                                default: self.points_per_decade,
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "cycles_per_measurement".into(),
                            label: "Cycles per measurement".into(),
                            tooltip: Some(
                                "Modulation cycles integrated per amplitude point".into(),
                            ),
                            kind: SettingKind::I64Slider {
                                min: 50,
                                max: 5_000,
                                default: self.cycles_per_measurement,
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "polarity_on".into(),
                            label: "Polarity".into(),
                            tooltip: Some("Which comparator path this sweep measures".into()),
                            kind: SettingKind::Enum {
                                variants: vec!["ON".into(), "OFF".into()],
                                default: usize::from(!self.polarity_on),
                            },
                        },
                        SettingItem {
                            key: "alpha".into(),
                            label: "Significance α".into(),
                            tooltip: Some(
                                "Per-measurement false-positive budget (Bonferroni-corrected \
                                 for the frequency scan)"
                                    .into(),
                            ),
                            kind: SettingKind::F64Drag {
                                min: 1e-6,
                                max: 0.05,
                                speed: 1e-4,
                                default: self.alpha,
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Device".into(),
                    description: None,
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "initial_amplitude_dac".into(),
                            label: "Initial amplitude (DAC)".into(),
                            tooltip: None,
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 2_047,
                                default: self.initial_amplitude_dac,
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "settle_ms".into(),
                            label: "Settle time".into(),
                            tooltip: Some(
                                "Discarded after each drive change (HVA/Pockels settling + \
                                 refractory clearing)"
                                    .into(),
                            ),
                            kind: SettingKind::I64Slider {
                                min: 10,
                                max: 2_000,
                                default: self.settle_ms,
                                suffix: Some(" ms".into()),
                            },
                        },
                        SettingItem {
                            key: "dark_millivolts".into(),
                            label: "Dark level".into(),
                            tooltip: None,
                            kind: SettingKind::F64Drag {
                                min: 0.0,
                                max: 3_300.0,
                                speed: 1.0,
                                default: self.calibration.dark_volts * 1_000.0,
                            },
                        },
                    ],
                },
            ],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "freq_start_hz" => Some(json!(self.freq_start_hz)),
            "freq_stop_hz" => Some(json!(self.freq_stop_hz)),
            "points_per_decade" => Some(json!(self.points_per_decade)),
            "cycles_per_measurement" => Some(json!(self.cycles_per_measurement)),
            "polarity_on" => Some(json!(if self.polarity_on { "ON" } else { "OFF" })),
            "alpha" => Some(json!(self.alpha)),
            "initial_amplitude_dac" => Some(json!(self.initial_amplitude_dac)),
            "settle_ms" => Some(json!(self.settle_ms)),
            "dark_millivolts" => Some(json!(self.calibration.dark_volts * 1_000.0)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "freq_start_hz" => {
                self.freq_start_hz = value.as_f64().ok_or("must be a number")?.max(1.0);
            }
            "freq_stop_hz" => {
                self.freq_stop_hz = value.as_f64().ok_or("must be a number")?.max(1.0);
            }
            "points_per_decade" => {
                self.points_per_decade = value.as_i64().ok_or("must be an integer")?.clamp(2, 12);
            }
            "cycles_per_measurement" => {
                self.cycles_per_measurement =
                    value.as_i64().ok_or("must be an integer")?.clamp(50, 5_000);
            }
            "polarity_on" => {
                let text = value.as_str().ok_or("must be a string")?;
                self.polarity_on = text.eq_ignore_ascii_case("on");
            }
            "alpha" => {
                self.alpha = value.as_f64().ok_or("must be a number")?.clamp(1e-6, 0.05);
            }
            "initial_amplitude_dac" => {
                self.initial_amplitude_dac =
                    value.as_i64().ok_or("must be an integer")?.clamp(1, 2_047);
            }
            "settle_ms" => {
                self.settle_ms = value.as_i64().ok_or("must be an integer")?.clamp(10, 2_000);
            }
            "dark_millivolts" => {
                let mv = value.as_f64().ok_or("must be a number")?;
                self.calibration.dark_volts = (mv / 1_000.0).clamp(0.0, 3.3);
            }
            _ => return Err(format!("unknown setting: {key}")),
        }
        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = Vec::new();
        if let Some(reason) = &self.effects_blocked_reason {
            entries.push(StatusEntry::Text(format!("Hardware locked: {reason}")));
        }
        if let Some(engine) = &self.engine {
            entries.push(StatusEntry::Text(format!(
                "{} frequency points finished",
                engine.results.len()
            )));
        }
        if let Some(err) = &self.last_error {
            entries.push(StatusEntry::Text(format!("Error: {err}")));
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        HostViewRegistry {
            datasets: vec![
                HostDatasetDescriptor {
                    id: AMIN_DATASET_ID.into(),
                    title: "a_min(f)".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "No fitted frequency points yet.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: PHASE_DATASET_ID.into(),
                    title: "Phase histogram".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "No measurement window yet.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: DEPTH_DATASET_ID.into(),
                    title: "N(a) at current frequency".into(),
                    kind: HostDatasetKind::Series1dV1,
                    empty_message: "No depth points yet.".into(),
                    display: None,
                    relations: Vec::new(),
                },
                HostDatasetDescriptor {
                    id: STATUS_DATASET_ID.into(),
                    title: "A1 run status".into(),
                    kind: HostDatasetKind::TableV1(self.status_schema()),
                    empty_message: "Idle.".into(),
                    display: None,
                    relations: Vec::new(),
                },
            ],
            views: vec![
                HostViewDescriptor {
                    id: format!("{AMIN_DATASET_ID}.view"),
                    title: "A1 Bode (a_min)".into(),
                    dataset_id: AMIN_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: format!("{PHASE_DATASET_ID}.view"),
                    title: "Phase fold".into(),
                    dataset_id: PHASE_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: format!("{DEPTH_DATASET_ID}.view"),
                    title: "Depth staircase".into(),
                    dataset_id: DEPTH_DATASET_ID.into(),
                    placement: HostViewPlacement::Window,
                    kind: HostViewKind::LineSeriesWindow,
                },
                HostViewDescriptor {
                    id: format!("{STATUS_DATASET_ID}.view"),
                    title: "A1 status".into(),
                    dataset_id: STATUS_DATASET_ID.into(),
                    placement: HostViewPlacement::AnalysisPanel,
                    kind: HostViewKind::CompactTable,
                },
            ],
            actions: vec![
                HostActionDescriptor {
                    id: ACTION_ARM.into(),
                    title: "Arm controller".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
                HostActionDescriptor {
                    id: ACTION_RUN.into(),
                    title: "Run A1 sweep".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
                HostActionDescriptor {
                    id: ACTION_STOP.into(),
                    title: "Stop".into(),
                    scope: HostActionScope::Dataset {
                        dataset_id: STATUS_DATASET_ID.into(),
                    },
                    param_schema: None,
                },
            ],
        }
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        match dataset_id {
            AMIN_DATASET_ID => serde_json::to_vec(&self.amin_dataset()).ok(),
            PHASE_DATASET_ID => serde_json::to_vec(&self.phase_dataset()).ok(),
            DEPTH_DATASET_ID => serde_json::to_vec(&self.depth_dataset()).ok(),
            STATUS_DATASET_ID => serde_json::to_vec(&self.status_dataset()).ok(),
            _ => None,
        }
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            AMIN_DATASET_ID | PHASE_DATASET_ID | DEPTH_DATASET_ID | STATUS_DATASET_ID => {
                self.dataset_generation.max(1)
            }
            _ => 0,
        }
    }
}

impl Drop for StageAA1Plugin {
    fn drop(&mut self) {
        self.disarm("plugin destroyed");
    }
}

export_plugin!(StageAA1Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frequency_grid_is_log_spaced_and_bounded() {
        let mut plugin = StageAA1Plugin::default();
        plugin.freq_start_hz = 100.0;
        plugin.freq_stop_hz = 10_000.0;
        plugin.points_per_decade = 4;
        let grid = plugin.frequency_grid();
        assert!((grid.first().copied().unwrap() - 100.0).abs() < 1e-9);
        assert!(grid.last().copied().unwrap() <= 10_000.0 * 1.001);
        assert_eq!(grid.len(), 9);
        for pair in grid.windows(2) {
            let ratio = pair[1] / pair[0];
            assert!((ratio - 10f64.powf(0.25)).abs() < 1e-9);
        }
    }

    #[test]
    fn status_dataset_matches_schema() {
        let plugin = StageAA1Plugin::default();
        let dataset = plugin.status_dataset();
        let schema = plugin.status_schema();
        assert_eq!(dataset.columns.len(), schema.columns.len());
    }
}
