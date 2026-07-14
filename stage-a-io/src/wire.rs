//! PDA1 binary wire format (Teensy → host).
//!
//! Mirrors `stage-a-controller/include/wire_protocol.h` exactly: a packed
//! 36-byte little-endian header followed by `payload_bytes` of payload,
//! integrity-protected by CRC32 (IEEE, reflected) over the zeroed-CRC header
//! plus payload. The host must tolerate arbitrary USB fragmentation and
//! resynchronise at the next valid magic + CRC.

/// `"PDA1"` interpreted as a little-endian `u32`.
pub const MAGIC: u32 = 0x3141_4450;
pub const PROTOCOL_VERSION: u8 = 1;
pub const HEADER_BYTES: usize = 36;

/// Maximum payload the parser will attempt to buffer. Larger claimed sizes
/// are treated as corruption and trigger resynchronisation.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Control,
    SamplesU16,
    Summary,
    Marker,
    Unknown(u8),
}

impl FrameType {
    pub fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::Control,
            2 => Self::SamplesU16,
            3 => Self::Summary,
            4 => Self::Marker,
            other => Self::Unknown(other),
        }
    }

    pub fn to_raw(self) -> u8 {
        match self {
            Self::Control => 1,
            Self::SamplesU16 => 2,
            Self::Summary => 3,
            Self::Marker => 4,
            Self::Unknown(other) => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub version: u8,
    pub frame_type: FrameType,
    pub flags: u16,
    pub sequence: u32,
    pub payload_bytes: u32,
    pub first_sample_index: u64,
    pub sample_rate_hz: u32,
    pub dropped_samples: u32,
    pub crc32: u32,
}

impl FrameHeader {
    pub fn parse(bytes: &[u8; HEADER_BYTES]) -> Option<Self> {
        let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        if magic != MAGIC {
            return None;
        }
        // Unknown protocol versions are corruption, not future frames: the
        // reference host parser resynchronises past them byte by byte.
        if bytes[4] != PROTOCOL_VERSION {
            return None;
        }
        Some(Self {
            version: bytes[4],
            frame_type: FrameType::from_raw(bytes[5]),
            flags: u16::from_le_bytes(bytes[6..8].try_into().ok()?),
            sequence: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            payload_bytes: u32::from_le_bytes(bytes[12..16].try_into().ok()?),
            first_sample_index: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            sample_rate_hz: u32::from_le_bytes(bytes[24..28].try_into().ok()?),
            dropped_samples: u32::from_le_bytes(bytes[28..32].try_into().ok()?),
            crc32: u32::from_le_bytes(bytes[32..36].try_into().ok()?),
        })
    }

    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut out = [0_u8; HEADER_BYTES];
        out[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        out[4] = self.version;
        out[5] = self.frame_type.to_raw();
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..12].copy_from_slice(&self.sequence.to_le_bytes());
        out[12..16].copy_from_slice(&self.payload_bytes.to_le_bytes());
        out[16..24].copy_from_slice(&self.first_sample_index.to_le_bytes());
        out[24..28].copy_from_slice(&self.sample_rate_hz.to_le_bytes());
        out[28..32].copy_from_slice(&self.dropped_samples.to_le_bytes());
        out[32..36].copy_from_slice(&self.crc32.to_le_bytes());
        out
    }
}

/// One complete, CRC-verified frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Builds a frame with a freshly computed CRC (mock/firmware side).
    pub fn build(mut header: FrameHeader, payload: Vec<u8>) -> Self {
        header.payload_bytes = payload.len() as u32;
        header.crc32 = frame_crc(&header, &payload);
        Self { header, payload }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_BYTES + self.payload.len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&self.payload);
        out
    }

    /// Decodes the payload of a `Summary` frame.
    pub fn summary(&self) -> Option<SummaryPayload> {
        if self.header.frame_type != FrameType::Summary || self.payload.len() != 24 {
            return None;
        }
        let p = &self.payload;
        Some(SummaryPayload {
            min_code: u16::from_le_bytes(p[0..2].try_into().ok()?),
            max_code: u16::from_le_bytes(p[2..4].try_into().ok()?),
            sample_count: u32::from_le_bytes(p[4..8].try_into().ok()?),
            sum_codes: u64::from_le_bytes(p[8..16].try_into().ok()?),
            first_tick_us: u32::from_le_bytes(p[16..20].try_into().ok()?),
            last_tick_us: u32::from_le_bytes(p[20..24].try_into().ok()?),
        })
    }

    /// Decodes the payload of a `SamplesU16` frame into ADC codes.
    pub fn samples(&self) -> Option<Vec<u16>> {
        if self.header.frame_type != FrameType::SamplesU16 || self.payload.len() % 2 != 0 {
            return None;
        }
        Some(
            self.payload
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect(),
        )
    }

    /// The ASCII payload of a `Control` frame.
    pub fn control_text(&self) -> Option<&str> {
        if self.header.frame_type != FrameType::Control {
            return None;
        }
        std::str::from_utf8(&self.payload).ok()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryPayload {
    pub min_code: u16,
    pub max_code: u16,
    pub sample_count: u32,
    pub sum_codes: u64,
    pub first_tick_us: u32,
    pub last_tick_us: u32,
}

impl SummaryPayload {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.min_code.to_le_bytes());
        out.extend_from_slice(&self.max_code.to_le_bytes());
        out.extend_from_slice(&self.sample_count.to_le_bytes());
        out.extend_from_slice(&self.sum_codes.to_le_bytes());
        out.extend_from_slice(&self.first_tick_us.to_le_bytes());
        out.extend_from_slice(&self.last_tick_us.to_le_bytes());
        out
    }

    pub fn mean_code(&self) -> f64 {
        if self.sample_count == 0 {
            return 0.0;
        }
        self.sum_codes as f64 / f64::from(self.sample_count)
    }
}

/// CRC32 (IEEE, reflected, init/final 0xFFFF_FFFF) — identical to the
/// firmware's `crc32Update` loop.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_update(0xFFFF_FFFF, data) ^ 0xFFFF_FFFF
}

/// Streaming CRC32 with the same parameters as [`crc32`], for hashing data
/// that is not held in memory at once (e.g. the PDQ file writer).
#[derive(Debug, Clone, Copy)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self { state: 0xFFFF_FFFF }
    }
}

impl Crc32 {
    pub fn update(&mut self, data: &[u8]) {
        self.state = crc32_update(self.state, data);
    }

    pub fn finalize(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    crc
}

/// CRC over the zeroed-CRC header plus payload (firmware `frameCrc`).
pub fn frame_crc(header: &FrameHeader, payload: &[u8]) -> u32 {
    let mut zeroed = *header;
    zeroed.crc32 = 0;
    let mut crc = 0xFFFF_FFFF_u32;
    crc = crc32_update(crc, &zeroed.encode());
    crc = crc32_update(crc, payload);
    crc ^ 0xFFFF_FFFF
}

/// What the incremental parser reports for each recovered unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseEvent {
    Frame(Frame),
    /// Bytes were skipped or a frame failed its CRC — the stream stays
    /// usable, but the run must be flagged invalid.
    Corruption {
        skipped_bytes: usize,
        crc_failures: usize,
    },
}

/// Incremental PDA1 parser tolerating arbitrary fragmentation.
///
/// Feed raw serial bytes with [`FrameParser::extend`], then drain complete
/// frames with [`FrameParser::next_event`]. On a bad magic the parser skips
/// forward one byte at a time; on a bad CRC it discards the candidate header
/// and rescans from the next byte, so a corrupted stream re-locks at the
/// next genuine frame boundary.
#[derive(Debug, Default)]
pub struct FrameParser {
    buffer: Vec<u8>,
    skipped_bytes: usize,
    crc_failures: usize,
}

impl FrameParser {
    pub fn extend(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn next_event(&mut self) -> Option<ParseEvent> {
        loop {
            // Scan to the next plausible magic.
            let mut offset = 0;
            while self.buffer.len() >= offset + 4
                && u32::from_le_bytes(self.buffer[offset..offset + 4].try_into().unwrap()) != MAGIC
            {
                offset += 1;
            }
            if offset > 0 {
                self.buffer.drain(..offset);
                self.skipped_bytes += offset;
            }

            if self.buffer.len() < HEADER_BYTES {
                return self.take_corruption();
            }

            let header_bytes: [u8; HEADER_BYTES] = self.buffer[..HEADER_BYTES].try_into().unwrap();
            let Some(header) = FrameHeader::parse(&header_bytes) else {
                // Magic matched but parse failed (cannot happen today, but
                // stay defensive): skip one byte and rescan.
                self.buffer.drain(..1);
                self.skipped_bytes += 1;
                continue;
            };

            let payload_bytes = header.payload_bytes as usize;
            if payload_bytes > MAX_PAYLOAD_BYTES {
                self.buffer.drain(..1);
                self.skipped_bytes += 1;
                continue;
            }
            if self.buffer.len() < HEADER_BYTES + payload_bytes {
                // Wait for more bytes; report any corruption noticed so far.
                return self.take_corruption();
            }

            let payload = self.buffer[HEADER_BYTES..HEADER_BYTES + payload_bytes].to_vec();
            if frame_crc(&header, &payload) != header.crc32 {
                self.crc_failures += 1;
                self.buffer.drain(..1);
                self.skipped_bytes += 1;
                continue;
            }

            self.buffer.drain(..HEADER_BYTES + payload_bytes);
            if let Some(corruption) = self.take_corruption() {
                // Deliver the corruption notice first; the verified frame is
                // still buffered as raw bytes, so re-parse it next call.
                let frame = Frame { header, payload };
                let mut bytes = frame.to_bytes();
                bytes.extend_from_slice(&self.buffer);
                self.buffer = bytes;
                return Some(corruption);
            }
            return Some(ParseEvent::Frame(Frame { header, payload }));
        }
    }

    fn take_corruption(&mut self) -> Option<ParseEvent> {
        if self.skipped_bytes == 0 && self.crc_failures == 0 {
            return None;
        }
        let event = ParseEvent::Corruption {
            skipped_bytes: self.skipped_bytes,
            crc_failures: self.crc_failures,
        };
        self.skipped_bytes = 0;
        self.crc_failures = 0;
        Some(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_frame(sequence: u32, text: &str) -> Frame {
        Frame::build(
            FrameHeader {
                version: PROTOCOL_VERSION,
                frame_type: FrameType::Control,
                flags: 0,
                sequence,
                payload_bytes: 0,
                first_sample_index: 0,
                sample_rate_hz: 0,
                dropped_samples: 0,
                crc32: 0,
            },
            text.as_bytes().to_vec(),
        )
    }

    #[test]
    fn round_trips_a_frame_through_arbitrary_fragmentation() {
        let frame = control_frame(7, "+7 OK state=SAFE_IDLE");
        let bytes = frame.to_bytes();

        let mut parser = FrameParser::default();
        for chunk in bytes.chunks(3) {
            parser.extend(chunk);
        }
        assert_eq!(parser.next_event(), Some(ParseEvent::Frame(frame)));
        assert_eq!(parser.next_event(), None);
    }

    #[test]
    fn resynchronises_after_garbage_and_reports_corruption() {
        let frame = control_frame(1, "+1 OK");
        let mut bytes = b"garbage!".to_vec();
        bytes.extend_from_slice(&frame.to_bytes());

        let mut parser = FrameParser::default();
        parser.extend(&bytes);
        assert_eq!(
            parser.next_event(),
            Some(ParseEvent::Corruption {
                skipped_bytes: 8,
                crc_failures: 0
            })
        );
        assert_eq!(parser.next_event(), Some(ParseEvent::Frame(frame)));
    }

    #[test]
    fn detects_crc_corruption_and_relocks_on_next_frame() {
        let bad = control_frame(1, "+1 OK");
        let good = control_frame(2, "!STATUS state=RUNNING");
        let mut bytes = bad.to_bytes();
        let len = bytes.len();
        bytes[len - 1] ^= 0xFF; // corrupt payload -> CRC mismatch
        bytes.extend_from_slice(&good.to_bytes());

        let mut parser = FrameParser::default();
        parser.extend(&bytes);
        let corruption = parser.next_event();
        match corruption {
            Some(ParseEvent::Corruption { crc_failures, .. }) => assert!(crc_failures >= 1),
            other => panic!("expected corruption, got {other:?}"),
        }
        assert_eq!(parser.next_event(), Some(ParseEvent::Frame(good)));
    }

    #[test]
    fn summary_payload_round_trips() {
        let summary = SummaryPayload {
            min_code: 12,
            max_code: 3_900,
            sample_count: 256,
            sum_codes: 500_000,
            first_tick_us: 1_000,
            last_tick_us: 13_800,
        };
        let frame = Frame::build(
            FrameHeader {
                version: PROTOCOL_VERSION,
                frame_type: FrameType::Summary,
                flags: 0,
                sequence: 5,
                payload_bytes: 0,
                first_sample_index: 4_096,
                sample_rate_hz: 20_000,
                dropped_samples: 0,
                crc32: 0,
            },
            summary.encode(),
        );
        assert_eq!(frame.summary(), Some(summary));
        assert!((summary.mean_code() - 1953.125).abs() < 1e-9);
    }

    #[test]
    fn samples_frame_decodes_codes() {
        let codes = [1_u16, 2, 4_095];
        let payload: Vec<u8> = codes.iter().flat_map(|c| c.to_le_bytes()).collect();
        let frame = Frame::build(
            FrameHeader {
                version: PROTOCOL_VERSION,
                frame_type: FrameType::SamplesU16,
                flags: 0,
                sequence: 9,
                payload_bytes: 0,
                first_sample_index: 0,
                sample_rate_hz: 20_000,
                dropped_samples: 0,
                crc32: 0,
            },
            payload,
        );
        assert_eq!(frame.samples(), Some(codes.to_vec()));
    }
}
