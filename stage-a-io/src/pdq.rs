//! `.pdq` writer: preserves every valid PDA1 frame verbatim on disk and
//! tracks run validity.
//!
//! Raw ADC waveforms belong in the PDQ file, never in `HostContext` JSON or
//! per-frame plugin output. A CRC error, frame-sequence gap, or nonzero
//! dropped-sample counter invalidates the run — the file is still written
//! (evidence), but the sidecar must record `valid = false`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::client::StreamIntegrity;
use crate::wire::{Crc32, Frame};

pub struct PdqWriter {
    path: PathBuf,
    file: BufWriter<File>,
    frames_written: u64,
    bytes_written: u64,
    running_crc: Crc32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdqSummary {
    pub path: PathBuf,
    pub frames_written: u64,
    pub bytes_written: u64,
    /// CRC32 over the whole file contents, recorded in the sidecar.
    pub file_crc32: u32,
    pub integrity: StreamIntegrity,
    pub valid: bool,
}

impl PdqWriter {
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_owned();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            file: BufWriter::new(File::create(&path)?),
            path,
            frames_written: 0,
            bytes_written: 0,
            running_crc: Crc32::default(),
        })
    }

    pub fn write_frame(&mut self, frame: &Frame) -> std::io::Result<()> {
        let bytes = frame.to_bytes();
        self.file.write_all(&bytes)?;
        self.frames_written += 1;
        self.bytes_written += bytes.len() as u64;
        self.running_crc.update(&bytes);
        Ok(())
    }

    /// Flushes and closes the file, returning the summary for the sidecar.
    pub fn finish(mut self, integrity: StreamIntegrity) -> std::io::Result<PdqSummary> {
        self.file.flush()?;
        Ok(PdqSummary {
            file_crc32: self.running_crc.finalize(),
            path: self.path,
            frames_written: self.frames_written,
            bytes_written: self.bytes_written,
            valid: integrity.is_clean(),
            integrity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{FrameHeader, FrameType, PROTOCOL_VERSION};

    fn frame(sequence: u32) -> Frame {
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

    #[test]
    fn writes_frames_verbatim_and_reports_validity() {
        let dir = std::env::temp_dir().join(format!(
            "stage-a-io-pdq-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("run.pdq");

        let mut writer = PdqWriter::create(&path).expect("create pdq");
        let first = frame(1);
        let second = frame(2);
        writer.write_frame(&first).expect("write");
        writer.write_frame(&second).expect("write");
        let summary = writer
            .finish(StreamIntegrity::default())
            .expect("finish pdq");

        assert!(summary.valid);
        assert_eq!(summary.frames_written, 2);
        let on_disk = std::fs::read(&path).expect("read back");
        let mut expected = first.to_bytes();
        expected.extend_from_slice(&second.to_bytes());
        assert_eq!(on_disk, expected);
        assert_eq!(summary.file_crc32, crate::wire::crc32(&expected));

        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[test]
    fn integrity_faults_invalidate_the_run_but_keep_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "stage-a-io-pdq-invalid-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("run.pdq");

        let mut writer = PdqWriter::create(&path).expect("create pdq");
        writer.write_frame(&frame(1)).expect("write");
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
