//! How much longer an unattended run still has to go.
//!
//! Every long run in this plugin — a depth sweep, a frequency ladder, a
//! protocol — knows what it *asked* for: so many points, so many seconds of
//! recording, so much settle dwell. None of them know what the bench actually
//! charges for a point. The start/finalize handshake, the drive settling at a
//! new depth, the trigger confirming a new frequency, the photodiode's own
//! estimator window: all of that is real time that appears in no setting, and
//! on this bench it is the larger half of a short point.
//!
//! So the estimate is the plan's own numbers scaled by the pace the run is
//! actually keeping. Until the first point finishes there is nothing to
//! measure and the plan stands alone, with a fixed allowance for the handshake;
//! from then on every finished point re-scales what is left. That makes the
//! first number a lower bound that improves rather than a promise — the honest
//! shape for a survey whose settle times are a property of the bench and not
//! of the file it was started from.
//!
//! Nothing here steers a run. The estimate exists so the operator can decide
//! whether to wait, and so a run started at 23:00 says whether it is a
//! coffee-length or a night-length one *before* it is left alone.

/// Wall-clock cost of the start/finalize handshake around one recording, on
/// top of the time the point itself asks for. Only carries the estimate until
/// the run has measured its own pace.
pub const POINT_OVERHEAD_S: f64 = 5.0;

/// Bounds on the measured pace correction. A run that lost a lease and spent a
/// point timing out must not extrapolate that point over the whole rest of the
/// survey, and one whose first point was refused before it began must not
/// predict the rest away either.
const PACE_RANGE: (f64, f64) = (0.5, 6.0);

/// One run's answer to "how much longer?": the plan's own seconds, corrected by
/// the pace the run is keeping.
#[derive(Debug, Clone, Copy)]
pub struct Eta {
    /// When the run started, for the elapsed leg of the total.
    started_ms: u64,
    /// When the point in flight started — the run start, then each boundary.
    point_started_ms: u64,
    /// Planned seconds of the points that have already finished.
    planned_done_s: f64,
    /// Wall-clock seconds those points actually took.
    actual_done_s: f64,
}

impl Eta {
    pub fn new(now_ms: u64) -> Self {
        Self {
            started_ms: now_ms,
            point_started_ms: now_ms,
            planned_done_s: 0.0,
            actual_done_s: 0.0,
        }
    }

    /// One point finished — recorded, skipped or given up — having been planned
    /// to cost `planned_s`.
    ///
    /// Skipped points are counted deliberately: what is being measured is how
    /// long this run takes to get through its list, and a point that fails
    /// still spends its timeouts.
    pub fn point_done(&mut self, now_ms: u64, planned_s: f64) {
        let spent_s = now_ms.saturating_sub(self.point_started_ms) as f64 / 1_000.0;
        self.point_started_ms = now_ms;
        if planned_s > 0.0 {
            self.planned_done_s += planned_s;
            self.actual_done_s += spent_s;
        }
    }

    /// Seconds of wall clock the bench spends per planned second, from the
    /// points that have finished. `None` until there is one to measure.
    pub fn pace(&self) -> Option<f64> {
        (self.planned_done_s > 0.0 && self.actual_done_s > 0.0)
            .then(|| (self.actual_done_s / self.planned_done_s).clamp(PACE_RANGE.0, PACE_RANGE.1))
    }

    /// Wall-clock seconds still to go, for a plan that has `planned_remaining_s`
    /// left *including* the point currently in flight.
    pub fn remaining_s(&self, now_ms: u64, planned_remaining_s: f64) -> f64 {
        let scaled = planned_remaining_s.max(0.0) * self.pace().unwrap_or(1.0);
        let in_flight_s = now_ms.saturating_sub(self.point_started_ms) as f64 / 1_000.0;
        (scaled - in_flight_s).max(0.0)
    }

    pub fn elapsed_s(&self, now_ms: u64) -> f64 {
        now_ms.saturating_sub(self.started_ms) as f64 / 1_000.0
    }
}

/// A duration as the operator would say it: seconds while that is meaningful,
/// then minutes, then hours. Never more than two units — the point is the size
/// of the wait, not its last second.
pub fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    if total < 90 {
        return format!("{total} s");
    }
    let minutes = total / 60;
    if minutes < 60 {
        let rest = total % 60;
        return if minutes < 10 && rest > 0 {
            format!("{minutes} min {rest} s")
        } else {
            format!("{minutes} min")
        };
    }
    let hours = minutes / 60;
    let rest = minutes % 60;
    if rest > 0 {
        format!("{hours} h {rest} min")
    } else {
        format!("{hours} h")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(format_duration(0.0), "0 s");
        assert_eq!(format_duration(45.4), "45 s");
        assert_eq!(format_duration(89.0), "89 s");
        assert_eq!(format_duration(200.0), "3 min 20 s");
        assert_eq!(format_duration(1_800.0), "30 min");
        assert_eq!(format_duration(3_600.0), "1 h");
        assert_eq!(format_duration(4_500.0), "1 h 15 min");
    }

    /// Before anything has finished the plan is all there is, and it counts
    /// down inside the point in flight rather than standing still.
    #[test]
    fn an_unmeasured_run_reports_the_plan_and_still_counts_down() {
        let eta = Eta::new(0);
        assert!(eta.pace().is_none());
        assert!((eta.remaining_s(0, 100.0) - 100.0).abs() < 1e-9);
        assert!((eta.remaining_s(30_000, 100.0) - 70.0).abs() < 1e-9);
    }

    /// A point that took twice its planned time says the rest will too.
    #[test]
    fn a_finished_point_rescales_what_is_left() {
        let mut eta = Eta::new(0);
        eta.point_done(20_000, 10.0);
        assert_eq!(eta.pace(), Some(2.0));
        // Four points of 10 planned seconds left, at 2 s of bench per planned
        // second, with nothing spent in the current point yet.
        assert!((eta.remaining_s(20_000, 40.0) - 80.0).abs() < 1e-9);
    }

    /// One pathological point must not extrapolate over the whole survey.
    #[test]
    fn the_pace_correction_is_bounded() {
        let mut eta = Eta::new(0);
        eta.point_done(600_000, 1.0);
        assert_eq!(eta.pace(), Some(PACE_RANGE.1));
    }

    /// The run's own clock keeps running across points, so the total the panel
    /// shows (elapsed + remaining) is wall clock and not a sum of estimates.
    #[test]
    fn elapsed_is_measured_from_the_run_start() {
        let mut eta = Eta::new(1_000);
        eta.point_done(11_000, 10.0);
        assert!((eta.elapsed_s(31_000) - 30.0).abs() < 1e-9);
    }
}
