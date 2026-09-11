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
            // Windows opens a port with DTR deasserted; macOS and Linux assert
            // it for us. A Teensy sketch that gates on `if (Serial)` would stay
            // silent there, so assert it everywhere (ADR 032).
            .dtr_on_open(true)
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

/// One serial port as the OS enumerated it.
///
/// `label` is the human-readable USB manufacturer/product where the OS
/// reports one — e.g. `"Teensyduino Dual Serial"` — so port pickers can show
/// which entry is the Teensy. `is_usb` records whether the OS classified the
/// port as a USB device at all, which is the only device hint Windows gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortInfo {
    pub name: String,
    pub label: Option<String>,
    pub is_usb: bool,
}

impl PortInfo {
    /// The port as a picker entry: the path, plus the USB label in parentheses
    /// where there is one. Only the leading path is the value — see
    /// `variant_path` in the Stage-A plugins.
    pub fn variant(&self) -> String {
        match &self.label {
            Some(label) => format!("{} ({label})", self.name),
            None => self.name.clone(),
        }
    }
}

/// Every serial port visible to the OS (empty without the `hardware` feature).
#[cfg(feature = "hardware")]
pub fn available_ports() -> Vec<PortInfo> {
    serialport::available_ports()
        .map(|ports| {
            ports
                .into_iter()
                .map(|p| {
                    let (label, is_usb) = match p.port_type {
                        serialport::SerialPortType::UsbPort(info) => {
                            let label = match (info.manufacturer, info.product) {
                                (Some(manufacturer), Some(product))
                                    if !product.starts_with(&manufacturer) =>
                                {
                                    Some(format!("{manufacturer} {product}"))
                                }
                                (_, Some(product)) => Some(product),
                                (Some(manufacturer), None) => Some(manufacturer),
                                (None, None) => None,
                            };
                            (label, true)
                        }
                        _ => (None, false),
                    };
                    PortInfo {
                        name: p.port_name,
                        label,
                        is_usb,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(feature = "hardware"))]
pub fn available_ports() -> Vec<PortInfo> {
    Vec::new()
}

/// The enumerated ports worth probing for a Teensy.
///
/// Unix names the device, so the name is the filter: macOS lists every device
/// twice (`tty.*` and `cu.*`) and only the callout node may be opened, and a
/// Teensy's CDC-ACM class node on Linux is `ttyACM*`. Windows names nothing —
/// every port is `COMn` — so USB-ness is the only signal there, and if the OS
/// classified no port at all the whole list is probed rather than none. What
/// actually identifies the Teensy is the probe (HELLO on the command port,
/// PDA1 sample frames on the stream port); this only keeps the probe from
/// stalling on unrelated ports such as Windows' phantom Bluetooth COM entries.
pub fn candidate_ports() -> Vec<PortInfo> {
    narrow_to_candidates(available_ports(), cfg!(windows))
}

fn narrow_to_candidates(ports: Vec<PortInfo>, windows: bool) -> Vec<PortInfo> {
    if !windows {
        return ports
            .into_iter()
            .filter(|port| port.name.contains("cu.usbmodem") || port.name.contains("ttyACM"))
            .collect();
    }
    if ports.iter().any(|port| port.is_usb) {
        return ports.into_iter().filter(|port| port.is_usb).collect();
    }
    ports
}

/// Why there was nothing to probe, naming what the OS did enumerate — the
/// difference between "no device is attached" and "a device is attached but
/// this platform's filter dropped it" is the operator's next step.
pub fn no_candidate_ports_message() -> String {
    let ports = available_ports();
    if ports.is_empty() {
        return "no serial ports found — check the USB cable and that the Teensy is powered"
            .to_owned();
    }
    let seen = ports
        .iter()
        .map(PortInfo::variant)
        .collect::<Vec<_>>()
        .join(", ");
    format!("no serial port looked like a Teensy (the OS offered {seen})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(name: &str, is_usb: bool) -> PortInfo {
        PortInfo {
            name: name.to_owned(),
            label: None,
            is_usb,
        }
    }

    fn names(ports: Vec<PortInfo>) -> Vec<String> {
        ports.into_iter().map(|port| port.name).collect()
    }

    #[test]
    fn unix_keeps_the_callout_node_and_drops_its_tty_twin() {
        let ports = vec![
            port("/dev/tty.usbmodem12345", true),
            port("/dev/cu.usbmodem12345", true),
            port("/dev/cu.Bluetooth-Incoming-Port", false),
        ];
        assert_eq!(
            names(narrow_to_candidates(ports, false)),
            vec!["/dev/cu.usbmodem12345"]
        );
    }

    #[test]
    fn unix_keeps_the_linux_cdc_acm_node() {
        let ports = vec![port("/dev/ttyACM0", true), port("/dev/ttyS0", false)];
        assert_eq!(
            names(narrow_to_candidates(ports, false)),
            vec!["/dev/ttyACM0"]
        );
    }

    #[test]
    fn windows_com_ports_survive_the_unix_name_filter() {
        // The bug: COMn matches neither `cu.usbmodem` nor `ttyACM`, so the
        // Teensy's two ports were filtered out before any probe could run.
        let ports = vec![port("COM3", true), port("COM4", true)];
        assert_eq!(
            names(narrow_to_candidates(ports, true)),
            vec!["COM3", "COM4"]
        );
    }

    #[test]
    fn windows_drops_non_usb_ports_when_a_usb_port_exists() {
        let ports = vec![port("COM1", false), port("COM7", true)];
        assert_eq!(names(narrow_to_candidates(ports, true)), vec!["COM7"]);
    }

    #[test]
    fn windows_probes_everything_when_the_os_classifies_nothing() {
        let ports = vec![port("COM1", false), port("COM3", false)];
        assert_eq!(
            names(narrow_to_candidates(ports, true)),
            vec!["COM1", "COM3"]
        );
    }

    #[test]
    fn a_labelled_port_shows_its_usb_name_in_the_picker() {
        let labelled = PortInfo {
            name: "COM3".to_owned(),
            label: Some("Teensyduino Dual Serial".to_owned()),
            is_usb: true,
        };
        assert_eq!(labelled.variant(), "COM3 (Teensyduino Dual Serial)");
        assert_eq!(port("COM4", true).variant(), "COM4");
    }
}
