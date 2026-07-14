//! Mock Stage-A controller for tests and hardware-free plugin development.
//!
//! Mirrors firmware 0.2.0 (`stage-a-controller/src/main.cpp`) faithfully:
//! the same verbs (`HELLO`, `STATUS`, `CONFIG`, `START`, `STOP`, `PING`),
//! the same state machine (`SAFE_IDLE` → `CONFIGURED` → `RUNNING`), the
//! same error codes/details (`PROTOCOL`, `RANGE`, `STATE`, `SYNTAX`,
//! `VERB`), the same single-entry idempotent reply cache, and rejection of
//! unknown `CONFIG` fields — which is the host's feature-detection
//! mechanism, so it must never be papered over here.
//!
//! [`MockController::with_waveform_extension`] additionally models the
//! *proposed* v2 waveform firmware (`stage-a-controller/docs/features/`
//! `waveform-drive.md`): `wave`/`freq_mhz`/`center_dac`/`amplitude_dac`
//! CONFIG fields, a `capabilities` HELLO entry, and synthetic photodiode
//! blocks derived from the configured drive through a Pockels-like sin²
//! transfer — commanded DAC amplitude maps *non-linearly* to optical
//! contrast, exactly why `a` must be measured, never assumed.

use crate::protocol::ControlMessage;
use crate::transport::Transport;
use crate::wire::{Frame, FrameHeader, FrameType, SummaryPayload, PROTOCOL_VERSION};

pub const MOCK_MAX_RATE_HZ: u32 = 100_000;
pub const MOCK_MAX_BLOCK_SAMPLES: u32 = 256;
/// Proposed v2 waveform ceiling (matches the drive UI bound: 200 kHz).
pub const MOCK_MAX_FREQ_MHZ: u32 = 200_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockState {
    SafeIdle,
    Configured,
    Running,
}

impl MockState {
    fn name(self) -> &'static str {
        match self {
            Self::SafeIdle => "SAFE_IDLE",
            Self::Configured => "CONFIGURED",
            Self::Running => "RUNNING",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockWave {
    Sine,
    Square,
    Saw,
}

impl MockWave {
    /// Normalised waveform value in [-1, 1] at cycle phase `t` in [0, 1).
    fn value(self, t: f64) -> f64 {
        match self {
            Self::Sine => (2.0 * std::f64::consts::PI * t).sin(),
            Self::Square => {
                if t < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::Saw => 2.0 * t - 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct MockConfig {
    mode: String,
    rate_hz: u32,
    block_samples: u32,
    raw: bool,
    summary: bool,
    // v2 waveform extension (None until configured).
    wave: Option<MockWave>,
    freq_mhz: u32,
    center_dac: u32,
    amplitude_dac: u32,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            mode: "A1".into(),
            rate_hz: 20_000,
            block_samples: 256,
            raw: true,
            summary: true,
            wave: None,
            freq_mhz: 0,
            center_dac: 2_048,
            amplitude_dac: 0,
        }
    }
}

pub struct MockController<T: Transport> {
    transport: T,
    state: MockState,
    config: MockConfig,
    /// v2 waveform CONFIG fields accepted (proposed firmware) instead of
    /// rejected as `unknown_config_field` (firmware 0.2.0).
    waveform_extension: bool,
    /// Firmware caches exactly one reply (`cached_request_sequence`).
    cached_reply: Option<(u32, String)>,
    executed_sequences: Vec<u32>,
    executions: u32,
    drop_next_reply: bool,
    out_sequence: u32,
    line_buffer: Vec<u8>,
    sample_index: u64,
    /// Synthetic optics for [`MockController::emit_configured_block`]:
    /// photodiode code = dark + span * sin²(π/2 · drive/4095).
    pub synth_dark_code: f64,
    pub synth_span_codes: f64,
    // Legacy direct-sine synthesis (emit_sine_block).
    pub synth_center: f64,
    pub synth_amplitude: f64,
}

impl<T: Transport> MockController<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            state: MockState::SafeIdle,
            config: MockConfig::default(),
            waveform_extension: false,
            cached_reply: None,
            executed_sequences: Vec::new(),
            executions: 0,
            drop_next_reply: false,
            out_sequence: 0,
            line_buffer: Vec::new(),
            sample_index: 0,
            synth_dark_code: 40.0,
            synth_span_codes: 3_800.0,
            synth_center: 2_048.0,
            synth_amplitude: 900.0,
        }
    }

    /// Enables the proposed v2 waveform command surface.
    pub fn with_waveform_extension(mut self) -> Self {
        self.waveform_extension = true;
        self
    }

    /// Swallow the next reply (simulates a lost USB packet) — the client
    /// must retry with the identical sequence.
    pub fn drop_first_reply(&mut self) {
        self.drop_next_reply = true;
    }

    pub fn state(&self) -> MockState {
        self.state
    }

    /// Commands actually executed (idempotent retries excluded).
    pub fn executions(&self) -> u32 {
        self.executions
    }

    /// Handles all complete command lines already received, without
    /// blocking — for long-lived in-process mock threads (e.g. a plugin's
    /// hardware-free `mock` port).
    pub fn poll_commands(&mut self) {
        let mut buf = [0_u8; 1024];
        loop {
            let read = self.transport.read(&mut buf).unwrap_or(0);
            if read == 0 {
                break;
            }
            self.line_buffer.extend_from_slice(&buf[..read]);
        }
        while let Some(pos) = self.line_buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.line_buffer.drain(..=pos).collect();
            if let Ok(text) = std::str::from_utf8(&line) {
                let text = text.trim_end().to_owned();
                self.handle_line(&text);
            }
        }
    }

    /// Wall-clock duration one configured sample block spans — the cadence
    /// at which a live mock should call [`Self::emit_configured_block`].
    pub fn block_period(&self) -> std::time::Duration {
        let rate = self.config.rate_hz.max(1);
        std::time::Duration::from_micros(
            u64::from(self.config.block_samples) * 1_000_000 / u64::from(rate),
        )
    }

    /// Serves exactly `n` command lines (counting retries), then returns.
    pub fn serve_n_commands(&mut self, n: usize) {
        let mut served = 0;
        let mut buf = [0_u8; 1024];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while served < n && std::time::Instant::now() < deadline {
            let read = self.transport.read(&mut buf).unwrap_or(0);
            if read == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            self.line_buffer.extend_from_slice(&buf[..read]);
            while let Some(pos) = self.line_buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.line_buffer.drain(..=pos).collect();
                if let Ok(text) = std::str::from_utf8(&line) {
                    self.handle_line(text.trim_end());
                }
                served += 1;
                if served >= n {
                    break;
                }
            }
        }
    }

    fn handle_line(&mut self, line: &str) {
        let Some(rest) = line.strip_prefix('@') else {
            self.send_control("-0 ERR code=SYNTAX detail=expected_sequence_and_verb");
            return;
        };
        let mut parts = rest.split_ascii_whitespace();
        let Some(sequence) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            self.send_control("-0 ERR code=SYNTAX detail=invalid_sequence");
            return;
        };
        // Idempotent retry: replay the cached reply without re-executing.
        if let Some((cached_seq, cached)) = &self.cached_reply {
            if *cached_seq == sequence {
                let payload = cached.clone();
                self.send_control(&payload);
                return;
            }
        }
        assert!(
            !self.executed_sequences.contains(&sequence),
            "sequence {sequence} re-executed — idempotency broken"
        );

        let verb = parts.next().unwrap_or("");
        // Preserve wire order: firmware validates fields as encountered.
        let fields: Vec<(String, String)> = parts
            .filter_map(|part| {
                let (key, value) = part.split_once('=')?;
                Some((key.to_owned(), value.to_owned()))
            })
            .collect();

        self.executions += 1;
        self.executed_sequences.push(sequence);
        let reply = self.execute(verb, &fields, sequence);
        self.cached_reply = Some((sequence, reply.clone()));
        if self.drop_next_reply {
            self.drop_next_reply = false;
            return;
        }
        self.send_control(&reply);
    }

    fn execute(&mut self, verb: &str, fields: &[(String, String)], sequence: u32) -> String {
        let field = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        match verb {
            "HELLO" => {
                if field("protocol") != Some("1") {
                    return format!("-{sequence} ERR code=PROTOCOL detail=requires_v1");
                }
                let capabilities = if self.waveform_extension {
                    " capabilities=A1,A2,A3,WAVE"
                } else {
                    ""
                };
                format!(
                    "+{sequence} OK protocol=1 firmware=0.2.0-mock board=MOCK adc_bits=12 \
                     max_rate_hz={MOCK_MAX_RATE_HZ} dac=AD5628 dac_bus=SPI1 dac_cs=29 \
                     dac_channel=1.4 dac_address=3{capabilities}"
                )
            }
            "STATUS" => format!(
                "+{sequence} OK state={} mode={} rate_hz={} block_samples={} raw={} summary={} \
                 sample_index={} dropped=0 marker_drops=0 dac=1.4/3 code=0",
                self.state.name(),
                self.config.mode,
                self.config.rate_hz,
                self.config.block_samples,
                u8::from(self.config.raw),
                u8::from(self.config.summary),
                self.sample_index,
            ),
            "CONFIG" => self.execute_config(fields, sequence),
            "START" => {
                if self.state != MockState::Configured {
                    return format!("-{sequence} ERR code=STATE detail=configure_before_start");
                }
                self.state = MockState::Running;
                format!("+{sequence} OK state=RUNNING")
            }
            // Firmware ignores extra STOP tokens (e.g. reason=…).
            "STOP" => {
                self.state = MockState::SafeIdle;
                format!("+{sequence} OK state=SAFE_IDLE")
            }
            "PING" => format!("+{sequence} OK watchdog=refreshed"),
            _ => format!("-{sequence} ERR code=VERB detail=unsupported_command"),
        }
    }

    fn execute_config(&mut self, fields: &[(String, String)], sequence: u32) -> String {
        if self.state == MockState::Running {
            return format!("-{sequence} ERR code=STATE detail=stop_before_config");
        }
        let err = |code: &str, detail: &str| format!("-{sequence} ERR code={code} detail={detail}");
        let mut next = self.config.clone();
        let mut saw_mode = false;
        let mut saw_rate = false;
        for (key, value) in fields {
            match key.as_str() {
                "mode" => {
                    saw_mode = true;
                    if !matches!(value.as_str(), "A1" | "A2" | "A3") {
                        return err("RANGE", "invalid_mode");
                    }
                    next.mode = value.clone();
                }
                "rate_hz" => {
                    saw_rate = true;
                    match value.parse::<u32>() {
                        Ok(rate) if (100..=MOCK_MAX_RATE_HZ).contains(&rate) => {
                            next.rate_hz = rate;
                        }
                        _ => return err("RANGE", "invalid_rate_hz"),
                    }
                }
                "block_samples" => match value.parse::<u32>() {
                    Ok(block) if (1..=MOCK_MAX_BLOCK_SAMPLES).contains(&block) => {
                        next.block_samples = block;
                    }
                    _ => return err("RANGE", "invalid_block_samples"),
                },
                "raw" => match value.as_str() {
                    "0" => next.raw = false,
                    "1" => next.raw = true,
                    _ => return err("RANGE", "invalid_raw_flag"),
                },
                "summary" => match value.as_str() {
                    "0" => next.summary = false,
                    "1" => next.summary = true,
                    _ => return err("RANGE", "invalid_summary_flag"),
                },
                "wave" if self.waveform_extension => {
                    next.wave = Some(match value.as_str() {
                        "SINE" => MockWave::Sine,
                        "SQUARE" => MockWave::Square,
                        "SAW" => MockWave::Saw,
                        _ => return err("RANGE", "invalid_wave"),
                    });
                }
                "freq_mhz" if self.waveform_extension => match value.parse::<u32>() {
                    Ok(freq) if (1..=MOCK_MAX_FREQ_MHZ).contains(&freq) => {
                        next.freq_mhz = freq;
                    }
                    _ => return err("RANGE", "invalid_freq_mhz"),
                },
                "center_dac" if self.waveform_extension => match value.parse::<u32>() {
                    Ok(center) if center <= 4_095 => next.center_dac = center,
                    _ => return err("RANGE", "invalid_center_dac"),
                },
                "amplitude_dac" if self.waveform_extension => match value.parse::<u32>() {
                    Ok(amplitude) if amplitude <= 2_047 => next.amplitude_dac = amplitude,
                    _ => return err("RANGE", "invalid_amplitude_dac"),
                },
                // Firmware 0.2.0 rejects unknown fields — the host relies
                // on this for feature detection. Never accept silently.
                _ => return err("SYNTAX", "unknown_config_field"),
            }
        }
        if !saw_mode || !saw_rate || (!next.raw && !next.summary) {
            return err("SYNTAX", "mode_rate_and_output_required");
        }
        if next.wave.is_some()
            && (next.center_dac + next.amplitude_dac > 4_095
                || next.center_dac < next.amplitude_dac)
        {
            return err("RANGE", "amplitude_exceeds_range");
        }
        self.config = next;
        self.state = MockState::Configured;
        format!(
            "+{sequence} OK state=CONFIGURED mode={} rate_hz={} block_samples={} raw={} \
             summary={} backend=mock",
            self.config.mode,
            self.config.rate_hz,
            self.config.block_samples,
            u8::from(self.config.raw),
            u8::from(self.config.summary),
        )
    }

    fn send_control(&mut self, payload: &str) {
        let frame = self.build_frame(FrameType::Control, payload.as_bytes().to_vec(), 0, 0);
        let bytes = frame.to_bytes();
        let _ = self.transport.write_all(&bytes);
    }

    /// Emits the watchdog fault notice and drops to `SAFE_IDLE`, exactly as
    /// the firmware does after 1.5 s without host contact.
    pub fn emit_watchdog_fault(&mut self) {
        self.state = MockState::SafeIdle;
        self.send_control("!FAULT code=WATCHDOG state=SAFE_IDLE");
    }

    fn build_frame(
        &mut self,
        frame_type: FrameType,
        payload: Vec<u8>,
        sample_rate_hz: u32,
        dropped_samples: u32,
    ) -> Frame {
        self.out_sequence = self.out_sequence.wrapping_add(1);
        Frame::build(
            FrameHeader {
                version: PROTOCOL_VERSION,
                frame_type,
                flags: 0,
                sequence: self.out_sequence,
                payload_bytes: 0,
                first_sample_index: self.sample_index,
                sample_rate_hz,
                dropped_samples,
                crc32: 0,
            },
            payload,
        )
    }

    fn emit_codes_block(&mut self, codes: &[u16], rate_hz: u32, raw: bool, summary: bool) {
        let mut min_code = u16::MAX;
        let mut max_code = 0_u16;
        let mut sum = 0_u64;
        let mut payload = Vec::with_capacity(codes.len() * 2);
        for &code in codes {
            min_code = min_code.min(code);
            max_code = max_code.max(code);
            sum += u64::from(code);
            payload.extend_from_slice(&code.to_le_bytes());
        }
        if raw {
            let frame = self.build_frame(FrameType::SamplesU16, payload, rate_hz, 0);
            let bytes = frame.to_bytes();
            let _ = self.transport.write_all(&bytes);
        }
        if summary {
            let summary_payload = SummaryPayload {
                min_code,
                max_code,
                sample_count: codes.len() as u32,
                sum_codes: sum,
                first_tick_us: 0,
                last_tick_us: ((codes.len() as f64 / f64::from(rate_hz)) * 1e6) as u32,
            };
            let frame = self.build_frame(FrameType::Summary, summary_payload.encode(), rate_hz, 0);
            let bytes = frame.to_bytes();
            let _ = self.transport.write_all(&bytes);
        }
        self.sample_index += codes.len() as u64;
    }

    /// Emits one photodiode block synthesized from the *configured* v2
    /// drive: DAC waveform → Pockels-like sin² intensity transfer → ADC
    /// codes. Without a configured `wave` (or with `amplitude_dac = 0`) the
    /// output is the flat unmodulated level at `center_dac`.
    pub fn emit_configured_block(&mut self) {
        if self.state != MockState::Running {
            return;
        }
        let config = self.config.clone();
        let rate = f64::from(config.rate_hz);
        let freq_hz = f64::from(config.freq_mhz) / 1_000.0;
        let codes: Vec<u16> = (0..config.block_samples as u64)
            .map(|i| {
                let t = (self.sample_index + i) as f64 / rate;
                let shape = match (config.wave, config.amplitude_dac) {
                    (Some(wave), amplitude) if amplitude > 0 && freq_hz > 0.0 => {
                        wave.value((t * freq_hz).fract())
                    }
                    _ => 0.0,
                };
                let drive = f64::from(config.center_dac) + f64::from(config.amplitude_dac) * shape;
                let transmission = (std::f64::consts::FRAC_PI_2 * drive / 4_095.0)
                    .sin()
                    .powi(2);
                (self.synth_dark_code + self.synth_span_codes * transmission)
                    .round()
                    .clamp(0.0, 4_095.0) as u16
            })
            .collect();
        self.emit_codes_block(&codes, config.rate_hz, config.raw, config.summary);
    }

    /// Emits one synthetic sinusoidal sample block (`SamplesU16` +
    /// `Summary`), bypassing the drive model — codes = center + A·sin.
    pub fn emit_sine_block(&mut self, samples: usize, rate_hz: u32, freq_hz: f64) {
        let codes: Vec<u16> = (0..samples as u64)
            .map(|i| {
                let t = (self.sample_index + i) as f64 / f64::from(rate_hz);
                (self.synth_center
                    + self.synth_amplitude * (2.0 * std::f64::consts::PI * freq_hz * t).sin())
                .round()
                .clamp(0.0, 4_095.0) as u16
            })
            .collect();
        self.emit_codes_block(&codes, rate_hz, true, true);
    }

    /// Emits a summary frame carrying a nonzero overrun counter.
    pub fn emit_summary_with_drops(&mut self, dropped: u32) {
        let summary = SummaryPayload {
            min_code: 0,
            max_code: 0,
            sample_count: 0,
            sum_codes: 0,
            first_tick_us: 0,
            last_tick_us: 0,
        };
        let frame = self.build_frame(FrameType::Summary, summary.encode(), 20_000, dropped);
        let bytes = frame.to_bytes();
        let _ = self.transport.write_all(&bytes);
    }
}

/// Convenience for tests that need a parsed view of a control payload.
pub fn parse_control(text: &str) -> Option<ControlMessage> {
    ControlMessage::parse(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::MockLink;

    fn request(controller: &mut MockController<crate::transport::MockTransport>, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        // Feed the line directly through the device-side buffer path.
        controller.line_buffer.extend_from_slice(&bytes);
        while let Some(pos) = controller.line_buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = controller.line_buffer.drain(..=pos).collect();
            let text = std::str::from_utf8(&line).unwrap().trim_end().to_owned();
            controller.handle_line(&text);
        }
    }

    fn last_control_text(host: &mut crate::transport::MockTransport) -> String {
        let mut parser = crate::wire::FrameParser::default();
        let mut buf = [0_u8; 4096];
        let mut last = None;
        loop {
            let n = crate::transport::Transport::read(host, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            parser.extend(&buf[..n]);
        }
        while let Some(event) = parser.next_event() {
            if let crate::wire::ParseEvent::Frame(frame) = event {
                if let Some(text) = frame.control_text() {
                    last = Some(text.to_owned());
                }
            }
        }
        last.expect("a control frame was emitted")
    }

    #[test]
    fn matches_firmware_state_machine_and_error_details() {
        let link = MockLink::new();
        let mut host = link.host_end();
        let mut controller = MockController::new(link.device_end());

        // START before CONFIG → STATE error, firmware detail string.
        request(&mut controller, "@1 START");
        assert!(last_control_text(&mut host).contains("code=STATE detail=configure_before_start"));

        // Valid CONFIG, then START, then CONFIG while running is rejected.
        request(&mut controller, "@2 CONFIG mode=A1 rate_hz=20000");
        assert!(last_control_text(&mut host).starts_with("+2 OK state=CONFIGURED"));
        request(&mut controller, "@3 START");
        assert_eq!(controller.state(), MockState::Running);
        request(&mut controller, "@4 CONFIG mode=A1 rate_hz=20000");
        assert!(last_control_text(&mut host).contains("code=STATE detail=stop_before_config"));

        // STOP always succeeds and ignores extra fields.
        request(&mut controller, "@5 STOP reason=test");
        assert_eq!(controller.state(), MockState::SafeIdle);
    }

    #[test]
    fn firmware_v1_rejects_waveform_fields_as_unknown() {
        let link = MockLink::new();
        let mut host = link.host_end();
        let mut controller = MockController::new(link.device_end());

        request(
            &mut controller,
            "@1 CONFIG mode=A1 wave=SINE freq_mhz=1000000 rate_hz=20000",
        );
        assert!(last_control_text(&mut host).contains("code=SYNTAX detail=unknown_config_field"));
    }

    #[test]
    fn hello_requires_protocol_v1_and_advertises_capabilities_only_with_extension() {
        let link = MockLink::new();
        let mut host = link.host_end();
        let mut controller = MockController::new(link.device_end());
        request(&mut controller, "@1 HELLO");
        assert!(last_control_text(&mut host).contains("code=PROTOCOL detail=requires_v1"));
        request(&mut controller, "@2 HELLO protocol=1");
        assert!(!last_control_text(&mut host).contains("capabilities"));

        let link = MockLink::new();
        let mut host = link.host_end();
        let mut controller = MockController::new(link.device_end()).with_waveform_extension();
        request(&mut controller, "@1 HELLO protocol=1");
        assert!(last_control_text(&mut host).contains("capabilities=A1,A2,A3,WAVE"));
    }

    #[test]
    fn waveform_extension_validates_drive_bounds() {
        let link = MockLink::new();
        let mut host = link.host_end();
        let mut controller = MockController::new(link.device_end()).with_waveform_extension();

        request(
            &mut controller,
            "@1 CONFIG mode=A1 rate_hz=20000 wave=TRIANGLE freq_mhz=1000000",
        );
        assert!(last_control_text(&mut host).contains("code=RANGE detail=invalid_wave"));

        request(
            &mut controller,
            "@2 CONFIG mode=A1 rate_hz=20000 wave=SINE freq_mhz=1000000 center_dac=3000 \
             amplitude_dac=2000",
        );
        assert!(last_control_text(&mut host).contains("code=RANGE detail=amplitude_exceeds_range"));

        request(
            &mut controller,
            "@3 CONFIG mode=A1 rate_hz=20000 wave=SAW freq_mhz=1000000 center_dac=2048 \
             amplitude_dac=512",
        );
        assert!(last_control_text(&mut host).starts_with("+3 OK state=CONFIGURED"));
    }

    #[test]
    fn configured_drive_synthesizes_nonlinear_pockels_response() {
        let contrast_for_amplitude = |amplitude: u32| -> f64 {
            let link = MockLink::new();
            let mut host = link.host_end();
            let mut controller = MockController::new(link.device_end()).with_waveform_extension();
            request(
                &mut controller,
                &format!(
                    "@1 CONFIG mode=A1 rate_hz=20000 wave=SINE freq_mhz=100000 center_dac=2048 \
                     amplitude_dac={amplitude}"
                ),
            );
            request(&mut controller, "@2 START");
            let _ = last_control_text(&mut host);
            for _ in 0..8 {
                controller.emit_configured_block();
            }

            let mut parser = crate::wire::FrameParser::default();
            let mut buf = [0_u8; 65_536];
            loop {
                let n = crate::transport::Transport::read(&mut host, &mut buf).unwrap();
                if n == 0 {
                    break;
                }
                parser.extend(&buf[..n]);
            }
            let mut codes = Vec::new();
            while let Some(event) = parser.next_event() {
                if let crate::wire::ParseEvent::Frame(frame) = event {
                    if let Some(samples) = frame.samples() {
                        codes.extend(samples);
                    }
                }
            }
            let estimate = crate::estimator::estimate_contrast(
                &codes,
                &crate::estimator::AdcCalibration {
                    dark_volts: 40.0 * 3.3 / 4_095.0,
                    ..Default::default()
                },
            )
            .expect("clean synthetic window");
            estimate.a
        };

        let a_small = contrast_for_amplitude(512);
        let a_double = contrast_for_amplitude(1_024);
        assert!(a_small > 0.0 && a_double > a_small);
        // sin² transfer: doubling the DAC amplitude must NOT double a.
        assert!(
            (a_double / a_small - 2.0).abs() > 0.05,
            "a_small={a_small} a_double={a_double} — response looks linear"
        );
    }
}
