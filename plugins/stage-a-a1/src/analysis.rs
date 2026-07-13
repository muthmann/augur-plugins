//! Statistical core of the A1 minimum-depth measurement.
//!
//! ## Why phase, not raw counts
//!
//! Background activity (BA) is uniform in modulation phase; genuine
//! modulation events are phase-locked to the drive. Testing for a
//! phase-locked component (Rayleigh test) therefore discounts uniform
//! background *automatically*, instead of requiring an absolute background
//! rate that drifts with temperature. "Mean events per half-cycle > 1" is
//! kept as the *estimator* (it is the quantity `⌊a·|H|/C⌋` predicts), but
//! the *detector* is the phase test.
//!
//! ## Cycle fiducials without the trigger cable
//!
//! With the phase-0 TTL wired, `frame.external_triggers()` marks each cycle
//! on the camera clock. Without it, the Teensy and camera clocks drift
//! (tens of ppm — folding dies after ~0.1 s at 10 kHz), so the drive
//! frequency is *refined against the events themselves*: scan a small
//! window around the commanded frequency and keep the value maximising the
//! Rayleigh power. The scan multiplicity is charged to the significance
//! test (Bonferroni).
//!
//! ## a_min as a fitted crossing
//!
//! Near threshold the 0→1 step of `⌊a·|H|/C⌋` is smeared by shot-noise
//! first-passage randomness and per-pixel threshold dispersion, so "events
//! just vanish" is not a crisp edge. a_min is defined as the fitted point
//! where the mean phase-locked events per half-cycle crosses 0.5, from a
//! probit-in-ln(a) fit over the transition, with a profile confidence
//! interval. The plateau of a_min(f) reads out the contrast quantum C.

// ---------------------------------------------------------------------------
// Phase folding
// ---------------------------------------------------------------------------

/// Folds event timestamps at `frequency_hz` relative to `t0_us`,
/// returning phases in `[0, 1)`.
pub fn fold_phases(
    timestamps_us: impl Iterator<Item = u64>,
    t0_us: u64,
    frequency_hz: f64,
) -> Vec<f64> {
    let period_us = 1.0e6 / frequency_hz;
    timestamps_us
        .map(|t| {
            let dt = t.saturating_sub(t0_us) as f64;
            (dt / period_us).fract()
        })
        .collect()
}

/// Folds against explicit cycle-start fiducials (rising trigger edges):
/// each event's phase is its position inside the enclosing cycle. Events
/// before the first or after the last fiducial are dropped (their cycle
/// length is unknown).
pub fn fold_phases_with_fiducials(events: &[u64], cycle_starts_us: &[u64]) -> Vec<f64> {
    if cycle_starts_us.len() < 2 {
        return Vec::new();
    }
    let mut phases = Vec::with_capacity(events.len());
    for &t in events {
        let idx = match cycle_starts_us.binary_search(&t) {
            Ok(i) => i,
            Err(0) => continue,
            Err(i) => i - 1,
        };
        if idx + 1 >= cycle_starts_us.len() {
            continue;
        }
        let start = cycle_starts_us[idx];
        let end = cycle_starts_us[idx + 1];
        if end <= start {
            continue;
        }
        phases.push((t - start) as f64 / (end - start) as f64);
    }
    phases
}

// ---------------------------------------------------------------------------
// Rayleigh test
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RayleighResult {
    pub n: usize,
    /// Resultant length in [0, 1].
    pub r: f64,
    /// Z = n·R².
    pub z: f64,
    /// Approximate p-value under uniformity, `exp(-Z)` with the standard
    /// small-sample correction (Zar / Wilkie).
    pub p_value: f64,
}

pub fn rayleigh_test(phases: &[f64]) -> RayleighResult {
    let n = phases.len();
    if n == 0 {
        return RayleighResult {
            n,
            r: 0.0,
            z: 0.0,
            p_value: 1.0,
        };
    }
    let (mut c, mut s) = (0.0_f64, 0.0_f64);
    for &phase in phases {
        let angle = 2.0 * std::f64::consts::PI * phase;
        c += angle.cos();
        s += angle.sin();
    }
    let r = (c * c + s * s).sqrt() / n as f64;
    let z = n as f64 * r * r;
    let nf = n as f64;
    let p = (-z).exp() * (1.0 + (2.0 * z - z * z) / (4.0 * nf)
        - (24.0 * z - 132.0 * z * z + 76.0 * z.powi(3) - 9.0 * z.powi(4)) / (288.0 * nf * nf));
    RayleighResult {
        n,
        r,
        z,
        p_value: p.clamp(0.0, 1.0),
    }
}

// ---------------------------------------------------------------------------
// Frequency refinement (clock-skew recovery without a trigger cable)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrequencyLock {
    pub frequency_hz: f64,
    pub rayleigh: RayleighResult,
    /// Number of candidate frequencies tested — multiply into the
    /// significance threshold (Bonferroni).
    pub trials: usize,
}

/// Scans `±window_ppm` around `nominal_hz` and returns the frequency with
/// the maximum Rayleigh power. The step is chosen so consecutive candidates
/// dephase by ≤ 0.1 cycle over the observation span (finer is wasted).
pub fn refine_frequency(
    timestamps_us: &[u64],
    nominal_hz: f64,
    window_ppm: f64,
) -> Option<FrequencyLock> {
    let (&first, &last) = (timestamps_us.first()?, timestamps_us.last()?);
    let span_s = (last.saturating_sub(first)) as f64 / 1.0e6;
    if span_s <= 0.0 {
        return None;
    }
    let df_step = 0.1 / span_s;
    let half_window_hz = nominal_hz * window_ppm * 1e-6;
    let steps = ((half_window_hz / df_step).ceil() as i64).clamp(0, 5_000);
    let mut best: Option<FrequencyLock> = None;
    let trials = (2 * steps + 1) as usize;
    for k in -steps..=steps {
        let f = nominal_hz + k as f64 * df_step;
        if f <= 0.0 {
            continue;
        }
        let phases = fold_phases(timestamps_us.iter().copied(), first, f);
        let stat = rayleigh_test(&phases);
        if best.as_ref().is_none_or(|b| stat.z > b.rayleigh.z) {
            best = Some(FrequencyLock {
                frequency_hz: f,
                rayleigh: stat,
                trials,
            });
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Phase-locked excess (the events/half-cycle estimator)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct PhaseHistogram {
    pub bins: Vec<u32>,
    pub total: usize,
}

pub fn phase_histogram(phases: &[f64], bin_count: usize) -> PhaseHistogram {
    let mut bins = vec![0_u32; bin_count.max(1)];
    for &phase in phases {
        let idx = ((phase * bins.len() as f64) as usize).min(bins.len() - 1);
        bins[idx] += 1;
    }
    PhaseHistogram {
        bins,
        total: phases.len(),
    }
}

/// Estimates the phase-locked event count above the uniform background.
///
/// The per-bin background is the *median* bin occupancy — robust because
/// the locked cluster occupies a minority of bins. Returns the summed
/// positive excess. Dividing by the number of observed cycles gives the
/// mean phase-locked events per cycle (per polarity: one burst per cycle).
pub fn phase_locked_excess(histogram: &PhaseHistogram) -> f64 {
    if histogram.bins.is_empty() {
        return 0.0;
    }
    let mut sorted = histogram.bins.clone();
    sorted.sort_unstable();
    let median = f64::from(sorted[sorted.len() / 2]);
    histogram
        .bins
        .iter()
        .map(|&count| (f64::from(count) - median).max(0.0))
        .sum()
}

// ---------------------------------------------------------------------------
// Detection verdict for one (frequency, amplitude) measurement
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DetectionVerdict {
    pub detected: bool,
    pub p_value: f64,
    /// Bonferroni-corrected significance threshold actually applied.
    pub alpha_effective: f64,
    /// Mean phase-locked events per cycle (per polarity), background-free.
    pub locked_events_per_cycle: f64,
}

/// Decides whether phase-locked modulation events are present.
///
/// `alpha` is the per-measurement false-positive budget; `trials` is the
/// look-elsewhere multiplicity (frequency-scan candidates × bisection
/// steps), charged via Bonferroni.
pub fn detect(
    rayleigh: RayleighResult,
    excess: f64,
    observed_cycles: f64,
    alpha: f64,
    trials: usize,
) -> DetectionVerdict {
    let alpha_effective = alpha / trials.max(1) as f64;
    DetectionVerdict {
        detected: rayleigh.p_value < alpha_effective,
        p_value: rayleigh.p_value,
        alpha_effective,
        locked_events_per_cycle: if observed_cycles > 0.0 {
            excess / observed_cycles
        } else {
            0.0
        },
    }
}

// ---------------------------------------------------------------------------
// a_min fit: probit in ln(a) with profile CI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MinDepthFit {
    /// a at which the mean locked events/half-cycle crosses 0.5.
    pub a_min: f64,
    /// Profile interval (Δ SSE ≤ SSE_min · (1 + 2/dof)); honest-but-cheap.
    pub a_min_low: f64,
    pub a_min_high: f64,
    /// Transition width in ln(a) — first look at σ_C + FPT smear.
    pub sigma_ln_a: f64,
    pub points_used: usize,
}

/// One measured amplitude point for the fit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthPoint {
    /// Measured optical log-contrast (photodiode, never the drive code).
    pub a: f64,
    /// Mean phase-locked events per half-cycle at this contrast.
    pub events_per_half_cycle: f64,
}

fn standard_normal_cdf(z: f64) -> f64 {
    // Abramowitz & Stegun 7.1.26 via erf; |error| < 1.5e-7.
    let x = z / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.327_591_1 * x.abs());
    let poly = t
        * (0.254_829_592
            + t * (-0.284_496_736 + t * (1.421_413_741 + t * (-1.453_152_027 + t * 1.061_405_429))));
    let erf_abs = 1.0 - poly * (-x * x).exp();
    let erf = if x >= 0.0 { erf_abs } else { -erf_abs };
    0.5 * (1.0 + erf)
}

/// Fits `N(a) = Φ((ln a − μ)/σ)` over the transition region and reports
/// `a_min = e^μ` (the N = 0.5 crossing). Points far above the first step
/// (`N > 1.5`) are excluded — there the staircase's higher steps dominate
/// and the single-step model no longer applies.
pub fn fit_min_depth(points: &[DepthPoint]) -> Option<MinDepthFit> {
    let usable: Vec<DepthPoint> = points
        .iter()
        .copied()
        .filter(|p| p.a > 0.0 && p.events_per_half_cycle.is_finite() && p.events_per_half_cycle <= 1.5)
        .collect();
    if usable.len() < 3 {
        return None;
    }
    let has_low = usable.iter().any(|p| p.events_per_half_cycle < 0.4);
    let has_high = usable.iter().any(|p| p.events_per_half_cycle > 0.6);
    if !has_low || !has_high {
        return None;
    }

    let ln_min = usable.iter().map(|p| p.a.ln()).fold(f64::INFINITY, f64::min);
    let ln_max = usable
        .iter()
        .map(|p| p.a.ln())
        .fold(f64::NEG_INFINITY, f64::max);

    let sse = |mu: f64, sigma: f64| -> f64 {
        usable
            .iter()
            .map(|p| {
                let model = standard_normal_cdf((p.a.ln() - mu) / sigma);
                let d = p.events_per_half_cycle.min(1.0) - model;
                d * d
            })
            .sum()
    };

    let mut best = (f64::INFINITY, ln_min, 0.1);
    let mu_steps = 200;
    for i in 0..=mu_steps {
        let mu = ln_min + (ln_max - ln_min) * i as f64 / mu_steps as f64;
        for j in 0..40 {
            let sigma = 0.005 * 1.2_f64.powi(j); // 0.005 .. ~7 in ln a
            let value = sse(mu, sigma);
            if value < best.0 {
                best = (value, mu, sigma);
            }
        }
    }
    let (sse_min, mu_hat, sigma_hat) = best;
    let dof = usable.len().saturating_sub(2).max(1) as f64;
    let threshold = sse_min * (1.0 + 2.0 / dof) + 1e-12;

    // Profile over mu: the interval where some sigma keeps SSE under the
    // threshold.
    let mut low = mu_hat;
    let mut high = mu_hat;
    for i in 0..=mu_steps {
        let mu = ln_min + (ln_max - ln_min) * i as f64 / mu_steps as f64;
        let feasible = (0..40).any(|j| {
            let sigma = 0.005 * 1.2_f64.powi(j);
            sse(mu, sigma) <= threshold
        });
        if feasible {
            low = low.min(mu);
            high = high.max(mu);
        }
    }

    Some(MinDepthFit {
        a_min: mu_hat.exp(),
        a_min_low: low.exp(),
        a_min_high: high.exp(),
        sigma_ln_a: sigma_hat,
        points_used: usable.len(),
    })
}

// ---------------------------------------------------------------------------
// Hot-pixel mask (background is heavy-tailed; mask the tail, use the body)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HotPixelMask {
    width: u16,
    masked: Vec<bool>,
}

impl HotPixelMask {
    /// Builds the mask from per-pixel counts of an *unmodulated* reference
    /// window: pixels above `median + 5·MAD` (and above a small absolute
    /// floor) are masked. The mask is fixed-pattern and belongs in the run
    /// metadata, not just preprocessing.
    pub fn from_reference_counts(width: u16, _height: u16, counts: &[u32]) -> Self {
        let mut sorted: Vec<u32> = counts.to_vec();
        sorted.sort_unstable();
        let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0) as f64;
        let mut deviations: Vec<f64> = counts
            .iter()
            .map(|&count| (f64::from(count) - median).abs())
            .collect();
        deviations.sort_by(f64::total_cmp);
        let mad = deviations.get(deviations.len() / 2).copied().unwrap_or(0.0);
        let threshold = median + 5.0 * mad.max(0.5) + 2.0;
        let masked = counts
            .iter()
            .map(|&count| f64::from(count) > threshold)
            .collect();
        Self { width, masked }
    }

    pub fn is_masked(&self, x: u16, y: u16) -> bool {
        self.masked
            .get(y as usize * self.width as usize + x as usize)
            .copied()
            .unwrap_or(false)
    }

    pub fn masked_count(&self) -> usize {
        self.masked.iter().filter(|&&m| m).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-uniform stream (splitmix64 → [0,1)).
    struct UniformStream {
        state: u64,
    }

    impl UniformStream {
        fn new(seed: u64) -> Self {
            Self { state: seed }
        }

        fn next(&mut self) -> f64 {
            self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z = z ^ (z >> 31);
            (z >> 11) as f64 / (1_u64 << 53) as f64
        }

        fn take(&mut self, n: usize) -> Vec<f64> {
            (0..n).map(|_| self.next()).collect()
        }
    }

    fn uniform_sequence(seed: u64, n: usize) -> Vec<f64> {
        UniformStream::new(seed).take(n)
    }

    /// Synthetic event stream: `per_cycle` phase-locked events per cycle at
    /// `locked_phase` (jitter ±0.02) plus `background_rate_hz` uniform noise.
    fn synthetic_events(
        frequency_hz: f64,
        duration_s: f64,
        per_cycle: f64,
        background_rate_hz: f64,
        seed: u64,
    ) -> Vec<u64> {
        let cycles = (frequency_hz * duration_s) as usize;
        let period_us = 1.0e6 / frequency_hz;
        let mut stream = UniformStream::new(seed);
        let mut next = move || stream.next();
        let mut events = Vec::new();
        for cycle in 0..cycles {
            let base = cycle as f64 * period_us;
            // Bernoulli(per_cycle fractional part) + floor.
            let mut count = per_cycle.floor() as usize;
            if next() < per_cycle.fract() {
                count += 1;
            }
            for _ in 0..count {
                let phase = 0.25 + (next() - 0.5) * 0.04;
                events.push((base + phase * period_us) as u64);
            }
        }
        let n_background = (background_rate_hz * duration_s) as usize;
        for _ in 0..n_background {
            events.push((next() * duration_s * 1.0e6) as u64);
        }
        events.sort_unstable();
        events
    }

    #[test]
    fn rayleigh_accepts_uniform_and_rejects_locked_phases() {
        let uniform = uniform_sequence(7, 2_000);
        let stat = rayleigh_test(&uniform);
        assert!(stat.p_value > 0.01, "uniform phases: p={}", stat.p_value);

        let locked: Vec<f64> = uniform_sequence(11, 200)
            .into_iter()
            .map(|u| 0.3 + 0.02 * (u - 0.5))
            .collect();
        let stat = rayleigh_test(&locked);
        assert!(stat.p_value < 1e-12, "locked phases: p={}", stat.p_value);
    }

    #[test]
    fn detection_discounts_uniform_background() {
        // 0.8 locked events/cycle at 1 kHz for 0.5 s, drowned in 10x
        // background rate: still detected via phase.
        let events = synthetic_events(1_000.0, 0.5, 0.8, 8_000.0, 3);
        let phases = fold_phases(events.iter().copied(), 0, 1_000.0);
        let stat = rayleigh_test(&phases);
        assert!(stat.p_value < 1e-6, "p={}", stat.p_value);

        // Background alone must NOT detect.
        let noise_only = synthetic_events(1_000.0, 0.5, 0.0, 8_000.0, 5);
        let phases = fold_phases(noise_only.iter().copied(), 0, 1_000.0);
        let stat = rayleigh_test(&phases);
        assert!(stat.p_value > 1e-3, "background-only p={}", stat.p_value);
    }

    #[test]
    fn phase_locked_excess_recovers_events_per_cycle() {
        let frequency = 2_000.0;
        let duration = 0.5;
        let per_cycle = 0.6;
        let events = synthetic_events(frequency, duration, per_cycle, 2_000.0, 9);
        let phases = fold_phases(events.iter().copied(), 0, frequency);
        let histogram = phase_histogram(&phases, 32);
        let cycles = frequency * duration;
        let recovered = phase_locked_excess(&histogram) / cycles;
        assert!(
            (recovered - per_cycle).abs() < 0.12,
            "recovered {recovered} vs {per_cycle}"
        );
    }

    #[test]
    fn frequency_refinement_recovers_clock_skew() {
        // Commanded 5 kHz, true (camera-clock) frequency 300 ppm higher —
        // the naive fold dephases by 1.5 cycles over the 1 s span and
        // collapses, while the refined lock recovers the true frequency.
        let true_hz = 5_000.0 * (1.0 + 300e-6);
        let events = synthetic_events(true_hz, 1.0, 1.0, 500.0, 13);
        let lock = refine_frequency(&events, 5_000.0, 500.0).expect("lock found");
        let recovered_ppm = (lock.frequency_hz / 5_000.0 - 1.0) * 1e6;
        // The scan step is 0.1/span = 0.1 Hz = 20 ppm at 5 kHz.
        assert!(
            (recovered_ppm - 300.0).abs() < 25.0,
            "recovered {recovered_ppm} ppm"
        );
        let naive = rayleigh_test(&fold_phases(events.iter().copied(), events[0], 5_000.0));
        assert!(
            lock.rayleigh.z > naive.z * 5.0,
            "lock z={} naive z={}",
            lock.rayleigh.z,
            naive.z
        );
    }

    #[test]
    fn fiducial_folding_matches_known_phase() {
        let cycle_starts: Vec<u64> = (0..100).map(|k| k * 1_000).collect();
        let events: Vec<u64> = (0..99).map(|k| k * 1_000 + 250).collect();
        let phases = fold_phases_with_fiducials(&events, &cycle_starts);
        assert_eq!(phases.len(), 99);
        assert!(phases.iter().all(|p| (p - 0.25).abs() < 1e-9));
    }

    #[test]
    fn min_depth_fit_recovers_the_crossing() {
        // True a_min = 0.20, smear sigma = 0.15 in ln a.
        let mu = 0.2_f64.ln();
        let points: Vec<DepthPoint> = (0..12)
            .map(|i| {
                let a = 0.08 * 1.25_f64.powi(i); // 0.08 .. ~0.9
                DepthPoint {
                    a,
                    events_per_half_cycle: standard_normal_cdf((a.ln() - mu) / 0.15),
                }
            })
            .collect();
        let fit = fit_min_depth(&points).expect("fit succeeds");
        assert!(
            (fit.a_min - 0.2).abs() < 0.02,
            "a_min={} (expected 0.20)",
            fit.a_min
        );
        assert!(fit.a_min_low <= fit.a_min && fit.a_min <= fit.a_min_high);
        assert!((fit.sigma_ln_a - 0.15).abs() < 0.08);
    }

    #[test]
    fn min_depth_fit_requires_a_bracketed_transition() {
        // All points fully above threshold: no crossing to fit.
        let points: Vec<DepthPoint> = (0..6)
            .map(|i| DepthPoint {
                a: 0.5 + 0.1 * i as f64,
                events_per_half_cycle: 1.0,
            })
            .collect();
        assert!(fit_min_depth(&points).is_none());
    }

    #[test]
    fn hot_pixel_mask_flags_the_tail_only() {
        let mut counts = vec![2_u32; 64 * 64];
        counts[5] = 500; // hot
        counts[700] = 300; // hot
        let mask = HotPixelMask::from_reference_counts(64, 64, &counts);
        assert_eq!(mask.masked_count(), 2);
        assert!(mask.is_masked(5, 0));
        assert!(!mask.is_masked(6, 0));
    }
}
