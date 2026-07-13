//! Byte transports: the real USB serial port and an in-memory mock.
//!
//! Exactly one armed plugin owns the port at a time; opening a busy device
//! is a visible error, never a silent second connection (the OS enforces
//! exclusivity via `serialport`'s exclusive open on POSIX).

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub trait Transport: Send {
    /// Reads whatever is available into `buf`, blocking up to the
    /// transport's timeout. `Ok(0)` means "nothing arrived this poll".
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
}

/// Real serial port. Construction fails visibly if the device is busy or
/// absent.
#[cfg(feature = "hardware")]
pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
}

#[cfg(feature = "hardware")]
impl SerialTransport {
    pub fn open(path: &str, baud: u32, poll_timeout: Duration) -> io::Result<Self> {
        let port = serialport::new(path, baud)
            .timeout(poll_timeout)
            .open()
            .map_err(|err| io::Error::other(format!("opening {path} failed: {err}")))?;
        Ok(Self { port })
    }
}

#[cfg(feature = "hardware")]
impl Transport for SerialTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(err) => Err(err),
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        io::Write::write_all(&mut self.port, bytes)
    }
}

/// Shared in-memory duplex used by tests and the mock controller: the
/// "host" side reads what the "device" side wrote and vice versa.
#[derive(Default)]
struct DuplexState {
    to_host: Vec<u8>,
    to_device: Vec<u8>,
}

#[derive(Clone, Default)]
pub struct MockLink {
    state: Arc<Mutex<DuplexState>>,
}

impl MockLink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn host_end(&self) -> MockTransport {
        MockTransport {
            state: Arc::clone(&self.state),
            is_host: true,
        }
    }

    pub fn device_end(&self) -> MockTransport {
        MockTransport {
            state: Arc::clone(&self.state),
            is_host: false,
        }
    }
}

pub struct MockTransport {
    state: Arc<Mutex<DuplexState>>,
    is_host: bool,
}

impl Transport for MockTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let source = if self.is_host {
            &mut state.to_host
        } else {
            &mut state.to_device
        };
        let n = source.len().min(buf.len());
        buf[..n].copy_from_slice(&source[..n]);
        source.drain(..n);
        Ok(n)
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let sink = if self.is_host {
            &mut state.to_device
        } else {
            &mut state.to_host
        };
        sink.extend_from_slice(bytes);
        Ok(())
    }
}
