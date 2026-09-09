//! EXT_TRIGGER marker validation and camera-clock phase folding.

use crate::types::{CameraEvent, Polarity};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkerValidationConfig {
    pub expected_frequency_hz: f64,
    pub frequency_tolerance_fraction: f64,
    pub max_period_jitter_fraction: f64,
    /// Expected complete cycles, when the acquisition declared one.
    pub expected_cycles: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarkerValidation {
    pub cycle_count: usize,
    pub measured_frequency_hz: f64,
    pub mean_period_us: f64,
    pub max_period_jitter_fraction: f64,
    pub first_marker_us: u64,
    pub last_marker_us: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MarkerError {
    InvalidConfiguration(&'static str),
    TooFewMarkers {
        count: usize,
    },
    NonIncreasing {
        index: usize,
    },
    CycleCount {
        expected: usize,
        actual: usize,
    },
    FrequencyOutOfTolerance {
        expected_hz: f64,
        measured_hz: f64,
        tolerance_fraction: f64,
    },
    JitterOutOfTolerance {
        measured_fraction: f64,
        tolerance_fraction: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FoldedEvent {
    pub timestamp_us: u64,
    pub x: u16,
    pub y: u16,
    pub polarity: Polarity,
    pub cycle_index: usize,
    /// Circular phase in `[0, 1)`.
    pub phase: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhaseFold {
    pub markers_us: Vec<u64>,
    pub validation: MarkerValidation,
    pub events: Vec<FoldedEvent>,
    pub events_outside_complete_cycles: usize,
}

impl PhaseFold {
    pub fn phase_at(&self, timestamp_us: u64) -> f64 {
        let period_us = self.validation.mean_period_us;
        (timestamp_us.saturating_sub(self.validation.first_marker_us) as f64 / period_us)
            .rem_euclid(1.0)
    }
}

pub fn validate_markers(
    markers_us: &[u64],
    config: MarkerValidationConfig,
) -> Result<MarkerValidation, MarkerError> {
    if !config.expected_frequency_hz.is_finite() || config.expected_frequency_hz <= 0.0 {
        return Err(MarkerError::InvalidConfiguration(
            "expected frequency must be finite and positive",
        ));
    }
    if !config.frequency_tolerance_fraction.is_finite()
        || config.frequency_tolerance_fraction < 0.0
        || !config.max_period_jitter_fraction.is_finite()
        || config.max_period_jitter_fraction < 0.0
    {
        return Err(MarkerError::InvalidConfiguration(
            "marker tolerances must be finite and non-negative",
        ));
    }
    if markers_us.len() < 2 {
        return Err(MarkerError::TooFewMarkers {
            count: markers_us.len(),
        });
    }

    let mut periods = Vec::with_capacity(markers_us.len() - 1);
    for (index, pair) in markers_us.windows(2).enumerate() {
        if pair[1] <= pair[0] {
            return Err(MarkerError::NonIncreasing { index: index + 1 });
        }
        periods.push((pair[1] - pair[0]) as f64);
    }

    let cycle_count = periods.len();
    if let Some(expected) = config.expected_cycles {
        if cycle_count != expected {
            return Err(MarkerError::CycleCount {
                expected,
                actual: cycle_count,
            });
        }
    }
    let mean_period_us = periods.iter().sum::<f64>() / cycle_count as f64;
    let measured_frequency_hz = 1_000_000.0 / mean_period_us;
    let frequency_error = ((measured_frequency_hz - config.expected_frequency_hz)
        / config.expected_frequency_hz)
        .abs();
    if frequency_error > config.frequency_tolerance_fraction {
        return Err(MarkerError::FrequencyOutOfTolerance {
            expected_hz: config.expected_frequency_hz,
            measured_hz: measured_frequency_hz,
            tolerance_fraction: config.frequency_tolerance_fraction,
        });
    }

    let max_period_jitter_fraction = periods
        .iter()
        .map(|period| ((period - mean_period_us) / mean_period_us).abs())
        .fold(0.0_f64, f64::max);
    if max_period_jitter_fraction > config.max_period_jitter_fraction {
        return Err(MarkerError::JitterOutOfTolerance {
            measured_fraction: max_period_jitter_fraction,
            tolerance_fraction: config.max_period_jitter_fraction,
        });
    }

    Ok(MarkerValidation {
        cycle_count,
        measured_frequency_hz,
        mean_period_us,
        max_period_jitter_fraction,
        first_marker_us: markers_us[0],
        last_marker_us: *markers_us.last().expect("at least two markers"),
    })
}

/// Folds events against a free-running modulation period, with the phase
/// origin placed at the first event. This is the "phase-0 unanchored" path
/// used until a hardware `EXT_TRIGGER` reaches the camera: bins are relative
/// to the first event, not tied to the drive waveform. Only events inside the
/// whole-cycle span are retained so the rate normalisation matches
/// `cycle_count`. Returns `None` when the period is invalid or the window does
/// not cover at least one whole cycle.
pub fn fold_events_free_running(events: &[CameraEvent], period_us: f64) -> Option<PhaseFold> {
    if !period_us.is_finite() || period_us <= 0.0 || events.is_empty() {
        return None;
    }
    let first = events.iter().map(|event| event.timestamp_us).min()?;
    let last = events.iter().map(|event| event.timestamp_us).max()?;
    let cycle_count = ((last.saturating_sub(first)) as f64 / period_us).floor() as usize;
    if cycle_count == 0 {
        return None;
    }

    let mut folded = Vec::with_capacity(events.len());
    let mut outside = 0;
    for event in events {
        let cycles = event.timestamp_us.saturating_sub(first) as f64 / period_us;
        let cycle_index = cycles.floor() as usize;
        if cycle_index >= cycle_count {
            outside += 1;
            continue;
        }
        folded.push(FoldedEvent {
            timestamp_us: event.timestamp_us,
            x: event.x,
            y: event.y,
            polarity: event.polarity,
            cycle_index,
            phase: cycles.fract(),
        });
    }

    Some(PhaseFold {
        markers_us: Vec::new(),
        validation: MarkerValidation {
            cycle_count,
            measured_frequency_hz: 1_000_000.0 / period_us,
            mean_period_us: period_us,
            max_period_jitter_fraction: 0.0,
            first_marker_us: first,
            last_marker_us: first + (cycle_count as f64 * period_us).round() as u64,
        },
        events: folded,
        events_outside_complete_cycles: outside,
    })
}

pub fn fold_events(
    events: &[CameraEvent],
    markers_us: &[u64],
    config: MarkerValidationConfig,
) -> Result<PhaseFold, MarkerError> {
    let validation = validate_markers(markers_us, config)?;
    let mut folded = Vec::with_capacity(events.len());
    let mut outside = 0;

    for event in events {
        let cycle_index = match markers_us.binary_search(&event.timestamp_us) {
            Ok(index) if index + 1 < markers_us.len() => index,
            Ok(_) => {
                outside += 1;
                continue;
            }
            Err(0) => {
                outside += 1;
                continue;
            }
            Err(index) if index < markers_us.len() => index - 1,
            Err(_) => {
                outside += 1;
                continue;
            }
        };
        let start = markers_us[cycle_index];
        let end = markers_us[cycle_index + 1];
        let phase = (event.timestamp_us - start) as f64 / (end - start) as f64;
        folded.push(FoldedEvent {
            timestamp_us: event.timestamp_us,
            x: event.x,
            y: event.y,
            polarity: event.polarity,
            cycle_index,
            phase,
        });
    }

    Ok(PhaseFold {
        markers_us: markers_us.to_vec(),
        validation,
        events: folded,
        events_outside_complete_cycles: outside,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> MarkerValidationConfig {
        MarkerValidationConfig {
            expected_frequency_hz: 1_000.0,
            frequency_tolerance_fraction: 0.01,
            max_period_jitter_fraction: 0.02,
            expected_cycles: Some(3),
        }
    }

    #[test]
    fn validates_and_folds_against_camera_clock_markers() {
        let markers = [10_000, 11_000, 12_000, 13_000];
        let events = [
            CameraEvent {
                timestamp_us: 10_250,
                x: 1,
                y: 2,
                polarity: Polarity::On,
            },
            CameraEvent {
                timestamp_us: 11_750,
                x: 3,
                y: 4,
                polarity: Polarity::Off,
            },
            CameraEvent {
                timestamp_us: 13_000,
                x: 0,
                y: 0,
                polarity: Polarity::On,
            },
        ];
        let fold = fold_events(&events, &markers, config()).expect("valid markers");
        assert_eq!(fold.validation.cycle_count, 3);
        assert_eq!(fold.events.len(), 2);
        assert_eq!(fold.events_outside_complete_cycles, 1);
        assert_eq!(fold.events[0].cycle_index, 0);
        assert!((fold.events[0].phase - 0.25).abs() < 1e-12);
        assert_eq!(fold.events[1].cycle_index, 1);
        assert!((fold.events[1].phase - 0.75).abs() < 1e-12);
    }

    #[test]
    fn rejects_marker_count_frequency_and_jitter_mismatches() {
        let mut wrong_count = config();
        wrong_count.expected_cycles = Some(4);
        assert!(matches!(
            validate_markers(&[0, 1_000, 2_000, 3_000], wrong_count),
            Err(MarkerError::CycleCount { .. })
        ));

        assert!(matches!(
            validate_markers(&[0, 2_000, 4_000, 6_000], config()),
            Err(MarkerError::FrequencyOutOfTolerance { .. })
        ));

        assert!(matches!(
            validate_markers(&[0, 1_000, 2_100, 3_000], config()),
            Err(MarkerError::JitterOutOfTolerance { .. })
        ));
    }

    #[test]
    fn free_running_fold_bins_relative_to_first_event() {
        // Period 1000 us; three whole cycles from the first event at 500 us.
        let events = [
            CameraEvent {
                timestamp_us: 500,
                x: 0,
                y: 0,
                polarity: Polarity::On,
            },
            CameraEvent {
                timestamp_us: 750,
                x: 0,
                y: 0,
                polarity: Polarity::Off,
            },
            CameraEvent {
                timestamp_us: 1_750,
                x: 0,
                y: 0,
                polarity: Polarity::On,
            },
            // Beyond the last whole cycle -> excluded.
            CameraEvent {
                timestamp_us: 4_000,
                x: 0,
                y: 0,
                polarity: Polarity::On,
            },
        ];
        let fold = fold_events_free_running(&events, 1_000.0).expect("one whole cycle");
        assert_eq!(fold.validation.cycle_count, 3);
        assert_eq!(fold.events.len(), 3);
        assert_eq!(fold.events_outside_complete_cycles, 1);
        assert!((fold.events[0].phase - 0.0).abs() < 1e-12);
        assert!((fold.events[1].phase - 0.25).abs() < 1e-12);
        assert_eq!(fold.events[1].cycle_index, 0);
        assert_eq!(fold.events[2].cycle_index, 1);
        assert!((fold.events[2].phase - 0.25).abs() < 1e-12);
    }

    #[test]
    fn free_running_fold_needs_one_whole_cycle() {
        let events = [
            CameraEvent {
                timestamp_us: 0,
                x: 0,
                y: 0,
                polarity: Polarity::On,
            },
            CameraEvent {
                timestamp_us: 400,
                x: 0,
                y: 0,
                polarity: Polarity::On,
            },
        ];
        assert!(fold_events_free_running(&events, 1_000.0).is_none());
    }

    #[test]
    fn rejects_non_monotonic_markers() {
        assert_eq!(
            validate_markers(&[0, 1_000, 999, 2_000], config()),
            Err(MarkerError::NonIncreasing { index: 2 })
        );
    }
}
