//! Phase-bin event rates and rolling half-period operator quicklooks.

use crate::phase::PhaseFold;
use crate::types::Polarity;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLayer {
    pub count: u64,
    /// Events per valid pixel per second.
    pub rate_per_pixel_s: f64,
    /// Poisson standard error in the same units as `rate_per_pixel_s`.
    pub standard_error: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhaseRateBin {
    pub phase_start: f64,
    pub phase_end: f64,
    pub run: RateLayer,
    pub background: Option<RateLayer>,
    /// Run minus background. Negative values are intentionally preserved.
    pub net_rate_per_pixel_s: Option<f64>,
    /// Independent Poisson uncertainty propagated in quadrature.
    pub net_standard_error: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PolarityPhaseRates {
    pub polarity: Polarity,
    pub valid_pixels: usize,
    pub run_cycles: usize,
    pub background_cycles: Option<usize>,
    pub bins: Vec<PhaseRateBin>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhaseRateSet {
    pub on: PolarityPhaseRates,
    pub off: PolarityPhaseRates,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RateError {
    ZeroValidPixels,
    InvalidBinCount,
    EmptyCycles,
}

pub fn phase_bin_rates(
    run: &PhaseFold,
    background: Option<&PhaseFold>,
    valid_pixels: usize,
    bin_count: usize,
) -> Result<PhaseRateSet, RateError> {
    if valid_pixels == 0 {
        return Err(RateError::ZeroValidPixels);
    }
    if bin_count == 0 {
        return Err(RateError::InvalidBinCount);
    }
    if run.validation.cycle_count == 0
        || background.is_some_and(|fold| fold.validation.cycle_count == 0)
    {
        return Err(RateError::EmptyCycles);
    }

    Ok(PhaseRateSet {
        on: rates_for_polarity(run, background, valid_pixels, bin_count, Polarity::On),
        off: rates_for_polarity(run, background, valid_pixels, bin_count, Polarity::Off),
    })
}

fn rates_for_polarity(
    run: &PhaseFold,
    background: Option<&PhaseFold>,
    valid_pixels: usize,
    bin_count: usize,
    polarity: Polarity,
) -> PolarityPhaseRates {
    let mut run_counts = vec![0_u64; bin_count];
    let mut background_counts = vec![0_u64; bin_count];
    for event in run.events.iter().filter(|event| event.polarity == polarity) {
        run_counts[phase_bin(event.phase, bin_count)] += 1;
    }
    if let Some(background) = background {
        for event in background
            .events
            .iter()
            .filter(|event| event.polarity == polarity)
        {
            background_counts[phase_bin(event.phase, bin_count)] += 1;
        }
    }

    let run_bin_s = run.validation.mean_period_us / 1_000_000.0 / bin_count as f64;
    let run_exposure = valid_pixels as f64 * run.validation.cycle_count as f64 * run_bin_s;
    let background_exposure = background.map(|fold| {
        valid_pixels as f64
            * fold.validation.cycle_count as f64
            * (fold.validation.mean_period_us / 1_000_000.0 / bin_count as f64)
    });

    let bins = (0..bin_count)
        .map(|index| {
            let run_layer = poisson_layer(run_counts[index], run_exposure);
            let background_layer = background_exposure
                .map(|exposure| poisson_layer(background_counts[index], exposure));
            let (net, net_error) = background_layer.map_or((None, None), |background| {
                (
                    Some(run_layer.rate_per_pixel_s - background.rate_per_pixel_s),
                    Some(
                        (run_layer.standard_error.powi(2) + background.standard_error.powi(2))
                            .sqrt(),
                    ),
                )
            });
            PhaseRateBin {
                phase_start: index as f64 / bin_count as f64,
                phase_end: (index + 1) as f64 / bin_count as f64,
                run: run_layer,
                background: background_layer,
                net_rate_per_pixel_s: net,
                net_standard_error: net_error,
            }
        })
        .collect();

    PolarityPhaseRates {
        polarity,
        valid_pixels,
        run_cycles: run.validation.cycle_count,
        background_cycles: background.map(|fold| fold.validation.cycle_count),
        bins,
    }
}

fn phase_bin(phase: f64, bin_count: usize) -> usize {
    ((phase.rem_euclid(1.0) * bin_count as f64).floor() as usize).min(bin_count - 1)
}

fn poisson_layer(count: u64, exposure_pixel_s: f64) -> RateLayer {
    RateLayer {
        count,
        rate_per_pixel_s: count as f64 / exposure_pixel_s,
        standard_error: (count as f64).sqrt() / exposure_pixel_s,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RollingResponsePoint {
    pub timestamp_us: u64,
    /// Events in `(t - T/2, t]` per valid pixel.
    pub run_per_pixel: f64,
    /// Integral of the periodic phase-resolved background rate, when enabled.
    pub background_per_pixel: Option<f64>,
    pub net_per_pixel: Option<f64>,
}

pub fn rolling_half_period_response(
    run: &PhaseFold,
    polarity: Polarity,
    valid_pixels: usize,
    sample_times_us: &[u64],
    background_model: Option<&PolarityPhaseRates>,
) -> Result<Vec<RollingResponsePoint>, RateError> {
    if valid_pixels == 0 {
        return Err(RateError::ZeroValidPixels);
    }
    let half_period_us = run.validation.mean_period_us / 2.0;
    let period_s = run.validation.mean_period_us / 1_000_000.0;
    Ok(sample_times_us
        .iter()
        .map(|&timestamp_us| {
            let window_start = timestamp_us as f64 - half_period_us;
            let count = run
                .events
                .iter()
                .filter(|event| {
                    event.polarity == polarity
                        && event.timestamp_us as f64 > window_start
                        && event.timestamp_us <= timestamp_us
                })
                .count();
            let run_per_pixel = count as f64 / valid_pixels as f64;
            let background_per_pixel = background_model.map(|model| {
                let start_phase = run.phase_at(timestamp_us.saturating_sub(half_period_us as u64));
                integrate_periodic_rates(model, start_phase, 0.5) * period_s
            });
            RollingResponsePoint {
                timestamp_us,
                run_per_pixel,
                background_per_pixel,
                net_per_pixel: background_per_pixel.map(|bg| run_per_pixel - bg),
            }
        })
        .collect())
}

/// Integrates rates over a circular phase span and returns rate × phase.
fn integrate_periodic_rates(model: &PolarityPhaseRates, start_phase: f64, phase_span: f64) -> f64 {
    let mut total = 0.0;
    let start = start_phase.rem_euclid(1.0);
    let end = start + phase_span;
    for bin in &model.bins {
        for offset in [0.0, 1.0] {
            let bin_start = bin.phase_start + offset;
            let bin_end = bin.phase_end + offset;
            let overlap = (end.min(bin_end) - start.max(bin_start)).max(0.0);
            let layer = bin.background.unwrap_or(bin.run);
            total += overlap * layer.rate_per_pixel_s;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase::{fold_events, MarkerValidationConfig};
    use crate::types::CameraEvent;

    fn fold(events: &[CameraEvent]) -> PhaseFold {
        fold_events(
            events,
            &[0, 1_000, 2_000],
            MarkerValidationConfig {
                expected_frequency_hz: 1_000.0,
                frequency_tolerance_fraction: 0.0,
                max_period_jitter_fraction: 0.0,
                expected_cycles: Some(2),
            },
        )
        .unwrap()
    }

    fn event(timestamp_us: u64, polarity: Polarity) -> CameraEvent {
        CameraEvent {
            timestamp_us,
            x: 0,
            y: 0,
            polarity,
        }
    }

    #[test]
    fn computes_raw_background_and_negative_net_rates_per_polarity() {
        let run = fold(&[
            event(100, Polarity::On),
            event(1_100, Polarity::On),
            event(600, Polarity::Off),
        ]);
        let background = fold(&[
            event(100, Polarity::On),
            event(200, Polarity::On),
            event(1_100, Polarity::On),
            event(1_200, Polarity::On),
        ]);
        let rates = phase_bin_rates(&run, Some(&background), 10, 2).unwrap();
        let on_first = &rates.on.bins[0];
        assert_eq!(on_first.run.count, 2);
        assert_eq!(on_first.background.unwrap().count, 4);
        assert!(on_first.net_rate_per_pixel_s.unwrap() < 0.0);
        assert!(on_first.net_standard_error.unwrap() > 0.0);
        assert_eq!(rates.off.bins[1].run.count, 1);
    }

    #[test]
    fn rolling_quicklook_uses_open_left_closed_right_window_and_background_integral() {
        let run = fold(&[
            event(500, Polarity::On),
            event(750, Polarity::On),
            event(1_000, Polarity::On),
        ]);
        let background = fold(&[
            event(100, Polarity::On),
            event(600, Polarity::On),
            event(1_100, Polarity::On),
            event(1_600, Polarity::On),
        ]);
        let background_rates = phase_bin_rates(&run, Some(&background), 1, 2).unwrap();
        let points = rolling_half_period_response(
            &run,
            Polarity::On,
            1,
            &[1_000],
            Some(&background_rates.on),
        )
        .unwrap();
        // Event at exactly t-T/2 is excluded; 750 and 1000 are included.
        assert_eq!(points[0].run_per_pixel, 2.0);
        assert!((points[0].background_per_pixel.unwrap() - 1.0).abs() < 1e-12);
        assert!((points[0].net_per_pixel.unwrap() - 1.0).abs() < 1e-12);
    }
}
