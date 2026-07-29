//! Calibrated optical log-contrast estimator.
//!
//! `a = ln(I_exc,max / I_exc,min)` is defined by the *excitation light*, never
//! by the commanded DAC excursion: the Pockels-cell V→T response is non-linear,
//! so the photodiode ADC trace is the only valid source of `a`
//! (knowledge base: `methodology/camera-calibration.md`, "define `a` from
//! the light, not the drive").
//!
//! The detector geometry matters. When the photodiode sits behind the PBS
//! reject port it measures the *rejected complement* `I_pd = I_tot - I_exc`,
//! so the peak detector ratio is **not** the excitation contrast. The caller
//! selects the geometry via [`ContrastGeometry`]:
//! - [`ContrastGeometry::Direct`] — the detector already sees the excitation
//!   intensity (e.g. the plugin's EXCITATION display, `I_tot - I_pd`), so
//!   `a = ln(v_max / v_min)`.
//! - [`ContrastGeometry::RejectedComplement`] — the detector sees the rejected
//!   light (the plugin's RAW display), so
//!   `a = ln((I_tot - v_min) / (I_tot - v_max))`.
//!
//! The estimator therefore:
//! - converts ADC codes to volts through a characterised affine calibration,
//! - subtracts the dark level (the detector is DC-coupled; `a` needs true
//!   levels including DC),
//! - takes robust percentile extrema rather than raw min/max so single-code
//!   noise spikes do not bias the contrast,
//! - refuses to produce a value at all when the window clips (top/bottom of
//!   the ADC range), has no headroom above dark, or the total-power anchor is
//!   below the measured signal — a wrong `a` is worse than no `a`.

use serde::{Deserialize, Serialize};

/// Affine ADC calibration plus dark level, all in physical units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdcCalibration {
    /// Volts per ADC code (gain of the whole front end into the ADC).
    pub volts_per_code: f64,
    /// Voltage at code 0.
    pub offset_volts: f64,
    /// Dark level (light blocked), in volts after the affine map.
    pub dark_volts: f64,
    /// Full-scale code (4095 for the Teensy 12-bit ADC).
    pub full_scale_code: u16,
}

impl Default for AdcCalibration {
    fn default() -> Self {
        Self {
            volts_per_code: 3.3 / 4_095.0,
            offset_volts: 0.0,
            dark_volts: 0.0,
            full_scale_code: 4_095,
        }
    }
}

impl AdcCalibration {
    pub fn code_to_volts(&self, code: u16) -> f64 {
        self.offset_volts + f64::from(code) * self.volts_per_code
    }
}

/// Optical geometry of the detector relative to the excitation beam.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ContrastGeometry {
    /// The detector already measures the excitation intensity, so
    /// `a = ln(v_max / v_min)`.
    Direct,
    /// The detector sits behind the PBS reject port and measures the rejected
    /// complement `I_pd = I_tot - I_exc`. `total_power_volts` is the
    /// dark-corrected total power `I_tot`; the excitation contrast is
    /// `a = ln((I_tot - v_min) / (I_tot - v_max))`.
    RejectedComplement { total_power_volts: f64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContrastEstimate {
    /// Peak-to-peak excitation log-contrast `a = ln(I_exc,max / I_exc,min)`
    /// (dark-corrected, geometry-resolved).
    pub a: f64,
    /// Excitation intensity extrema in volts after the geometry transform.
    pub v_min_volts: f64,
    pub v_max_volts: f64,
    /// Fraction of samples at or below code 0 + margin.
    pub low_clip_fraction: f64,
    /// Fraction of samples at or above full scale - margin.
    pub high_clip_fraction: f64,
    pub sample_count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EstimateError {
    /// The rejected-complement geometry has no explicitly confirmed,
    /// traceable total-power anchor.
    MissingTotalPowerAnchor,
    /// No marker-bounded window containing at least two complete modulation
    /// cycles fits inside the retained sample budget.
    IncompleteModulationCycles {
        marker_count: usize,
        max_samples: usize,
    },
    /// Fewer samples than the estimator can use robustly.
    TooFewSamples { count: usize, minimum: usize },
    /// The window touches the ADC rails — `a` would be silently wrong.
    Clipped {
        low_fraction_permille: u32,
        high_fraction_permille: u32,
    },
    /// The dark-corrected minimum is not positive: no optical headroom.
    NoHeadroomAboveDark,
    /// The rejected-complement total-power anchor `I_tot` is not above the
    /// measured detector maximum, so the excitation minimum would be
    /// non-positive: the anchor is wrong or the light is not the complement.
    TotalPowerBelowSignal {
        total_power_volts: f64,
        detector_max_volts: f64,
    },
}

impl std::fmt::Display for EstimateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTotalPowerAnchor => f.write_str(
                "no confirmed, named total-power anchor; excitation contrast a is withheld",
            ),
            Self::IncompleteModulationCycles {
                marker_count,
                max_samples,
            } => write!(
                f,
                "no marker-bounded window with at least two complete cycles fits in \
                 {max_samples} samples ({marker_count} usable markers)"
            ),
            Self::TooFewSamples { count, minimum } => {
                write!(f, "only {count} samples (minimum {minimum})")
            }
            Self::Clipped {
                low_fraction_permille,
                high_fraction_permille,
            } => write!(
                f,
                "ADC clipping: {low_fraction_permille}‰ low / {high_fraction_permille}‰ high"
            ),
            Self::NoHeadroomAboveDark => {
                f.write_str("dark-corrected minimum is not positive; a is undefined")
            }
            Self::TotalPowerBelowSignal {
                total_power_volts,
                detector_max_volts,
            } => write!(
                f,
                "total-power anchor {total_power_volts:.4} V is not above the detector \
                 maximum {detector_max_volts:.4} V; a is undefined"
            ),
        }
    }
}

impl std::error::Error for EstimateError {}

pub const MIN_SAMPLES: usize = 64;
/// Codes within this margin of the rails count as clipped.
pub const CLIP_MARGIN_CODES: u16 = 4;
/// Reject the window when more than 1‰ of samples clip.
pub const MAX_CLIP_FRACTION: f64 = 0.001;
/// Robust extrema: 1st / 99th percentile.
const LOW_PERCENTILE: f64 = 0.01;
const HIGH_PERCENTILE: f64 = 0.99;

/// Estimates the excitation log-contrast from one settled, phase-attributed
/// ADC window. The window must span at least a few full modulation cycles;
/// enforcing that is the caller's job (it knows the drive frequency). The
/// `geometry` selects whether the codes are the excitation intensity directly
/// or the rejected complement measured behind the PBS reject port.
pub fn estimate_contrast(
    codes: &[u16],
    calibration: &AdcCalibration,
    geometry: ContrastGeometry,
) -> Result<ContrastEstimate, EstimateError> {
    if codes.len() < MIN_SAMPLES {
        return Err(EstimateError::TooFewSamples {
            count: codes.len(),
            minimum: MIN_SAMPLES,
        });
    }

    let low_clip_threshold = CLIP_MARGIN_CODES;
    let high_clip_threshold = calibration
        .full_scale_code
        .saturating_sub(CLIP_MARGIN_CODES);
    let low_clipped = codes.iter().filter(|&&c| c <= low_clip_threshold).count();
    let high_clipped = codes.iter().filter(|&&c| c >= high_clip_threshold).count();
    let low_clip_fraction = low_clipped as f64 / codes.len() as f64;
    let high_clip_fraction = high_clipped as f64 / codes.len() as f64;
    if low_clip_fraction > MAX_CLIP_FRACTION || high_clip_fraction > MAX_CLIP_FRACTION {
        return Err(EstimateError::Clipped {
            low_fraction_permille: (low_clip_fraction * 1_000.0).round() as u32,
            high_fraction_permille: (high_clip_fraction * 1_000.0).round() as u32,
        });
    }

    let mut sorted = codes.to_vec();
    sorted.sort_unstable();
    let low_code = percentile(&sorted, LOW_PERCENTILE);
    let high_code = percentile(&sorted, HIGH_PERCENTILE);

    // Dark-corrected detector volts at the robust extrema.
    let detector_low = calibration.code_to_volts(low_code) - calibration.dark_volts;
    let detector_high = calibration.code_to_volts(high_code) - calibration.dark_volts;

    // Resolve the excitation extrema from the detector geometry.
    let (exc_min, exc_max) = match geometry {
        ContrastGeometry::Direct => {
            if detector_low <= 0.0 {
                return Err(EstimateError::NoHeadroomAboveDark);
            }
            (detector_low, detector_high)
        }
        ContrastGeometry::RejectedComplement { total_power_volts } => {
            // The most transmitted excitation coincides with the least rejected
            // light (detector_low), and vice versa.
            if total_power_volts <= detector_high {
                return Err(EstimateError::TotalPowerBelowSignal {
                    total_power_volts,
                    detector_max_volts: detector_high,
                });
            }
            (
                total_power_volts - detector_high,
                total_power_volts - detector_low,
            )
        }
    };

    Ok(ContrastEstimate {
        a: (exc_max / exc_min).ln(),
        v_min_volts: exc_min,
        v_max_volts: exc_max,
        low_clip_fraction,
        high_clip_fraction,
        sample_count: codes.len(),
    })
}

fn percentile(sorted: &[u16], q: f64) -> u16 {
    let index = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_codes(center: f64, amplitude: f64, n: usize) -> Vec<u16> {
        (0..n)
            .map(|i| {
                let phase = 2.0 * std::f64::consts::PI * (i as f64) * 7.0 / n as f64;
                (center + amplitude * phase.sin())
                    .round()
                    .clamp(0.0, 4_095.0) as u16
            })
            .collect()
    }

    #[test]
    fn recovers_known_contrast_from_synthetic_sine() {
        let calibration = AdcCalibration {
            dark_volts: 40.0 * (3.3 / 4_095.0),
            ..AdcCalibration::default()
        };
        // center 2048, amplitude 900 -> dark-corrected V ratio:
        let codes = sine_codes(2_048.0, 900.0, 4_096);
        let estimate = estimate_contrast(&codes, &calibration, ContrastGeometry::Direct)
            .expect("clean window estimates");

        let expected = ((2_048.0_f64 + 900.0 - 40.0) / (2_048.0 - 900.0 - 40.0)).ln();
        assert!(
            (estimate.a - expected).abs() < 0.01,
            "a={} expected~{expected}",
            estimate.a
        );
        assert!(estimate.low_clip_fraction == 0.0 && estimate.high_clip_fraction == 0.0);
    }

    #[test]
    fn direct_and_rejected_complement_recover_the_same_excitation_contrast() {
        // Excitation is a clean sine between exc_min and exc_max; the reject
        // port sees the complement I_tot - I_exc. Both geometries must recover
        // the same excitation log-contrast a = ln(exc_max / exc_min).
        let calibration = AdcCalibration::default();
        let volts_per_code = calibration.volts_per_code;
        let total_power_volts = 3_600.0 * volts_per_code;
        let exc_center = 1_600.0;
        let exc_amplitude = 900.0;

        let excitation_codes = sine_codes(exc_center, exc_amplitude, 4_096);
        let rejected_codes: Vec<u16> = excitation_codes.iter().map(|&code| 3_600 - code).collect();

        let direct = estimate_contrast(&excitation_codes, &calibration, ContrastGeometry::Direct)
            .expect("direct excitation window");
        let rejected = estimate_contrast(
            &rejected_codes,
            &calibration,
            ContrastGeometry::RejectedComplement { total_power_volts },
        )
        .expect("rejected complement window");

        let expected = ((exc_center + exc_amplitude) / (exc_center - exc_amplitude)).ln();
        assert!((direct.a - expected).abs() < 0.01, "direct a={}", direct.a);
        assert!(
            (rejected.a - direct.a).abs() < 0.01,
            "rejected a={} direct a={}",
            rejected.a,
            direct.a
        );
    }

    #[test]
    fn rejected_complement_rejects_a_total_power_anchor_below_the_signal() {
        let calibration = AdcCalibration::default();
        let codes = sine_codes(2_048.0, 900.0, 2_048);
        // Anchor far below the detector maximum (~2948 codes).
        let err = estimate_contrast(
            &codes,
            &calibration,
            ContrastGeometry::RejectedComplement {
                total_power_volts: 1_000.0 * calibration.volts_per_code,
            },
        )
        .expect_err("anchor below signal must be rejected");
        assert!(matches!(err, EstimateError::TotalPowerBelowSignal { .. }));
    }

    #[test]
    fn rejects_clipped_windows() {
        // Amplitude pushes past full scale -> clipping at the top rail.
        let codes = sine_codes(3_500.0, 900.0, 2_048);
        let err = estimate_contrast(&codes, &AdcCalibration::default(), ContrastGeometry::Direct)
            .expect_err("clipped window must be rejected");
        assert!(matches!(err, EstimateError::Clipped { .. }));
    }

    #[test]
    fn rejects_windows_without_dark_headroom() {
        let calibration = AdcCalibration {
            dark_volts: 1_300.0 * (3.3 / 4_095.0),
            ..AdcCalibration::default()
        };
        // Minimum (2048-900=1148) sits below the dark level (1300).
        let codes = sine_codes(2_048.0, 900.0, 2_048);
        let err = estimate_contrast(&codes, &calibration, ContrastGeometry::Direct)
            .expect_err("no headroom above dark must be rejected");
        assert_eq!(err, EstimateError::NoHeadroomAboveDark);
    }

    #[test]
    fn rejects_short_windows() {
        let err = estimate_contrast(
            &[100; 10],
            &AdcCalibration::default(),
            ContrastGeometry::Direct,
        )
        .expect_err("short window rejected");
        assert!(matches!(err, EstimateError::TooFewSamples { .. }));
    }

    #[test]
    fn single_sample_spikes_do_not_bias_the_contrast() {
        let mut codes = sine_codes(2_048.0, 500.0, 4_096);
        codes[7] = 4_000; // one hot spike, below the 1 - 99 percentile weight
        let clean = estimate_contrast(
            &sine_codes(2_048.0, 500.0, 4_096),
            &AdcCalibration::default(),
            ContrastGeometry::Direct,
        )
        .expect("clean");
        let spiked =
            estimate_contrast(&codes, &AdcCalibration::default(), ContrastGeometry::Direct)
                .expect("spiked");
        assert!((clean.a - spiked.a).abs() < 0.005);
    }
}
