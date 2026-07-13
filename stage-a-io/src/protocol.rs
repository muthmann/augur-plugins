//! ASCII command / control-reply grammar (host → Teensy and CONTROL frame
//! payloads), per the Stage-A serial protocol v1:
//!
//! ```text
//! Host request:          @<seq> <VERB> key=value key=value\n
//! CONTROL reply payload: +<seq> OK key=value ...
//! CONTROL reply payload: -<seq> ERR code=<CODE> detail=<TOKEN>
//! Async CONTROL payload: !<NAME> key=value ...
//! ```
//!
//! Commands are printable ASCII, max 192 bytes, integer values only.

use std::collections::BTreeMap;
use std::fmt;

pub const MAX_COMMAND_BYTES: usize = 192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub verb: String,
    /// Ordered key=value fields (insertion order is preserved on the wire;
    /// a BTreeMap would silently reorder, so use a Vec of pairs).
    pub fields: Vec<(String, String)>,
}

impl Command {
    pub fn new(verb: &str) -> Self {
        Self {
            verb: verb.to_owned(),
            fields: Vec::new(),
        }
    }

    pub fn field(mut self, key: &str, value: impl fmt::Display) -> Self {
        self.fields.push((key.to_owned(), value.to_string()));
        self
    }

    /// Encodes `@<seq> VERB k=v ...\n`, validating the printable-ASCII and
    /// length constraints.
    pub fn encode(&self, sequence: u32) -> Result<Vec<u8>, ProtocolError> {
        let mut line = format!("@{sequence} {}", self.verb);
        for (key, value) in &self.fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(value);
        }
        line.push('\n');
        if line.len() > MAX_COMMAND_BYTES {
            return Err(ProtocolError::CommandTooLong(line.len()));
        }
        if !line
            .bytes()
            .all(|b| b == b'\n' || (0x20..=0x7E).contains(&b))
        {
            return Err(ProtocolError::NonPrintable);
        }
        Ok(line.into_bytes())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    /// `+<seq> OK key=value ...`
    Ok {
        sequence: u32,
        fields: BTreeMap<String, String>,
    },
    /// `-<seq> ERR code=<CODE> detail=<TOKEN>`
    Err {
        sequence: u32,
        code: String,
        detail: String,
    },
    /// `!<NAME> key=value ...`
    Async {
        name: String,
        fields: BTreeMap<String, String>,
    },
}

impl ControlMessage {
    pub fn parse(text: &str) -> Result<Self, ProtocolError> {
        let text = text.trim_end_matches(['\r', '\n']);
        let mut parts = text.split_ascii_whitespace();
        let head = parts.next().ok_or(ProtocolError::EmptyControl)?;
        match head.as_bytes().first() {
            Some(b'+') => {
                let sequence = head[1..]
                    .parse()
                    .map_err(|_| ProtocolError::BadSequence(head.to_owned()))?;
                let ok = parts.next();
                if ok != Some("OK") {
                    return Err(ProtocolError::Malformed(text.to_owned()));
                }
                Ok(Self::Ok {
                    sequence,
                    fields: parse_fields(parts),
                })
            }
            Some(b'-') => {
                let sequence = head[1..]
                    .parse()
                    .map_err(|_| ProtocolError::BadSequence(head.to_owned()))?;
                let err = parts.next();
                if err != Some("ERR") {
                    return Err(ProtocolError::Malformed(text.to_owned()));
                }
                let fields = parse_fields(parts);
                Ok(Self::Err {
                    sequence,
                    code: fields.get("code").cloned().unwrap_or_default(),
                    detail: fields.get("detail").cloned().unwrap_or_default(),
                })
            }
            Some(b'!') => Ok(Self::Async {
                name: head[1..].to_owned(),
                fields: parse_fields(parts),
            }),
            _ => Err(ProtocolError::Malformed(text.to_owned())),
        }
    }

    pub fn sequence(&self) -> Option<u32> {
        match self {
            Self::Ok { sequence, .. } | Self::Err { sequence, .. } => Some(*sequence),
            Self::Async { .. } => None,
        }
    }
}

fn parse_fields<'a>(parts: impl Iterator<Item = &'a str>) -> BTreeMap<String, String> {
    parts
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    CommandTooLong(usize),
    NonPrintable,
    EmptyControl,
    BadSequence(String),
    Malformed(String),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandTooLong(len) => {
                write!(f, "command is {len} bytes (max {MAX_COMMAND_BYTES})")
            }
            Self::NonPrintable => f.write_str("command contains non-printable bytes"),
            Self::EmptyControl => f.write_str("empty control payload"),
            Self::BadSequence(head) => write!(f, "unparseable sequence in {head:?}"),
            Self::Malformed(text) => write!(f, "malformed control payload {text:?}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_commands_with_ordered_fields() {
        let cmd = Command::new("CONFIG")
            .field("mode", "A1")
            .field("freq_mhz", 12_500)
            .field("center_dac", 2_048)
            .field("amplitude_dac", 512);
        assert_eq!(
            String::from_utf8(cmd.encode(3).expect("encodes")).unwrap(),
            "@3 CONFIG mode=A1 freq_mhz=12500 center_dac=2048 amplitude_dac=512\n"
        );
    }

    #[test]
    fn rejects_oversized_and_non_printable_commands() {
        let long = Command::new("X").field("k", "y".repeat(200));
        assert!(matches!(
            long.encode(1),
            Err(ProtocolError::CommandTooLong(_))
        ));
        let bad = Command::new("X").field("k", "\u{7f}");
        assert!(matches!(bad.encode(1), Err(ProtocolError::NonPrintable)));
    }

    #[test]
    fn parses_ok_err_and_async_payloads() {
        let ok = ControlMessage::parse("+12 OK state=ARMED rev=4").expect("ok parses");
        match ok {
            ControlMessage::Ok { sequence, fields } => {
                assert_eq!(sequence, 12);
                assert_eq!(fields.get("rev").map(String::as_str), Some("4"));
            }
            other => panic!("unexpected {other:?}"),
        }

        let err =
            ControlMessage::parse("-13 ERR code=BOUNDS detail=amplitude_dac").expect("err parses");
        assert_eq!(
            err,
            ControlMessage::Err {
                sequence: 13,
                code: "BOUNDS".into(),
                detail: "amplitude_dac".into()
            }
        );

        let async_msg = ControlMessage::parse("!APPLIED rev=4").expect("async parses");
        match async_msg {
            ControlMessage::Async { name, fields } => {
                assert_eq!(name, "APPLIED");
                assert_eq!(fields.get("rev").map(String::as_str), Some("4"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
