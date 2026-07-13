//! Minimum-depth sweep state machine.
//!
//! For each frequency point: bisect on the integer DAC drive code until the
//! detection boundary is bracketed, then measure a small log-spaced grid
//! across the transition, then fit `a_min` (see `analysis::fit_min_depth`).
//! The engine is pure — device I/O and event analysis happen outside; it
//! only ingests finished measurements and emits the next drive request.
//! Note the asymmetry the whole design hinges on: the *search* variable is
//! the drive code, but every recorded point carries the **measured**
//! optical contrast `a` from the photodiode.

use crate::analysis::{fit_min_depth, DepthPoint, MinDepthFit};

#[derive(Debug, Clone, PartialEq)]
pub struct SweepPlan {
    pub frequencies_hz: Vec<f64>,
    pub initial_amplitude_dac: u32,
    pub max_amplitude_dac: u32,
    /// Grid points measured across the bracket after bisection.
    pub grid_points: usize,
    /// Hard cap on measurements per frequency (bisection + grid).
    pub max_measurements_per_frequency: usize,
}

impl Default for SweepPlan {
    fn default() -> Self {
        Self {
            frequencies_hz: Vec::new(),
            initial_amplitude_dac: 512,
            max_amplitude_dac: 2_047,
            grid_points: 6,
            max_measurements_per_frequency: 24,
        }
    }
}

/// One finished measurement at the currently requested drive.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    pub amplitude_dac: u32,
    /// Photodiode-measured optical log-contrast. `None` = invalid window
    /// (clipped / integrity fault) — the point is discarded and re-measured.
    pub measured_a: Option<f64>,
    pub events_per_half_cycle: f64,
    pub detected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SweepCommand {
    /// Configure the drive and measure at these settings.
    Measure { frequency_hz: f64, amplitude_dac: u32 },
    /// All frequencies finished.
    Finished,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrequencyResult {
    pub frequency_hz: f64,
    pub fit: Option<MinDepthFit>,
    pub points: Vec<DepthPoint>,
    pub measurements: usize,
    /// True when the point budget ran out before the transition was
    /// bracketed — a_min is not identifiable from this data.
    pub exhausted: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Bisecting,
    Grid { queue: Vec<u32> },
}

pub struct SweepEngine {
    plan: SweepPlan,
    frequency_index: usize,
    phase: Phase,
    current_dac: u32,
    measurements_at_frequency: usize,
    /// Highest drive code that did NOT detect / lowest that did.
    highest_undetected: Option<u32>,
    lowest_detected: Option<u32>,
    points: Vec<DepthPoint>,
    invalid_retries: usize,
    pub results: Vec<FrequencyResult>,
}

impl SweepEngine {
    pub fn new(plan: SweepPlan) -> Self {
        let current_dac = plan.initial_amplitude_dac;
        Self {
            plan,
            frequency_index: 0,
            phase: Phase::Bisecting,
            current_dac,
            measurements_at_frequency: 0,
            highest_undetected: None,
            lowest_detected: None,
            points: Vec::new(),
            invalid_retries: 0,
            results: Vec::new(),
        }
    }

    pub fn current_command(&self) -> SweepCommand {
        match self.plan.frequencies_hz.get(self.frequency_index) {
            Some(&frequency_hz) => SweepCommand::Measure {
                frequency_hz,
                amplitude_dac: self.current_dac,
            },
            None => SweepCommand::Finished,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.frequency_index >= self.plan.frequencies_hz.len()
    }

    /// Ingests the finished measurement for the last `Measure` command and
    /// advances the state machine.
    pub fn ingest(&mut self, measurement: Measurement) -> SweepCommand {
        if self.is_finished() {
            return SweepCommand::Finished;
        }

        let Some(a) = measurement.measured_a else {
            // Invalid window: re-measure the same point (bounded retries),
            // never silently keep the previous contrast.
            self.invalid_retries += 1;
            if self.invalid_retries > 3 {
                self.finish_frequency(true);
            }
            return self.current_command();
        };
        self.invalid_retries = 0;
        self.measurements_at_frequency += 1;
        self.points.push(DepthPoint {
            a,
            events_per_half_cycle: measurement.events_per_half_cycle,
        });

        if measurement.detected {
            self.lowest_detected = Some(
                self.lowest_detected
                    .map_or(measurement.amplitude_dac, |d| d.min(measurement.amplitude_dac)),
            );
        } else {
            self.highest_undetected = Some(
                self.highest_undetected
                    .map_or(measurement.amplitude_dac, |d| d.max(measurement.amplitude_dac)),
            );
        }

        if self.measurements_at_frequency >= self.plan.max_measurements_per_frequency {
            self.finish_frequency(!self.bracketed());
            return self.current_command();
        }

        match &mut self.phase {
            Phase::Bisecting => {
                if self.bracketed() {
                    let queue = self.grid_queue();
                    self.phase = Phase::Grid { queue };
                    self.advance_grid();
                } else if measurement.detected {
                    // Drive down toward the boundary.
                    let next = ((measurement.amplitude_dac as f64) * 0.65).round() as u32;
                    if next < 1 {
                        self.finish_frequency(false);
                    } else {
                        self.current_dac = next.max(1);
                    }
                } else {
                    // Drive up toward the boundary.
                    let next = ((measurement.amplitude_dac as f64) * 1.5).ceil() as u32;
                    if next > self.plan.max_amplitude_dac {
                        // Even full drive shows nothing: unmeasurable point.
                        self.finish_frequency(true);
                    } else {
                        self.current_dac = next;
                    }
                }
            }
            Phase::Grid { .. } => {
                self.advance_grid();
            }
        }
        self.current_command()
    }

    fn bracketed(&self) -> bool {
        matches!(
            (self.highest_undetected, self.lowest_detected),
            (Some(_), Some(_))
        )
    }

    fn grid_queue(&self) -> Vec<u32> {
        let (Some(low), Some(high)) = (self.highest_undetected, self.lowest_detected) else {
            return Vec::new();
        };
        let lo = (low.min(high) as f64 * 0.8).max(1.0);
        let hi = (low.max(high) as f64 * 1.25).min(self.plan.max_amplitude_dac as f64);
        let n = self.plan.grid_points.max(2);
        (0..n)
            .map(|i| {
                let t = i as f64 / (n - 1) as f64;
                (lo * (hi / lo).powf(t)).round() as u32
            })
            .collect()
    }

    fn advance_grid(&mut self) {
        let next = match &mut self.phase {
            Phase::Grid { queue } if !queue.is_empty() => Some(queue.remove(0)),
            _ => None,
        };
        match next {
            Some(dac) => self.current_dac = dac,
            None => self.finish_frequency(false),
        }
    }

    fn finish_frequency(&mut self, exhausted: bool) {
        let frequency_hz = self.plan.frequencies_hz[self.frequency_index];
        let fit = if exhausted {
            None
        } else {
            fit_min_depth(&self.points)
        };
        self.results.push(FrequencyResult {
            frequency_hz,
            fit,
            points: std::mem::take(&mut self.points),
            measurements: self.measurements_at_frequency,
            exhausted,
        });
        self.frequency_index += 1;
        self.phase = Phase::Bisecting;
        self.current_dac = self.plan.initial_amplitude_dac;
        self.measurements_at_frequency = 0;
        self.highest_undetected = None;
        self.lowest_detected = None;
        self.invalid_retries = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulated bench: optical contrast is proportional to the drive code
    /// (a = dac / 2000) and the pixel responds with the smeared first step
    /// around a_min = 0.2.
    fn respond(dac: u32) -> Measurement {
        let a = dac as f64 / 2_000.0;
        let z = (a.ln() - 0.2_f64.ln()) / 0.12;
        let n = 0.5 * (1.0 + erf_approx(z / std::f64::consts::SQRT_2));
        Measurement {
            amplitude_dac: dac,
            measured_a: Some(a),
            events_per_half_cycle: n,
            detected: n > 0.15,
        }
    }

    fn erf_approx(x: f64) -> f64 {
        let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
        let poly = t
            * (0.254_829_592
                + t * (-0.284_496_736
                    + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
        let value = 1.0 - poly * (-x * x).exp();
        if x >= 0.0 {
            value
        } else {
            -value
        }
    }

    #[test]
    fn converges_to_the_synthetic_a_min() {
        let mut engine = SweepEngine::new(SweepPlan {
            frequencies_hz: vec![1_000.0, 10_000.0],
            ..SweepPlan::default()
        });

        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 200, "sweep must terminate");
            match engine.current_command() {
                SweepCommand::Finished => break,
                SweepCommand::Measure { amplitude_dac, .. } => {
                    engine.ingest(respond(amplitude_dac));
                }
            }
        }

        assert_eq!(engine.results.len(), 2);
        for result in &engine.results {
            let fit = result.fit.as_ref().expect("fit must exist");
            assert!(
                (fit.a_min - 0.2).abs() < 0.04,
                "f={} a_min={}",
                result.frequency_hz,
                fit.a_min
            );
            assert!(!result.exhausted);
        }
    }

    #[test]
    fn undetectable_frequency_is_reported_exhausted_not_fitted() {
        let mut engine = SweepEngine::new(SweepPlan {
            frequencies_hz: vec![100_000.0],
            ..SweepPlan::default()
        });
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 100);
            match engine.current_command() {
                SweepCommand::Finished => break,
                SweepCommand::Measure { amplitude_dac, .. } => {
                    engine.ingest(Measurement {
                        amplitude_dac,
                        measured_a: Some(amplitude_dac as f64 / 2_000.0),
                        events_per_half_cycle: 0.0,
                        detected: false,
                    });
                }
            }
        }
        assert_eq!(engine.results.len(), 1);
        assert!(engine.results[0].exhausted);
        assert!(engine.results[0].fit.is_none());
    }

    #[test]
    fn invalid_windows_are_retried_then_abandoned() {
        let mut engine = SweepEngine::new(SweepPlan {
            frequencies_hz: vec![1_000.0],
            ..SweepPlan::default()
        });
        let mut measures = 0;
        loop {
            match engine.current_command() {
                SweepCommand::Finished => break,
                SweepCommand::Measure { amplitude_dac, .. } => {
                    measures += 1;
                    assert!(measures < 20);
                    engine.ingest(Measurement {
                        amplitude_dac,
                        measured_a: None,
                        events_per_half_cycle: 0.0,
                        detected: false,
                    });
                }
            }
        }
        assert!(engine.results[0].exhausted);
    }
}
