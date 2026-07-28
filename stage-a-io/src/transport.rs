//! Byte transports: the real USB serial port and an in-memory mock.
//!
//! Exactly one armed plugin owns the port at a time; opening a busy device
//! is a visible error, never a silent second connection (the OS enforces
//! exclusivity via `serialport`'s exclusive open on POSIX).

use std::io;
use std::sync::{Arc, Mutex};
#[cfg(feature = "hardware")]
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

/// Names of serial ports visible to the OS (empty without the `hardware`
/// feature). Used by plugins to offer a port picker.
#[cfg(feature = "hardware")]
pub fn available_port_names() -> Vec<String> {
    serialport::available_ports()
        .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default()
}

#[cfg(not(feature = "hardware"))]
pub fn available_port_names() -> Vec<String> {
    Vec::new()
}

/// Port names plus a human-readable USB label (manufacturer/product) where
/// the OS provides one — e.g. `("/dev/cu.usbmodem…", Some("Teensyduino Dual
/// Serial"))`. Lets port pickers show which entry is the Teensy.
#[cfg(feature = "hardware")]
pub fn available_ports_with_labels() -> Vec<(String, Option<String>)> {
    serialport::available_ports()
        .map(|ports| {
            ports
                .into_iter()
                .map(|p| {
                    let label = match p.port_type {
                        serialport::SerialPortType::UsbPort(info) => {
                            match (info.manufacturer, info.product) {
                                (Some(manufacturer), Some(product))
                                    if !product.starts_with(&manufacturer) =>
                                {
                                    Some(format!("{manufacturer} {product}"))
                                }
                                (_, Some(product)) => Some(product),
                                (Some(manufacturer), None) => Some(manufacturer),
                                (None, None) => None,
                            }
                        }
                        _ => None,
                    };
                    (p.port_name, label)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(feature = "hardware"))]
pub fn available_ports_with_labels() -> Vec<(String, Option<String>)> {
    Vec::new()
}
