/// Event-camera polarity. A1 always analyses ON and OFF separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Polarity {
    On,
    Off,
}

impl Polarity {
    pub const ALL: [Self; 2] = [Self::On, Self::Off];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

/// The event fields needed by the pure A1 analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraEvent {
    pub timestamp_us: u64,
    pub x: u16,
    pub y: u16,
    pub polarity: Polarity,
}
