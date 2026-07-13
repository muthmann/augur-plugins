//! Typed request/response client over a [`Transport`].
//!
//! Sends `@<seq> VERB …` commands and demultiplexes the PDA1 frame stream
//! into (a) the matching control reply, (b) async control notices, and
//! (c) data frames (samples / summaries / markers). On a reply timeout the
//! **identical** line (same `seq`) is resent; firmware caches recent replies,
//! so retries are idempotent by construction.

use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

use crate::protocol::{Command, ControlMessage, ProtocolError};
use crate::transport::Transport;
use crate::wire::{Frame, FrameParser, FrameType, ParseEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StreamIntegrity {
    pub skipped_bytes: u64,
    pub crc_failures: u64,
    pub sequence_gaps: u64,
    pub dropped_samples: u64,
}

impl StreamIntegrity {
    /// A run is valid only while the stream shows zero corruption.
    pub fn is_clean(&self) -> bool {
        self.skipped_bytes == 0
            && self.crc_failures == 0
            && self.sequence_gaps == 0
            && self.dropped_samples == 0
    }
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Protocol(ProtocolError),
    /// The device replied `-seq ERR …`.
    Device {
        code: String,
        detail: String,
    },
    /// No matching reply within the timeout across all retries.
    Timeout {
        verb: String,
        retries: u32,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "transport I/O failed: {err}"),
            Self::Protocol(err) => write!(f, "protocol violation: {err}"),
            Self::Device { code, detail } => {
                write!(f, "device rejected command: code={code} detail={detail}")
            }
            Self::Timeout { verb, retries } => {
                write!(f, "no reply to {verb} after {retries} retries")
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl From<io::Error> for ClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<ProtocolError> for ClientError {
    fn from(err: ProtocolError) -> Self {
        Self::Protocol(err)
    }
}

/// Non-reply traffic observed while waiting for or between replies.
#[derive(Debug, Clone, PartialEq)]
pub enum DeviceEvent {
    Data(Frame),
    Async {
        name: String,
        fields: BTreeMap<String, String>,
    },
}

pub struct StageAClient<T: Transport> {
    transport: T,
    parser: FrameParser,
    next_sequence: u32,
    last_frame_sequence: Option<u32>,
    integrity: StreamIntegrity,
    pending_events: Vec<DeviceEvent>,
    reply_timeout: Duration,
    max_retries: u32,
    read_buf: Vec<u8>,
}

impl<T: Transport> StageAClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            parser: FrameParser::default(),
            next_sequence: 1,
            last_frame_sequence: None,
            integrity: StreamIntegrity::default(),
            pending_events: Vec::new(),
            reply_timeout: Duration::from_millis(500),
            max_retries: 2,
            read_buf: vec![0_u8; 16 * 1024],
        }
    }

    pub fn with_reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    pub fn integrity(&self) -> StreamIntegrity {
        self.integrity
    }

    /// Sends a command and waits for its `+seq OK` reply, retrying the
    /// identical line on timeout. Data/async frames arriving in between are
    /// queued for [`StageAClient::poll_events`].
    pub fn request(&mut self, command: &Command) -> Result<BTreeMap<String, String>, ClientError> {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        let line = command.encode(sequence)?;

        for _attempt in 0..=self.max_retries {
            self.transport.write_all(&line)?;
            let deadline = Instant::now() + self.reply_timeout;
            while Instant::now() < deadline {
                self.pump()?;
                if let Some(reply) = self.take_reply(sequence)? {
                    return Ok(reply);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Err(ClientError::Timeout {
            verb: command.verb.clone(),
            retries: self.max_retries,
        })
    }

    /// Drains any pending non-reply device traffic (data frames, async
    /// notices) without blocking.
    pub fn poll_events(&mut self) -> Result<Vec<DeviceEvent>, ClientError> {
        self.pump()?;
        Ok(std::mem::take(&mut self.pending_events))
    }

    fn pump(&mut self) -> Result<(), ClientError> {
        let n = self.transport.read(&mut self.read_buf)?;
        if n > 0 {
            self.parser.extend(&self.read_buf[..n]);
        }
        while let Some(event) = self.parser.next_event() {
            match event {
                ParseEvent::Corruption {
                    skipped_bytes,
                    crc_failures,
                } => {
                    self.integrity.skipped_bytes += skipped_bytes as u64;
                    self.integrity.crc_failures += crc_failures as u64;
                }
                ParseEvent::Frame(frame) => self.accept_frame(frame),
            }
        }
        Ok(())
    }

    fn accept_frame(&mut self, frame: Frame) {
        if let Some(last) = self.last_frame_sequence {
            let expected = last.wrapping_add(1);
            if frame.header.sequence != expected {
                self.integrity.sequence_gaps += 1;
            }
        }
        self.last_frame_sequence = Some(frame.header.sequence);
        if frame.header.dropped_samples > 0 {
            self.integrity.dropped_samples = u64::from(frame.header.dropped_samples);
        }

        match frame.header.frame_type {
            FrameType::Control => {
                // Control payloads are handled by take_reply / async queue;
                // keep the raw frame so replies can be matched later.
                self.pending_events.push(DeviceEvent::Data(frame));
            }
            _ => self.pending_events.push(DeviceEvent::Data(frame)),
        }
    }

    fn take_reply(
        &mut self,
        sequence: u32,
    ) -> Result<Option<BTreeMap<String, String>>, ClientError> {
        let mut result = None;
        let mut remaining = Vec::with_capacity(self.pending_events.len());
        for event in std::mem::take(&mut self.pending_events) {
            if result.is_some() {
                remaining.push(event);
                continue;
            }
            let DeviceEvent::Data(frame) = &event else {
                remaining.push(event);
                continue;
            };
            let Some(text) = frame.control_text() else {
                remaining.push(event);
                continue;
            };
            match ControlMessage::parse(text) {
                Ok(ControlMessage::Ok {
                    sequence: reply_seq,
                    fields,
                }) if reply_seq == sequence => {
                    result = Some(Ok(fields));
                }
                Ok(ControlMessage::Err {
                    sequence: reply_seq,
                    code,
                    detail,
                }) if reply_seq == sequence => {
                    result = Some(Err(ClientError::Device { code, detail }));
                }
                Ok(ControlMessage::Async { name, fields }) => {
                    remaining.push(DeviceEvent::Async { name, fields });
                }
                // Stale replies to earlier (retried) sequences are dropped;
                // malformed control payloads count as corruption.
                Ok(_) => {}
                Err(_) => {
                    self.integrity.crc_failures += 0; // parse failure, not CRC
                    self.integrity.skipped_bytes += frame.payload.len() as u64;
                }
            }
        }
        self.pending_events = remaining;
        match result {
            Some(Ok(fields)) => Ok(Some(fields)),
            Some(Err(err)) => Err(err),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockController;
    use crate::transport::MockLink;

    #[test]
    fn request_reply_round_trip_with_hello() {
        let link = MockLink::new();
        let mut controller = MockController::new(link.device_end());
        let mut client =
            StageAClient::new(link.host_end()).with_reply_timeout(Duration::from_millis(100));

        let handle = std::thread::spawn(move || controller.serve_n_commands(1));
        let reply = client
            .request(&Command::new("HELLO").field("protocol", 1))
            .expect("HELLO replies");
        handle.join().expect("mock thread joins");

        assert_eq!(reply.get("protocol").map(String::as_str), Some("1"));
        assert!(client.integrity().is_clean());
    }

    #[test]
    fn timeout_retries_are_idempotent_via_reply_cache() {
        let link = MockLink::new();
        let mut controller = MockController::new(link.device_end());
        controller.drop_first_reply();
        let mut client =
            StageAClient::new(link.host_end()).with_reply_timeout(Duration::from_millis(50));

        // The controller swallows the first reply; the client must resend the
        // identical sequence and accept the cached second reply. The mock
        // panics if a retried sequence re-executes the operation.
        let handle = std::thread::spawn(move || controller.serve_n_commands(2));
        let reply = client
            .request(&Command::new("STATUS"))
            .expect("retried STATUS succeeds");
        handle.join().expect("mock thread joins");

        assert_eq!(reply.get("state").map(String::as_str), Some("SAFE_IDLE"));
        assert_eq!(reply.get("executions").map(String::as_str), Some("1"));
    }

    #[test]
    fn device_error_reply_surfaces_code_and_detail() {
        let link = MockLink::new();
        let mut controller = MockController::new(link.device_end());
        let mut client =
            StageAClient::new(link.host_end()).with_reply_timeout(Duration::from_millis(100));

        let handle = std::thread::spawn(move || controller.serve_n_commands(1));
        let err = client
            .request(&Command::new("CONFIG").field("mode", "A9"))
            .expect_err("invalid mode is rejected");
        handle.join().expect("mock thread joins");

        match err {
            ClientError::Device { code, .. } => assert_eq!(code, "BAD_MODE"),
            other => panic!("expected device error, got {other:?}"),
        }
    }

    #[test]
    fn overrun_frames_invalidate_integrity() {
        let link = MockLink::new();
        let mut controller = MockController::new(link.device_end());
        let mut client =
            StageAClient::new(link.host_end()).with_reply_timeout(Duration::from_millis(100));

        controller.emit_summary_with_drops(3);
        client.poll_events().expect("poll");
        assert!(!client.integrity().is_clean());
        assert_eq!(client.integrity().dropped_samples, 3);
    }
}
