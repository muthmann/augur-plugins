//! Mock Stage-A controller for tests and hardware-free plugin development.
//!
//! Implements the v1 command surface (`HELLO`, `STATUS`, `CONFIG`, `ARM`,
//! `RUN`, `START`, `STOP`, `PING`, `FAULT_CLEAR`) with the same idempotency
//! contract as the firmware: replies to recent sequences are cached and
//! resent without re-executing the operation. It can also synthesize
//! photodiode sample/summary frames (sinusoidal drive) so the estimator and
//! plugins can be exercised end to end without a Teensy.

use std::collections::BTreeMap;

use crate::protocol::ControlMessage;
use crate::transport::Transport;
use crate::wire::{Frame, FrameHeader, FrameType, SummaryPayload, PROTOCOL_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockState {
    SafeIdle,
    Configured,
    Armed,
    Running,
}

impl MockState {
    fn name(self) -> &'static str {
        match self {
            Self::SafeIdle => "SAFE_IDLE",
            Self::Configured => "CONFIGURED",
            Self::Armed => "ARMED",
            Self::Running => "RUNNING",
        }
    }
}

pub struct MockController<T: Transport> {
    transport: T,
    state: MockState,
    config: BTreeMap<String, String>,
    config_revision: u32,
    reply_cache: Vec<(u32, String)>,
    executed_sequences: Vec<u32>,
    /// Commands executed (used to assert idempotency in tests).
    executions: u32,
    drop_next_reply: bool,
    out_sequence: u32,
    line_buffer: Vec<u8>,
    sample_index: u64,
    /// Synthetic optical waveform: codes = center + amplitude*sin(phase).
    pub synth_center: f64,
    pub synth_amplitude: f64,
    pub synth_dark_code: f64,
}

impl<T: Transport> MockController<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            state: MockState::SafeIdle,
            config: BTreeMap::new(),
            config_revision: 0,
            reply_cache: Vec::new(),
            executed_sequences: Vec::new(),
            executions: 0,
            drop_next_reply: false,
            out_sequence: 0,
            line_buffer: Vec::new(),
            sample_index: 0,
            synth_center: 2_048.0,
            synth_amplitude: 900.0,
            synth_dark_code: 40.0,
        }
    }

    /// Swallow the next reply (simulates a lost USB packet) — the client
    /// must retry with the identical sequence.
    pub fn drop_first_reply(&mut self) {
        self.drop_next_reply = true;
    }

    pub fn state(&self) -> MockState {
        self.state
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
            return;
        };
        let mut parts = rest.split_ascii_whitespace();
        let Some(sequence) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            return;
        };
        // Idempotent retry: replay the cached reply without re-executing.
        if let Some((_, cached)) = self
            .reply_cache
            .iter()
            .find(|(cached_seq, _)| *cached_seq == sequence)
        {
            let payload = cached.clone();
            self.send_control(&payload);
            return;
        }
        assert!(
            !self.executed_sequences.contains(&sequence),
            "sequence {sequence} re-executed — idempotency broken"
        );

        let verb = parts.next().unwrap_or("");
        let fields: BTreeMap<String, String> = parts
            .filter_map(|part| {
                let (key, value) = part.split_once('=')?;
                Some((key.to_owned(), value.to_owned()))
            })
            .collect();

        self.executions += 1;
        self.executed_sequences.push(sequence);
        let reply = self.execute(verb, &fields, sequence);
        self.reply_cache.push((sequence, reply.clone()));
        if self.reply_cache.len() > 8 {
            self.reply_cache.remove(0);
        }
        if self.drop_next_reply {
            self.drop_next_reply = false;
            return;
        }
        self.send_control(&reply);
    }

    fn execute(&mut self, verb: &str, fields: &BTreeMap<String, String>, sequence: u32) -> String {
        match verb {
            "HELLO" => format!(
                "+{sequence} OK protocol=1 firmware=0.1.0-mock board=mock dac_bits=12 \
                 capabilities=A1,A2,A3"
            ),
            "STATUS" => format!(
                "+{sequence} OK state={} rev={} executions={}",
                self.state.name(),
                self.config_revision,
                self.executions
            ),
            "PING" => format!("+{sequence} OK state={}", self.state.name()),
            "CONFIG" => {
                let mode = fields.get("mode").map(String::as_str).unwrap_or("");
                if !matches!(mode, "A1" | "A2" | "A3") {
                    return format!("-{sequence} ERR code=BAD_MODE detail=mode");
                }
                self.config = fields.clone();
                self.config_revision += 1;
                self.state = MockState::Configured;
                format!("+{sequence} OK rev={}", self.config_revision)
            }
            "ARM" => {
                if self.state != MockState::Configured {
                    return format!("-{sequence} ERR code=BAD_STATE detail=arm_requires_config");
                }
                self.state = MockState::Armed;
                format!("+{sequence} OK state=ARMED rev={}", self.config_revision)
            }
            "RUN" | "START" => {
                if !matches!(self.state, MockState::Armed | MockState::Configured) {
                    return format!("-{sequence} ERR code=BAD_STATE detail=run_requires_arm");
                }
                self.state = MockState::Running;
                format!("+{sequence} OK state=RUNNING")
            }
            "STOP" => {
                self.state = MockState::SafeIdle;
                format!("+{sequence} OK state=SAFE_IDLE")
            }
            "FAULT_CLEAR" => format!("+{sequence} OK state={}", self.state.name()),
            _ => format!("-{sequence} ERR code=BAD_VERB detail={verb}"),
        }
    }

    fn send_control(&mut self, payload: &str) {
        let frame = self.build_frame(FrameType::Control, payload.as_bytes().to_vec(), 0, 0);
        let bytes = frame.to_bytes();
        let _ = self.transport.write_all(&bytes);
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

    /// Emits one synthetic sinusoidal sample block (`SamplesU16`).
    pub fn emit_sine_block(&mut self, samples: usize, rate_hz: u32, freq_hz: f64) {
        let mut payload = Vec::with_capacity(samples * 2);
        let mut min_code = u16::MAX;
        let mut max_code = 0_u16;
        let mut sum = 0_u64;
        for i in 0..samples {
            let t = (self.sample_index + i as u64) as f64 / f64::from(rate_hz);
            let value = self.synth_center
                + self.synth_amplitude * (2.0 * std::f64::consts::PI * freq_hz * t).sin();
            let code = value.round().clamp(0.0, 4_095.0) as u16;
            min_code = min_code.min(code);
            max_code = max_code.max(code);
            sum += u64::from(code);
            payload.extend_from_slice(&code.to_le_bytes());
        }
        let frame = self.build_frame(FrameType::SamplesU16, payload, rate_hz, 0);
        let bytes = frame.to_bytes();
        let _ = self.transport.write_all(&bytes);

        let summary = SummaryPayload {
            min_code,
            max_code,
            sample_count: samples as u32,
            sum_codes: sum,
            first_tick_us: 0,
            last_tick_us: ((samples as f64 / f64::from(rate_hz)) * 1e6) as u32,
        };
        let frame = self.build_frame(FrameType::Summary, summary.encode(), rate_hz, 0);
        let bytes = frame.to_bytes();
        let _ = self.transport.write_all(&bytes);
        self.sample_index += samples as u64;
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
