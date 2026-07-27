//! Streaming `.pdq` persistence, replay, and evidence receipts.
//!
//! Writers preserve every clean PDA1 frame verbatim and finalize the file
//! with CRC32, SHA-256, byte/frame counts, and a contiguous device sample
//! range when one exists. Readers incrementally recover the same frames,
//! report corruption/truncated tails, and produce an independently computed
//! summary suitable for replay verification.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use crate::client::StreamIntegrity;
use crate::sha256::{Sha256, Sha256Digest};
use crate::wire::{Crc32, Frame, FrameParser, FrameType, ParseEvent};

const READ_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PdqSampleRange {
    pub first_sample_index: u64,
    pub end_sample_index_exclusive: u64,
    pub sample_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdqSummary {
    pub path: PathBuf,
    pub frames_written: u64,
    pub sample_frames_written: u64,
    pub samples_written: u64,
    pub bytes_written: u64,
    /// CRC32 over the complete file contents, retained for compatibility
    /// with existing sidecars and quick local checks.
    pub file_crc32: u32,
    /// SHA-256 over the complete file contents for immutable run receipts.
    pub file_sha256: Sha256Digest,
    /// Present only when every sample frame belongs to one contiguous,
    /// constant-rate device-index segment.
    pub sample_range: Option<PdqSampleRange>,
    pub sample_rate_hz: Option<u32>,
    pub sample_segments: u64,
    pub integrity: StreamIntegrity,
    pub valid: bool,
}

impl PdqSummary {
    pub fn file_sha256_hex(&self) -> String {
        self.file_sha256.to_hex()
    }
}

pub struct PdqWriter {
    path: PathBuf,
    file: BufWriter<File>,
    frames_written: u64,
    bytes_written: u64,
    running_crc: Crc32,
    running_sha256: Sha256,
    tracker: FrameTracker,
}

impl PdqWriter {
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::create_with(path.as_ref(), false)
    }

    /// Creates a new evidence file without replacing an existing run.
    pub fn create_new(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::create_with(path.as_ref(), true)
    }

    fn create_with(path: &Path, exclusive: bool) -> io::Result<Self> {
        let path = path.to_owned();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .create_new(exclusive)
            .truncate(!exclusive)
            .open(&path)?;
        Ok(Self {
            file: BufWriter::new(file),
            path,
            frames_written: 0,
            bytes_written: 0,
            running_crc: Crc32::default(),
            running_sha256: Sha256::default(),
            tracker: FrameTracker::default(),
        })
    }

    pub fn write_frame(&mut self, frame: &Frame) -> io::Result<()> {
        let bytes = frame.to_bytes();
        self.file.write_all(&bytes)?;
        self.frames_written += 1;
        self.bytes_written += bytes.len() as u64;
        self.running_crc.update(&bytes);
        self.running_sha256.update(&bytes);
        self.tracker.observe(frame);
        Ok(())
    }

    /// Flushes and closes the file, returning everything needed for a named
    /// finalized receipt. Integrity observed by the live transport is merged
    /// fail-closed with discontinuities inferable from the written frames.
    pub fn finish(mut self, mut integrity: StreamIntegrity) -> io::Result<PdqSummary> {
        self.file.flush()?;
        integrity.sequence_gaps = integrity
            .sequence_gaps
            .max(self.tracker.frame_sequence_gaps);
        integrity.dropped_samples = integrity
            .dropped_samples
            .max(self.tracker.dropped_samples_delta());
        let sample_range = self.tracker.contiguous_sample_range();
        let valid = integrity.is_clean()
            && self.tracker.malformed_sample_frames == 0
            && self.tracker.sample_segments <= 1;
        Ok(PdqSummary {
            file_crc32: self.running_crc.finalize(),
            file_sha256: self.running_sha256.finalize(),
            path: self.path,
            frames_written: self.frames_written,
            sample_frames_written: self.tracker.sample_frames,
            samples_written: self.tracker.samples,
            bytes_written: self.bytes_written,
            sample_range,
            sample_rate_hz: self.tracker.uniform_sample_rate(),
            sample_segments: self.tracker.sample_segments,
            integrity,
            valid,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PdqReadEvent {
    Frame(Frame),
    Corruption {
        skipped_bytes: usize,
        crc_failures: usize,
    },
    TruncatedTail {
        bytes: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdqReadSummary {
    pub frames_read: u64,
    pub sample_frames_read: u64,
    pub samples_read: u64,
    pub bytes_read: u64,
    pub file_crc32: u32,
    pub file_sha256: Sha256Digest,
    pub sample_range: Option<PdqSampleRange>,
    pub sample_rate_hz: Option<u32>,
    pub sample_segments: u64,
    pub malformed_sample_frames: u64,
    pub truncated_bytes: u64,
    pub integrity: StreamIntegrity,
    pub valid: bool,
}

impl PdqReadSummary {
    pub fn file_sha256_hex(&self) -> String {
        self.file_sha256.to_hex()
    }
}

/// Incremental PDA1 file reader. `next_event` preserves corruption notices
/// instead of silently skipping them, allowing replay to continue while the
/// final summary remains invalid.
pub struct PdqReader<R> {
    reader: R,
    parser: FrameParser,
    buffer: Vec<u8>,
    eof: bool,
    tail_reported: bool,
    frames_read: u64,
    bytes_read: u64,
    running_crc: Crc32,
    running_sha256: Sha256,
    tracker: FrameTracker,
    integrity: StreamIntegrity,
    truncated_bytes: u64,
}

impl PdqReader<File> {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        File::open(path).map(Self::new)
    }
}

impl<R: Read> PdqReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            parser: FrameParser::default(),
            buffer: vec![0; READ_BUFFER_BYTES],
            eof: false,
            tail_reported: false,
            frames_read: 0,
            bytes_read: 0,
            running_crc: Crc32::default(),
            running_sha256: Sha256::default(),
            tracker: FrameTracker::default(),
            integrity: StreamIntegrity::default(),
            truncated_bytes: 0,
        }
    }

    pub fn next_event(&mut self) -> io::Result<Option<PdqReadEvent>> {
        loop {
            if let Some(event) = self.parser.next_event() {
                return Ok(Some(match event {
                    ParseEvent::Frame(frame) => {
                        self.frames_read += 1;
                        self.tracker.observe(&frame);
                        PdqReadEvent::Frame(frame)
                    }
                    ParseEvent::Corruption {
                        skipped_bytes,
                        crc_failures,
                    } => {
                        self.integrity.skipped_bytes += skipped_bytes as u64;
                        self.integrity.crc_failures += crc_failures as u64;
                        PdqReadEvent::Corruption {
                            skipped_bytes,
                            crc_failures,
                        }
                    }
                }));
            }

            if self.eof {
                if !self.tail_reported && self.parser.buffered_len() > 0 {
                    self.tail_reported = true;
                    let bytes = self.parser.discard_buffered();
                    self.truncated_bytes += bytes as u64;
                    self.integrity.skipped_bytes += bytes as u64;
                    return Ok(Some(PdqReadEvent::TruncatedTail { bytes }));
                }
                return Ok(None);
            }

            let read = match self.reader.read(&mut self.buffer) {
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            if read == 0 {
                self.eof = true;
                continue;
            }
            let bytes = &self.buffer[..read];
            self.bytes_read += read as u64;
            self.running_crc.update(bytes);
            self.running_sha256.update(bytes);
            self.parser.extend(bytes);
        }
    }

    /// Drains the remaining file and returns an independently verified
    /// summary. This can follow any number of prior `next_event` calls.
    pub fn finish(mut self) -> io::Result<PdqReadSummary> {
        while self.next_event()?.is_some() {}
        self.integrity.sequence_gaps = self.tracker.frame_sequence_gaps;
        self.integrity.dropped_samples = self.tracker.dropped_samples_delta();
        let sample_range = self.tracker.contiguous_sample_range();
        let valid = self.integrity.is_clean()
            && self.truncated_bytes == 0
            && self.tracker.malformed_sample_frames == 0
            && self.tracker.sample_segments <= 1;
        Ok(PdqReadSummary {
            frames_read: self.frames_read,
            sample_frames_read: self.tracker.sample_frames,
            samples_read: self.tracker.samples,
            bytes_read: self.bytes_read,
            file_crc32: self.running_crc.finalize(),
            file_sha256: self.running_sha256.finalize(),
            sample_range,
            sample_rate_hz: self.tracker.uniform_sample_rate(),
            sample_segments: self.tracker.sample_segments,
            malformed_sample_frames: self.tracker.malformed_sample_frames,
            truncated_bytes: self.truncated_bytes,
            integrity: self.integrity,
            valid,
        })
    }
}

pub fn inspect_pdq(path: impl AsRef<Path>) -> io::Result<PdqReadSummary> {
    PdqReader::open(path)?.finish()
}

#[derive(Default)]
struct FrameTracker {
    previous_frame_sequence: Option<u32>,
    frame_sequence_gaps: u64,
    sample_frames: u64,
    samples: u64,
    first_sample_index: Option<u64>,
    last_sample_end: Option<u64>,
    previous_sample_end: Option<u64>,
    first_sample_rate_hz: Option<u32>,
    previous_sample_rate_hz: Option<u32>,
    sample_rate_changed: bool,
    sample_segments: u64,
    malformed_sample_frames: u64,
    first_dropped_samples: Option<u32>,
    last_dropped_samples: Option<u32>,
}

impl FrameTracker {
    fn observe(&mut self, frame: &Frame) {
        if let Some(previous) = self.previous_frame_sequence {
            if frame.header.sequence != previous.wrapping_add(1) {
                self.frame_sequence_gaps += 1;
            }
        }
        self.previous_frame_sequence = Some(frame.header.sequence);
        self.first_dropped_samples
            .get_or_insert(frame.header.dropped_samples);
        self.last_dropped_samples = Some(frame.header.dropped_samples);

        if frame.header.frame_type != FrameType::SamplesU16 {
            return;
        }
        if !frame.payload.len().is_multiple_of(2) {
            self.malformed_sample_frames += 1;
            return;
        }
        let count = (frame.payload.len() / 2) as u64;
        if count == 0 {
            return;
        }
        let first = frame.header.first_sample_index;
        let end = first.saturating_add(count);
        let starts_new_segment = self.previous_sample_end.is_none()
            || self.previous_sample_end != Some(first)
            || self.previous_sample_rate_hz != Some(frame.header.sample_rate_hz);
        if starts_new_segment {
            self.sample_segments += 1;
        }
        if self
            .first_sample_rate_hz
            .is_some_and(|rate| rate != frame.header.sample_rate_hz)
        {
            self.sample_rate_changed = true;
        }
        self.first_sample_rate_hz
            .get_or_insert(frame.header.sample_rate_hz);
        self.previous_sample_rate_hz = Some(frame.header.sample_rate_hz);
        self.first_sample_index.get_or_insert(first);
        self.last_sample_end = Some(end);
        self.previous_sample_end = Some(end);
        self.sample_frames += 1;
        self.samples += count;
    }

    fn dropped_samples_delta(&self) -> u64 {
        match (self.first_dropped_samples, self.last_dropped_samples) {
            (Some(first), Some(last)) => u64::from(last.saturating_sub(first)),
            _ => 0,
        }
    }

    fn uniform_sample_rate(&self) -> Option<u32> {
        (!self.sample_rate_changed)
            .then_some(self.first_sample_rate_hz)
            .flatten()
    }

    fn contiguous_sample_range(&self) -> Option<PdqSampleRange> {
        if self.sample_segments != 1 || self.malformed_sample_frames > 0 {
            return None;
        }
        Some(PdqSampleRange {
            first_sample_index: self.first_sample_index?,
            end_sample_index_exclusive: self.last_sample_end?,
            sample_count: self.samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{FrameHeader, PROTOCOL_VERSION};
    use std::io::Cursor;

    fn control_frame(sequence: u32) -> Frame {
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
            format!("+{sequence} OK").into_bytes(),
        )
    }

    fn sample_frame(sequence: u32, first_index: u64, rate_hz: u32, codes: &[u16]) -> Frame {
        Frame::build(
            FrameHeader {
                version: PROTOCOL_VERSION,
                frame_type: FrameType::SamplesU16,
                flags: 0,
                sequence,
                payload_bytes: 0,
                first_sample_index: first_index,
                sample_rate_hz: rate_hz,
                dropped_samples: 0,
                crc32: 0,
            },
            codes.iter().flat_map(|code| code.to_le_bytes()).collect(),
        )
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "stage-a-io-pdq-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn writer_and_reader_agree_on_digest_size_and_sample_range() {
        let dir = temp_dir("receipt");
        let path = dir.join("run.pdq");
        let frames = [
            sample_frame(10, 1_000, 20_000, &[1, 2, 3]),
            sample_frame(11, 1_003, 20_000, &[4, 5]),
        ];
        let mut writer = PdqWriter::create(&path).expect("create pdq");
        for frame in &frames {
            writer.write_frame(frame).expect("write frame");
        }
        let written = writer
            .finish(StreamIntegrity::default())
            .expect("finish writer");
        let read = inspect_pdq(&path).expect("inspect pdq");

        assert!(written.valid && read.valid);
        assert_eq!(written.frames_written, 2);
        assert_eq!(written.sample_frames_written, 2);
        assert_eq!(written.samples_written, 5);
        assert_eq!(written.bytes_written, read.bytes_read);
        assert_eq!(written.file_crc32, read.file_crc32);
        assert_eq!(written.file_sha256, read.file_sha256);
        assert_eq!(written.file_sha256_hex().len(), 64);
        assert_eq!(
            written.sample_range,
            Some(PdqSampleRange {
                first_sample_index: 1_000,
                end_sample_index_exclusive: 1_005,
                sample_count: 5,
            })
        );
        assert_eq!(written.sample_range, read.sample_range);
        assert_eq!(written.sample_rate_hz, Some(20_000));

        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn reader_streams_frames_and_reports_crc_corruption() {
        let first = control_frame(1).to_bytes();
        let mut corrupt = control_frame(2).to_bytes();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x80;
        let third = control_frame(3).to_bytes();
        let bytes: Vec<u8> = first.into_iter().chain(corrupt).chain(third).collect();
        let mut reader = PdqReader::new(Cursor::new(bytes));
        let mut frames = Vec::new();
        let mut saw_corruption = false;
        while let Some(event) = reader.next_event().expect("read event") {
            match event {
                PdqReadEvent::Frame(frame) => frames.push(frame.header.sequence),
                PdqReadEvent::Corruption { crc_failures, .. } => {
                    saw_corruption |= crc_failures > 0;
                }
                PdqReadEvent::TruncatedTail { .. } => {}
            }
        }
        assert_eq!(frames, [1, 3]);
        assert!(saw_corruption);
        let summary = reader.finish().expect("finish after iteration");
        assert!(!summary.valid);
        assert_eq!(summary.integrity.crc_failures, 1);
        assert_eq!(summary.integrity.sequence_gaps, 1);
    }

    #[test]
    fn truncated_tail_is_visible_and_invalid() {
        let mut bytes = sample_frame(1, 0, 20_000, &[1, 2, 3]).to_bytes();
        bytes.extend_from_slice(b"PDA");
        let mut reader = PdqReader::new(Cursor::new(bytes));
        let mut truncated = 0;
        while let Some(event) = reader.next_event().expect("read") {
            if let PdqReadEvent::TruncatedTail { bytes } = event {
                truncated += bytes;
            }
        }
        assert_eq!(truncated, 3);
        let summary = reader.finish().expect("finish");
        assert_eq!(summary.truncated_bytes, 3);
        assert!(!summary.valid);
    }

    #[test]
    fn discontinuous_samples_have_no_contiguous_range() {
        let first = sample_frame(4, 100, 20_000, &[1, 2]).to_bytes();
        let second = sample_frame(5, 900, 50_000, &[3, 4]).to_bytes();
        let bytes: Vec<u8> = first.into_iter().chain(second).collect();
        let summary = PdqReader::new(Cursor::new(bytes))
            .finish()
            .expect("inspect");
        assert_eq!(summary.sample_segments, 2);
        assert_eq!(summary.sample_range, None);
        assert_eq!(summary.sample_rate_hz, None);
        assert!(!summary.valid);
    }

    #[test]
    fn explicit_integrity_faults_invalidate_writer_but_keep_the_file() {
        let dir = temp_dir("invalid");
        let path = dir.join("run.pdq");
        let mut writer = PdqWriter::create(&path).expect("create pdq");
        writer.write_frame(&control_frame(1)).expect("write frame");
        let summary = writer
            .finish(StreamIntegrity {
                dropped_samples: 5,
                ..StreamIntegrity::default()
            })
            .expect("finish pdq");

        assert!(!summary.valid);
        assert!(path.exists(), "evidence file is preserved");
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
