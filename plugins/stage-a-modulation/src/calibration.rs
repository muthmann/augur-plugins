//! Measured Pockels/PBS transfer calibration: fits `V_null` and `Vπ` from a
//! sweep of settled `CONST` DAC codes against the photodiode level.
//!
//! The operator must not have to trust a nominal `Vπ` (knowledge base:
//! `methodology/pockels-waveform-linearisation.md` §4). This module turns a
//! table of `(DAC code, detector volts)` points into the lobe parameters the
//! optical inversion in [`crate::waveform`] needs.
//!
//! # Model
//!
//! ```text
//! P(c) = p0 + p1 · sin²(π (c − V_null) / (2 Vπ))
//! ```
//!
//! `p1` is **signed**, because the Stage-A photodiode sits behind the PBS
//! *reject* port and measures the complement `I_pd = I_tot − I_exc`, moving
//! *against* the excitation (knowledge base: `setup/optical-path.md`).
//!
//! The sign cannot be inferred from the sweep. `sin²` is symmetric about its
//! peak, so `(v, p0, p1)` and `(v + Vπ, p0 + p1, −p1)` describe the *same*
//! measured curve exactly; the data alone cannot say which extremum is the
//! excitation null. That is a physical fact about the port, not a fit
//! parameter, so [`fit_transfer`] takes the geometry as an **input** and picks
//! the matching representation. Getting it wrong would place `V_null` a
//! quarter wave off and run the drive on the inverted branch, so it is asked
//! rather than guessed.
//!
//! Two consequences worth stating, because they remove procedure rather than
//! add it:
//!
//! - **The shape is dark- and gain-immune.** `p0` absorbs the dark level and
//!   any DC offset, `p1` absorbs the front-end gain. `V_null`/`Vπ` therefore
//!   need neither a dark measurement nor the total-power anchor.
//! - **The absolute scale is not recoverable here.** On the reject port the
//!   residual transmitted floor cannot be separated from the anchor `I_tot`
//!   (knowledge base §4.4), so this module reports the detector extrema and
//!   explicitly does *not* derive a maximum achievable `a` from them.
//!
//! # Fit
//!
//! Because `sin²(x) = (1 − cos 2x)/2`, the model is exactly a constant plus
//! **one sinusoid of period `2Vπ`** — and a sinusoid of known period is linear
//! in its quadrature components. So for each candidate `Vπ` the phase (hence
//! `V_null`) and both amplitudes fall out of a 3×3 linear solve, and the
//! search is one-dimensional: scan `Vπ` over every period the sweep can
//! resolve, then refine. See [`solve_harmonic`].
//!
//! This matters beyond elegance. Seeding the period from the measured extrema
//! — the obvious approach — breaks on exactly the sweeps that matter: with a
//! real `Vπ` near 860 the DAC range holds ~2.4 lobes, so the global minimum
//! and maximum can sit whole periods apart and the seed is meaningless.

use std::f64::consts::PI;

use crate::waveform::{LobeInversion, DAC_FULL_SCALE};

/// Sweep direction, kept per point so ascending/descending repeatability can
/// be reported (knowledge base §5 acceptance test 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Ascending,
    Descending,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ascending => "up",
            Self::Descending => "down",
        }
    }
}

/// One settled `(DAC code, detector level)` measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepPoint {
    pub code: u16,
    pub direction: Direction,
    /// Raw detector level in volts, as published by the photodiode owner.
    pub volts: f64,
    /// Spread over the averaged window; a settle-quality witness.
    pub peak_to_peak_volts: f64,
    pub clipped: bool,
}

/// Which port the detector watches. An input to the fit, not an output: the
/// swept curve is identical either way (see the module docs), so this states
/// the bench geometry that resolves the ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectorGeometry {
    /// Detector darkens as excitation rises — the Stage-A PBS reject port, and
    /// the default: on this bench the geometry is settled by construction.
    RejectedComplement,
    /// Detector brightens with excitation (a transmitted-port tap).
    Direct,
}

impl DetectorGeometry {
    pub const VARIANTS: [Self; 2] = [Self::RejectedComplement, Self::Direct];

    /// Named by what the operator can *observe*, not by optics jargon: the
    /// question the setting actually asks is which way the photodiode reading
    /// moves when the light reaching the sample gets brighter.
    pub fn name(self) -> &'static str {
        match self {
            Self::RejectedComplement => "REJECT PORT (PD falls as light rises)",
            Self::Direct => "DIRECT (PD rises with light)",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::VARIANTS.into_iter().find(|kind| kind.name() == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransferFit {
    /// DAC code at the excitation minimum.
    pub v_null_dac: f64,
    /// DAC codes from `v_null` to the excitation maximum (quarter wave).
    pub v_pi_dac: f64,
    /// Detector volts at the excitation null (`p0`).
    pub offset_volts: f64,
    /// Signed detector span across one lobe (`p1`); negative on the reject port.
    pub span_volts: f64,
    pub rms_residual_volts: f64,
    /// Residual as a fraction of the detector span — the headline fit quality.
    pub quality: f64,
    pub geometry: DetectorGeometry,
    /// Mean |ascending − descending| at matched codes, as a fraction of the
    /// span. `None` when the sweep ran in one direction only.
    pub hysteresis: Option<f64>,
    /// Fraction of one full lobe (`Vπ` codes) the sweep actually covered.
    /// Below ~1 the quarter-wave distance is extrapolated, not measured.
    pub lobe_coverage: f64,
    /// Points discarded as wild before the final fit. A couple is ordinary; a
    /// large share means the sweep, not the model, is the problem.
    pub rejected_points: usize,
    /// Every measured point, rejected ones included, so the plot shows what was
    /// actually seen.
    pub points: Vec<SweepPoint>,
}

impl TransferFit {
    pub fn inversion(&self) -> LobeInversion {
        LobeInversion {
            v_null_dac: self.v_null_dac,
            v_pi_dac: self.v_pi_dac,
        }
    }

    /// DAC code at the excitation maximum — the second of the two codes the
    /// drive is configured with.
    pub fn v_peak_dac(&self) -> f64 {
        self.v_null_dac + self.v_pi_dac
    }

    /// Detector extremum at the excitation null. On the reject port this is the
    /// detector *maximum* and a **lower bound** on the total-power anchor
    /// `I_tot` — not the anchor itself, because the residual transmitted floor
    /// is not separable here (knowledge base §4.4).
    pub fn detector_volts_at_null(&self) -> f64 {
        self.offset_volts
    }

    /// Detector extremum at the excitation maximum.
    pub fn detector_volts_at_peak(&self) -> f64 {
        self.offset_volts + self.span_volts
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FitError {
    /// Fewer points than parameters can be resolved from.
    TooFewPoints { count: usize, minimum: usize },
    /// The detector never moved: no lobe to fit (light blocked, no drive
    /// reaching the cell, or the sweep span sits in a flat region).
    NoModulation,
    /// A fitted lobe exists but no `[V_null, V_null+Vπ]` fits inside the
    /// commandable range, so no monotonic branch is usable.
    NoLobeInRange { v_pi_dac: f64 },
}

impl std::fmt::Display for FitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooFewPoints { count, minimum } => {
                write!(f, "only {count} sweep points (minimum {minimum})")
            }
            Self::NoModulation => f.write_str(
                "the detector level did not change across the sweep; check the light path, \
                 the HV amplifier, and that the photodiode is connected",
            ),
            Self::NoLobeInRange { v_pi_dac } => write!(
                f,
                "fitted Vπ = {v_pi_dac:.0} DAC codes leaves no full lobe inside the max limit; \
                 raise the max limit or re-check the drive gain"
            ),
        }
    }
}

impl std::error::Error for FitError {}

/// Smallest usable sweep: four points per fitted parameter.
pub const MIN_POINTS: usize = 16;
/// A detector span below this is treated as noise rather than a lobe.
const MIN_SPAN_VOLTS: f64 = 0.01;

/// Least-squares solution for one candidate quarter wave `w`.
struct Harmonic {
    /// Mean level `A`, and the quadrature amplitudes of `cos`/`sin(πc/w)`.
    mean: f64,
    amplitude: f64,
    phase: f64,
    sse: f64,
}

/// Fits `P = A + B·cos(πc/w) + C·sin(πc/w)` for a fixed `w`.
///
/// This is the whole trick that makes the search one-dimensional. Because
/// `sin²(x) = (1 − cos 2x)/2`, the lobe model
/// `p0 + p1·sin²(π(c − v)/(2w))` is *exactly* a constant plus one sinusoid of
/// period `2w` — and a sinusoid of known period is **linear** in its
/// quadrature components. So `V_null` (a phase) and both amplitudes drop out
/// of a 3×3 normal-equation solve, and only `Vπ` is ever searched. No seeding
/// from measured extrema, which is what fails once a sweep spans several
/// lobes and the global extrema sit periods apart.
fn solve_harmonic(points: &[SweepPoint], w: f64) -> Harmonic {
    let n = points.len() as f64;
    let (mut s_c, mut s_s, mut s_cc, mut s_ss, mut s_cs) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let (mut s_y, mut s_yc, mut s_ys) = (0.0, 0.0, 0.0);
    for point in points {
        let theta = PI * f64::from(point.code) / w;
        let (sin, cos) = theta.sin_cos();
        s_c += cos;
        s_s += sin;
        s_cc += cos * cos;
        s_ss += sin * sin;
        s_cs += cos * sin;
        s_y += point.volts;
        s_yc += point.volts * cos;
        s_ys += point.volts * sin;
    }
    // Symmetric 3×3 normal equations for (A, B, C), solved by cofactors.
    let m = [[n, s_c, s_s], [s_c, s_cc, s_cs], [s_s, s_cs, s_ss]];
    let rhs = [s_y, s_yc, s_ys];
    let cofactor = [
        m[1][1] * m[2][2] - m[1][2] * m[2][1],
        m[1][2] * m[2][0] - m[1][0] * m[2][2],
        m[1][0] * m[2][1] - m[1][1] * m[2][0],
    ];
    let determinant = m[0][0] * cofactor[0] + m[0][1] * cofactor[1] + m[0][2] * cofactor[2];
    if determinant.abs() < 1e-12 {
        return Harmonic {
            mean: s_y / n,
            amplitude: 0.0,
            phase: 0.0,
            sse: f64::MAX,
        };
    }
    let solve = |column: usize| {
        let mut augmented = m;
        for row in 0..3 {
            augmented[row][column] = rhs[row];
        }
        (augmented[0][0] * (augmented[1][1] * augmented[2][2] - augmented[1][2] * augmented[2][1])
            - augmented[0][1]
                * (augmented[1][0] * augmented[2][2] - augmented[1][2] * augmented[2][0])
            + augmented[0][2]
                * (augmented[1][0] * augmented[2][1] - augmented[1][1] * augmented[2][0]))
            / determinant
    };
    let (a, b, c) = (solve(0), solve(1), solve(2));
    let sse = points
        .iter()
        .map(|point| {
            let theta = PI * f64::from(point.code) / w;
            let residual = point.volts - (a + b * theta.cos() + c * theta.sin());
            residual * residual
        })
        .sum();
    Harmonic {
        mean: a,
        amplitude: b.hypot(c),
        phase: c.atan2(b),
        sse,
    }
}

/// Golden-section minimisation of `f` on `[lo, hi]`, used one axis at a time.
fn golden_min(lo: f64, hi: f64, tolerance: f64, f: impl Fn(f64) -> f64) -> f64 {
    const INV_PHI: f64 = 0.618_033_988_749_895;
    let (mut lo, mut hi) = (lo, hi);
    let mut c = hi - (hi - lo) * INV_PHI;
    let mut d = lo + (hi - lo) * INV_PHI;
    let (mut fc, mut fd) = (f(c), f(d));
    while (hi - lo) > tolerance {
        if fc < fd {
            hi = d;
            d = c;
            fd = fc;
            c = hi - (hi - lo) * INV_PHI;
            fc = f(c);
        } else {
            lo = c;
            c = d;
            fc = fd;
            d = lo + (hi - lo) * INV_PHI;
            fd = f(d);
        }
    }
    0.5 * (lo + hi)
}

/// Mean of the points at each distinct code, smoothed over three neighbours, so
/// the seed extrema are not chosen by a single noisy sample.
fn smoothed_profile(points: &[SweepPoint]) -> Vec<(f64, f64)> {
    let mut codes: Vec<u16> = points.iter().map(|point| point.code).collect();
    codes.sort_unstable();
    codes.dedup();
    let means: Vec<(f64, f64)> = codes
        .iter()
        .map(|&code| {
            let matching: Vec<f64> = points
                .iter()
                .filter(|point| point.code == code)
                .map(|point| point.volts)
                .collect();
            (
                f64::from(code),
                matching.iter().sum::<f64>() / matching.len() as f64,
            )
        })
        .collect();
    (0..means.len())
        .map(|index| {
            let lo = index.saturating_sub(1);
            let hi = (index + 2).min(means.len());
            let window = &means[lo..hi];
            (
                means[index].0,
                window.iter().map(|(_, v)| v).sum::<f64>() / window.len() as f64,
            )
        })
        .collect()
}

/// Shifts `v` by whole lobe periods to the **lowest** null whose lobe
/// `[v, v + w]` fits inside `0..=max_code`.
///
/// A sweep across several periods finds several equally valid nulls, so the
/// choice needs a rule the operator can predict rather than a nearest-match.
/// The lowest one drives the Pockels cell at the smallest codes — least
/// voltage across the crystal, most headroom under the max limit.
fn select_lobe(v: f64, w: f64, max_code: f64) -> Option<f64> {
    // Sub-code precision is meaningless on a 12-bit DAC, so a null fitted a
    // hair below 0 (or a peak a hair past the ceiling) is snapped into range
    // rather than refused — otherwise a lobe nulling exactly at code 0 fails
    // on fit noise alone.
    const TOLERANCE: f64 = 1.0;
    // The model repeats every `2w` in code, and `v + kw` for odd `k` is the
    // same branch mirrored, so stepping by `2w` enumerates every null.
    let period = 2.0 * w;
    let mut candidate = v - period * ((v / period).floor() + 1.0);
    while candidate <= max_code + TOLERANCE {
        if candidate >= -TOLERANCE && candidate + w <= max_code + TOLERANCE {
            return Some(candidate.clamp(0.0, (max_code - w).max(0.0)));
        }
        candidate += period;
    }
    None
}

/// Mean |ascending − descending| at codes visited in both directions, as a
/// fraction of the detector span.
fn hysteresis_fraction(points: &[SweepPoint], span: f64) -> Option<f64> {
    let mut differences = Vec::new();
    for up in points
        .iter()
        .filter(|point| point.direction == Direction::Ascending)
    {
        if let Some(down) = points
            .iter()
            .find(|point| point.direction == Direction::Descending && point.code == up.code)
        {
            differences.push((up.volts - down.volts).abs());
        }
    }
    if differences.is_empty() || span.abs() < f64::EPSILON {
        return None;
    }
    Some(differences.iter().sum::<f64>() / differences.len() as f64 / span.abs())
}

/// Scans the quarter wave over every period the sweep could resolve, then
/// refines. Returns the best `(Vπ, harmonic)`.
fn fit_period(points: &[SweepPoint], swept_span: f64) -> Option<(f64, Harmonic)> {
    // From four samples per lobe (below that the lobe is aliased) out to a
    // lobe twice the swept span (a barely-curved arc). Log-spaced, because a
    // fixed step wastes resolution at long periods and misses short ones.
    let point_spacing = swept_span / points.len().max(2) as f64;
    let w_min = (2.0 * point_spacing).max(1.0);
    let w_max = (2.0 * swept_span).max(w_min * 1.5);
    const SCAN_STEPS: usize = 600;
    let log_step = (w_max / w_min).ln() / SCAN_STEPS as f64;
    let mut best: Option<(f64, f64)> = None; // (sse, w)
    for step in 0..=SCAN_STEPS {
        let w = w_min * (log_step * step as f64).exp();
        let sse = solve_harmonic(points, w).sse;
        if best.is_none_or(|(previous, _)| sse < previous) {
            best = Some((sse, w));
        }
    }
    let (_, coarse_w) = best?;
    // Refine inside one scan cell, where the SSE is unimodal.
    let cell = coarse_w * log_step;
    let w = golden_min(
        (coarse_w - cell).max(w_min * 0.5),
        coarse_w + cell,
        1e-3,
        |candidate| solve_harmonic(points, candidate).sse,
    );
    let harmonic = solve_harmonic(points, w);
    Some((w, harmonic))
}

/// Points whose residual against `harmonic` is not wildly out of family.
///
/// The cut is on the **median** absolute residual, not the mean or the
/// standard deviation: those are themselves dragged out by the very points
/// being looked for. `6 × median` is roughly 4σ for Gaussian noise, so ordinary
/// scatter survives untouched and only genuine strays are dropped.
fn without_outliers(points: &[SweepPoint], w: f64, harmonic: &Harmonic) -> Vec<SweepPoint> {
    let residual = |point: &SweepPoint| {
        let theta = PI * f64::from(point.code) / w;
        point.volts - (harmonic.mean + harmonic.amplitude * (theta - harmonic.phase).cos())
    };
    let mut magnitudes: Vec<f64> = points.iter().map(|point| residual(point).abs()).collect();
    magnitudes.sort_by(f64::total_cmp);
    let median = magnitudes[magnitudes.len() / 2];
    if median <= 0.0 {
        return points.to_vec();
    }
    let limit = 6.0 * median;
    points
        .iter()
        .filter(|point| residual(point).abs() <= limit)
        .copied()
        .collect()
}

/// Fits the lobe. `max_code` is the highest commandable DAC code (the drive's
/// max limit), which constrains which branch can be used; `geometry` resolves
/// the null/peak ambiguity the data cannot (see the module docs).
pub fn fit_transfer(
    points: &[SweepPoint],
    max_code: f64,
    geometry: DetectorGeometry,
) -> Result<TransferFit, FitError> {
    if points.len() < MIN_POINTS {
        return Err(FitError::TooFewPoints {
            count: points.len(),
            minimum: MIN_POINTS,
        });
    }
    let profile = smoothed_profile(points);
    let min_volts = profile.iter().map(|(_, v)| *v).fold(f64::MAX, f64::min);
    let max_volts = profile.iter().map(|(_, v)| *v).fold(f64::MIN, f64::max);
    if max_volts - min_volts < MIN_SPAN_VOLTS {
        return Err(FitError::NoModulation);
    }

    let swept_lo = profile.first().map(|(code, _)| *code).unwrap_or(0.0);
    let swept_hi = profile.last().map(|(code, _)| *code).unwrap_or(max_code);
    let swept_span = (swept_hi - swept_lo).max(1.0);

    // A single stray point — one window caught mid-settle, one stream hiccup —
    // barely moves the fitted period but inflates the RMS residual several
    // fold. Fit once, drop the points the fit says are wild, and fit again on
    // what is left, so the reported residual describes the curve rather than
    // the worst sample.
    let (w, harmonic, rejected_points) = {
        let first = fit_period(points, swept_span).ok_or(FitError::NoModulation)?;
        let kept = without_outliers(points, first.0, &first.1);
        if kept.len() < points.len() && kept.len() >= MIN_POINTS {
            match fit_period(&kept, swept_span) {
                Some((w, harmonic)) => (w, harmonic, points.len() - kept.len()),
                None => (first.0, first.1, 0),
            }
        } else {
            (first.0, first.1, 0)
        }
    };
    // `A + R·cos(θ − φ)` with `θ = πc/w` is the same curve as
    // `p0 + p1·sin²(π(c − v)/(2w))` with `|p1| = 2R`. Which of the two signs
    // of `p1` applies — and therefore whether the null sits at the phase or a
    // quarter wave past it — is the geometry question the data cannot answer.
    let radius = harmonic.amplitude;
    let (v, p0, p1) = match geometry {
        DetectorGeometry::RejectedComplement => (
            harmonic.phase * w / PI,
            harmonic.mean + radius,
            -2.0 * radius,
        ),
        DetectorGeometry::Direct => (
            harmonic.phase * w / PI + w,
            harmonic.mean - radius,
            2.0 * radius,
        ),
    };
    if p1.abs() < MIN_SPAN_VOLTS {
        return Err(FitError::NoModulation);
    }

    let v_null = select_lobe(v, w, max_code).ok_or(FitError::NoLobeInRange { v_pi_dac: w })?;

    // Over the points the fit actually used: dividing the kept residual by the
    // full count would flatter the number.
    let rms = (harmonic.sse / (points.len() - rejected_points).max(1) as f64).sqrt();
    Ok(TransferFit {
        v_null_dac: v_null,
        v_pi_dac: w,
        offset_volts: p0,
        span_volts: p1,
        rms_residual_volts: rms,
        quality: rms / p1.abs(),
        geometry,
        hysteresis: hysteresis_fraction(points, p1),
        lobe_coverage: swept_span / w,
        rejected_points,
        // Every measured point is kept for the plot, rejected ones included:
        // seeing the strays next to the fit is how the operator judges it.
        points: points.to_vec(),
    })
}

/// Ascending then descending sweep codes over `0..=max_code`.
pub fn sweep_codes(
    max_code: u16,
    points_per_pass: usize,
    both_directions: bool,
) -> Vec<(u16, Direction)> {
    let points_per_pass = points_per_pass.max(2);
    let max_code = max_code.min(DAC_FULL_SCALE);
    let ascending: Vec<u16> = (0..points_per_pass)
        .map(|index| {
            (f64::from(max_code) * index as f64 / (points_per_pass - 1) as f64).round() as u16
        })
        .collect();
    let mut codes: Vec<(u16, Direction)> = ascending
        .iter()
        .map(|&code| (code, Direction::Ascending))
        .collect();
    if both_directions {
        codes.extend(
            ascending
                .iter()
                .rev()
                .map(|&code| (code, Direction::Descending)),
        );
    }
    codes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesizes a sweep of a known lobe as seen through a given port.
    /// `noise` is a deterministic zig-zag, not an RNG, so failures reproduce.
    fn synthetic_sweep(
        v_null: f64,
        v_pi: f64,
        offset: f64,
        span: f64,
        max_code: u16,
        noise: f64,
        both_directions: bool,
    ) -> Vec<SweepPoint> {
        let lobe = LobeInversion {
            v_null_dac: v_null,
            v_pi_dac: v_pi,
        };
        sweep_codes(max_code, 49, both_directions)
            .into_iter()
            .enumerate()
            .map(|(index, (code, direction))| {
                let u = lobe.u_for_dac(f64::from(code));
                let wobble = if index % 2 == 0 { noise } else { -noise };
                SweepPoint {
                    code,
                    direction,
                    volts: offset + span * u + wobble,
                    peak_to_peak_volts: 0.002,
                    clipped: false,
                }
            })
            .collect()
    }

    #[test]
    fn recovers_a_known_lobe_from_the_reject_port() {
        // Reject port: detector is brightest (2.4 V) at the excitation null.
        let points = synthetic_sweep(300.0, 1_600.0, 2.4, -2.2, 4_095, 0.004, true);
        let fit = fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement)
            .expect("fits the lobe");

        assert!(
            (fit.v_null_dac - 300.0).abs() < 5.0,
            "V_null = {}",
            fit.v_null_dac
        );
        assert!(
            (fit.v_pi_dac - 1_600.0).abs() < 10.0,
            "Vπ = {}",
            fit.v_pi_dac
        );
        assert!(fit.span_volts < 0.0, "reject port darkens with excitation");
        assert!((fit.detector_volts_at_null() - 2.4).abs() < 0.02);
        assert!(fit.quality < 0.01, "quality = {}", fit.quality);
        // Both directions carry the same synthetic curve, so the only
        // difference at matched codes is the alternating wobble.
        assert!(fit.hysteresis.expect("both directions") < 0.01);
    }

    #[test]
    fn recovers_the_same_lobe_from_a_direct_detector() {
        // Same physical lobe, opposite port: dim at the null, bright at peak.
        let points = synthetic_sweep(300.0, 1_600.0, 0.2, 2.2, 4_095, 0.004, true);
        let fit = fit_transfer(&points, 4_095.0, DetectorGeometry::Direct).expect("fits the lobe");

        assert!(
            (fit.v_null_dac - 300.0).abs() < 5.0,
            "V_null = {}",
            fit.v_null_dac
        );
        assert!((fit.v_pi_dac - 1_600.0).abs() < 10.0);
        assert!(fit.span_volts > 0.0, "direct detector brightens");
    }

    #[test]
    fn geometry_selects_between_the_two_equivalent_representations() {
        // One curve, two readings. Declaring the wrong port must move V_null by
        // exactly a quarter wave — the failure this input exists to prevent.
        let points = synthetic_sweep(300.0, 1_600.0, 2.4, -2.2, 4_095, 0.0, false);
        let reject =
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement).expect("fits");
        let direct = fit_transfer(&points, 4_095.0, DetectorGeometry::Direct).expect("fits");

        assert!((reject.v_null_dac - 300.0).abs() < 5.0);
        assert!(
            ((direct.v_null_dac - reject.v_null_dac).abs() - reject.v_pi_dac).abs() < 10.0,
            "direct = {}, reject = {}, Vπ = {}",
            direct.v_null_dac,
            reject.v_null_dac,
            reject.v_pi_dac
        );
        // Both describe the measured curve equally well; only the physics
        // distinguishes them.
        assert!((reject.rms_residual_volts - direct.rms_residual_volts).abs() < 1e-6);
    }

    #[test]
    fn resolves_a_sweep_spanning_several_lobes() {
        // A real Vπ near 860 puts ~2.4 lobes inside the DAC range. Seeding the
        // period from the global extrema fails here — they can sit whole
        // periods apart — which is why the period is scanned, not seeded.
        let points = synthetic_sweep(1_630.0, 860.0, 2.4, -2.2, 4_095, 0.003, true);
        let fit = fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement)
            .expect("fits a multi-lobe sweep");
        assert!((fit.v_pi_dac - 860.0).abs() < 10.0, "Vπ = {}", fit.v_pi_dac);
        // Any null is a valid answer as long as it names a real one and the
        // lobe it opens fits inside the range.
        let offset = (fit.v_null_dac - 1_630.0).rem_euclid(2.0 * 860.0);
        assert!(
            offset.min(2.0 * 860.0 - offset) < 10.0,
            "V_null = {} is not a null of the swept lobe",
            fit.v_null_dac
        );
        assert!(fit.v_null_dac >= 0.0 && fit.v_null_dac + fit.v_pi_dac <= 4_095.0);
        assert!(fit.quality < 0.01, "quality = {}", fit.quality);
        assert!(fit.lobe_coverage > 4.0, "coverage = {}", fit.lobe_coverage);
    }

    #[test]
    fn a_null_at_code_zero_is_not_lost_to_fit_noise() {
        // V_null = 0 fits a hair either side of the rail; snapping sub-code
        // slack into range is the difference between a usable calibration and
        // a refusal.
        let points = synthetic_sweep(0.0, 1_200.0, 2.4, -2.2, 4_095, 0.003, false);
        let fit =
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement).expect("fits");
        assert!(fit.v_null_dac.abs() < 2.0, "V_null = {}", fit.v_null_dac);
    }

    #[test]
    fn picks_a_lobe_that_fits_inside_the_max_limit() {
        // Null at 2600 with Vπ = 1600 would put peak light at 4200, past the
        // rail; the previous null one period down (2600 − 3200 < 0) does not
        // fit either, so only a lower branch inside the range is acceptable.
        let points = synthetic_sweep(1_000.0, 900.0, 2.4, -2.2, 4_095, 0.002, false);
        let fit =
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement).expect("fits");
        assert!(fit.v_null_dac >= 0.0);
        assert!(
            fit.v_null_dac + fit.v_pi_dac <= 4_095.0,
            "peak light at {} leaves the rail",
            fit.v_null_dac + fit.v_pi_dac
        );
    }

    #[test]
    fn refuses_a_flat_sweep() {
        let points: Vec<SweepPoint> = sweep_codes(4_095, 49, false)
            .into_iter()
            .map(|(code, direction)| SweepPoint {
                code,
                direction,
                volts: 1.5,
                peak_to_peak_volts: 0.001,
                clipped: false,
            })
            .collect();
        assert_eq!(
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement),
            Err(FitError::NoModulation)
        );
    }

    #[test]
    fn refuses_too_few_points() {
        let points = synthetic_sweep(300.0, 1_600.0, 2.4, -2.2, 4_095, 0.0, false);
        assert!(matches!(
            fit_transfer(&points[..4], 4_095.0, DetectorGeometry::RejectedComplement),
            Err(FitError::TooFewPoints { .. })
        ));
    }

    #[test]
    fn reports_hysteresis_between_the_two_passes() {
        // Descending runs 20 mV below ascending: a real hysteresis signature.
        let mut points = synthetic_sweep(300.0, 1_600.0, 2.4, -2.2, 4_095, 0.0, true);
        for point in &mut points {
            if point.direction == Direction::Descending {
                point.volts -= 0.02;
            }
        }
        let fit =
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement).expect("fits");
        let hysteresis = fit.hysteresis.expect("both directions");
        assert!(
            (hysteresis - 0.02 / 2.2).abs() < 1e-3,
            "hysteresis = {hysteresis}"
        );
    }

    #[test]
    fn single_direction_sweep_reports_no_hysteresis() {
        let points = synthetic_sweep(300.0, 1_600.0, 2.4, -2.2, 4_095, 0.002, false);
        let fit =
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement).expect("fits");
        assert_eq!(fit.hysteresis, None);
    }

    #[test]
    fn lobe_coverage_flags_an_extrapolated_quarter_wave() {
        // Sweeping only to code 800 with Vπ = 1600 sees half a lobe.
        let points = synthetic_sweep(0.0, 1_600.0, 2.4, -2.2, 800, 0.001, false);
        let fit =
            fit_transfer(&points, 4_095.0, DetectorGeometry::RejectedComplement).expect("fits");
        assert!(fit.lobe_coverage < 0.75, "coverage = {}", fit.lobe_coverage);
    }

    #[test]
    fn sweep_codes_span_the_range_in_both_directions() {
        let codes = sweep_codes(4_000, 5, true);
        let ascending: Vec<u16> = codes
            .iter()
            .filter(|(_, direction)| *direction == Direction::Ascending)
            .map(|(code, _)| *code)
            .collect();
        assert_eq!(ascending, [0, 1_000, 2_000, 3_000, 4_000]);
        let descending: Vec<u16> = codes
            .iter()
            .filter(|(_, direction)| *direction == Direction::Descending)
            .map(|(code, _)| *code)
            .collect();
        assert_eq!(descending, [4_000, 3_000, 2_000, 1_000, 0]);
        assert_eq!(sweep_codes(4_000, 5, false).len(), 5);
    }
}
