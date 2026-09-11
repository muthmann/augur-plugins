//! Quality control for one threshold point: what the sensor did while it was
//! recorded, and whether the bench held still enough to believe it.
//!
//! Everything here is pure. The runner feeds it counts and readings; it decides
//! nothing about the recording itself.
//!
//! The limits are **flags, not gates**. A point that drifts is recorded, kept,
//! and marked — because whether a 2 °C drift invalidated a threshold point is a
//! judgement to make later, with the file in hand, and a runner that discarded
//! the point would have thrown away the evidence for making it.

use crate::protocol::QcLimits;

/// Event counts and rates over one recording.
///
/// Rates are over the **recorded wall-clock duration**, not over the analysis
/// window, so they are comparable between points of different lengths.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct RateSummary {
    pub on_events: u64,
    pub off_events: u64,
    /// Seconds the counts were accumulated over.
    pub seconds: f64,
}

impl RateSummary {
    pub fn total_events(&self) -> u64 {
        self.on_events.saturating_add(self.off_events)
    }

    /// Events per second, or `None` when nothing was counted over a real
    /// interval. `None` is not zero: a point whose events were never seen must
    /// not report a rate of 0 Hz, which is a measurement.
    pub fn on_rate_hz(&self) -> Option<f64> {
        self.rate(self.on_events)
    }

    pub fn off_rate_hz(&self) -> Option<f64> {
        self.rate(self.off_events)
    }

    pub fn total_rate_hz(&self) -> Option<f64> {
        self.rate(self.total_events())
    }

    /// Share of events that were ON, in `0.0..=1.0`. The quantity a threshold
    /// survey is usually read through — an asymmetric `diff_on`/`diff_off` pair
    /// should move it.
    pub fn on_fraction(&self) -> Option<f64> {
        let total = self.total_events();
        (total > 0).then(|| self.on_events as f64 / total as f64)
    }

    fn rate(&self, count: u64) -> Option<f64> {
        (self.seconds > 0.0).then(|| count as f64 / self.seconds)
    }
}

/// How far a monitoring channel moved between the start and the end of a
/// recording. `None` for a channel the sensor could not report — absent, never
/// zero, because "no reading" and "no drift" are opposite facts.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Drift {
    /// |T_end − T_start| in °C.
    pub temperature_c: Option<f64>,
    /// |lux_end − lux_start| / lux_start × 100.
    pub illumination_percent: Option<f64>,
}

/// One channel's readings at the two ends of a recording.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Endpoints {
    pub start: Option<f32>,
    pub end: Option<f32>,
}

impl Endpoints {
    fn absolute_change(&self) -> Option<f64> {
        match (self.start, self.end) {
            (Some(start), Some(end)) => Some((end as f64 - start as f64).abs()),
            _ => None,
        }
    }

    fn relative_change_percent(&self) -> Option<f64> {
        match (self.start, self.end) {
            // A relative drift against a zero baseline is not a percentage of
            // anything. Report nothing rather than an infinity.
            (Some(start), Some(end)) if start.abs() > f32::EPSILON => {
                Some(((end as f64 - start as f64) / start as f64).abs() * 100.0)
            }
            _ => None,
        }
    }
}

pub fn drift(temperature: Endpoints, illumination: Endpoints) -> Drift {
    Drift {
        temperature_c: temperature.absolute_change(),
        illumination_percent: illumination.relative_change_percent(),
    }
}

/// The verdict on one recorded point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QcStatus {
    /// Every limit the row set was met.
    Pass,
    /// The row set no limits, so there was nothing to check. Distinct from
    /// `Pass`: an unchecked point must not read as a verified one.
    NotEvaluated,
    /// At least one limit was exceeded. The recording is kept; the reasons are
    /// carried into the sidecar and the run summary verbatim.
    Flagged(Vec<String>),
}

impl QcStatus {
    /// Short tag for the sidecar and the status table.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::NotEvaluated => "not_evaluated",
            Self::Flagged(_) => "flagged",
        }
    }

    pub fn flags(&self) -> &[String] {
        match self {
            Self::Flagged(flags) => flags,
            _ => &[],
        }
    }

    pub fn is_flagged(&self) -> bool {
        matches!(self, Self::Flagged(_))
    }
}

/// Judge a recorded point against the limits its protocol row set.
///
/// A limit whose quantity could not be measured is **not** a pass and **not** a
/// breach — it is recorded as a flag saying the check could not be made, so a
/// survey run on a camera with no temperature readback does not silently report
/// every point as within a drift limit nobody ever checked.
pub fn evaluate(limits: &QcLimits, rates: &RateSummary, drift: &Drift) -> QcStatus {
    if limits.is_empty() {
        return QcStatus::NotEvaluated;
    }
    let mut flags = Vec::new();

    if let Some(limit) = limits.max_temperature_drift_c {
        match drift.temperature_c {
            Some(measured) if measured > limit => flags.push(format!(
                "temperature drifted {measured:.2} °C, over the {limit:.2} °C limit"
            )),
            Some(_) => {}
            None => flags.push(
                "temperature drift could not be checked — the sensor reported no die temperature"
                    .into(),
            ),
        }
    }
    if let Some(limit) = limits.max_illumination_drift_percent {
        match drift.illumination_percent {
            Some(measured) if measured > limit => flags.push(format!(
                "illumination drifted {measured:.1} %, over the {limit:.1} % limit"
            )),
            Some(_) => {}
            None => flags.push(
                "illumination drift could not be checked — the sensor reported no usable lux"
                    .into(),
            ),
        }
    }
    if let Some(limit) = limits.max_event_rate {
        match rates.total_rate_hz() {
            Some(measured) if measured > limit => flags.push(format!(
                "event rate {measured:.0} ev/s, over the {limit:.0} ev/s limit"
            )),
            Some(_) => {}
            None => flags.push(
                "event rate could not be checked — no events were counted for this point".into(),
            ),
        }
    }

    if flags.is_empty() {
        QcStatus::Pass
    } else {
        QcStatus::Flagged(flags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rates(on: u64, off: u64, seconds: f64) -> RateSummary {
        RateSummary {
            on_events: on,
            off_events: off,
            seconds,
        }
    }

    #[test]
    fn rates_are_per_recorded_second_so_points_of_different_length_compare() {
        let summary = rates(6_000, 4_000, 60.0);
        assert_eq!(summary.total_events(), 10_000);
        assert_eq!(summary.on_rate_hz(), Some(100.0));
        assert_eq!(summary.off_rate_hz(), Some(200.0 / 3.0));
        assert!((summary.total_rate_hz().expect("counted") - 166.666_666).abs() < 1e-4);
        assert!((summary.on_fraction().expect("counted") - 0.6).abs() < 1e-12);
    }

    #[test]
    fn a_point_with_no_counted_interval_reports_no_rate_rather_than_zero() {
        // Zero events per second is a measurement. "We never counted" is not,
        // and the two must not be written into a sidecar as the same number.
        let summary = rates(0, 0, 0.0);
        assert_eq!(summary.total_rate_hz(), None);
        assert_eq!(summary.on_fraction(), None);
        // A real interval with genuinely no events *is* a rate of zero.
        assert_eq!(rates(0, 0, 60.0).total_rate_hz(), Some(0.0));
    }

    #[test]
    fn temperature_drift_is_absolute_and_illumination_drift_is_relative() {
        let measured = drift(
            Endpoints {
                start: Some(41.0),
                end: Some(43.5),
            },
            Endpoints {
                start: Some(200.0),
                end: Some(190.0),
            },
        );
        assert!((measured.temperature_c.expect("both ends") - 2.5).abs() < 1e-9);
        assert!((measured.illumination_percent.expect("both ends") - 5.0).abs() < 1e-9);
    }

    #[test]
    fn a_channel_missing_either_end_reports_no_drift_rather_than_zero() {
        let measured = drift(
            Endpoints {
                start: Some(41.0),
                end: None,
            },
            Endpoints::default(),
        );
        assert_eq!(measured.temperature_c, None);
        assert_eq!(measured.illumination_percent, None);
    }

    #[test]
    fn illumination_drift_against_a_dark_baseline_is_not_a_percentage() {
        let measured = drift(
            Endpoints::default(),
            Endpoints {
                start: Some(0.0),
                end: Some(5.0),
            },
        );
        assert_eq!(measured.illumination_percent, None);
    }

    #[test]
    fn a_row_with_no_limits_is_not_evaluated_rather_than_passing() {
        let status = evaluate(
            &QcLimits::default(),
            &rates(100, 100, 60.0),
            &Drift::default(),
        );
        assert_eq!(status, QcStatus::NotEvaluated);
        assert_eq!(status.as_str(), "not_evaluated");
        assert!(!status.is_flagged());
    }

    #[test]
    fn a_point_inside_every_limit_passes() {
        let limits = QcLimits {
            max_temperature_drift_c: Some(3.0),
            max_illumination_drift_percent: Some(10.0),
            max_event_rate: Some(1_000.0),
        };
        let status = evaluate(
            &limits,
            &rates(300, 300, 60.0),
            &Drift {
                temperature_c: Some(1.0),
                illumination_percent: Some(2.0),
            },
        );
        assert_eq!(status, QcStatus::Pass);
    }

    #[test]
    fn a_breach_names_the_measured_value_and_the_limit_it_passed() {
        let limits = QcLimits {
            max_temperature_drift_c: Some(1.0),
            max_illumination_drift_percent: None,
            max_event_rate: Some(100.0),
        };
        let status = evaluate(
            &limits,
            &rates(6_000, 6_000, 60.0),
            &Drift {
                temperature_c: Some(2.5),
                illumination_percent: None,
            },
        );
        let flags = status.flags();
        assert_eq!(flags.len(), 2, "{flags:?}");
        assert!(flags[0].contains("2.50 °C"), "{flags:?}");
        assert!(flags[0].contains("1.00 °C"), "{flags:?}");
        assert!(flags[1].contains("200 ev/s"), "{flags:?}");
        assert!(status.is_flagged());
    }

    #[test]
    fn a_limit_whose_quantity_was_never_measured_is_flagged_not_passed() {
        // The failure this prevents: a camera with no temperature readback
        // silently reporting every point as within a drift limit that was
        // never actually checked.
        let limits = QcLimits {
            max_temperature_drift_c: Some(1.0),
            ..QcLimits::default()
        };
        let status = evaluate(&limits, &rates(10, 10, 60.0), &Drift::default());
        assert!(status.is_flagged());
        assert!(
            status.flags()[0].contains("could not be checked"),
            "{:?}",
            status.flags()
        );
    }

    #[test]
    fn a_limit_exactly_met_is_not_a_breach() {
        let limits = QcLimits {
            max_temperature_drift_c: Some(2.0),
            ..QcLimits::default()
        };
        let status = evaluate(
            &limits,
            &rates(10, 10, 60.0),
            &Drift {
                temperature_c: Some(2.0),
                illumination_percent: None,
            },
        );
        assert_eq!(status, QcStatus::Pass);
    }
}
