//! Optical-target DAC warp-table synthesis for the Pockels/PBS modulator.
//!
//! On one monotonic Pockels/PBS lobe the excitation transfer is
//! `I(V) = I_floor + (I_ceil - I_floor) · sin²(α (V - V_null))`, with
//! `α = π / (2 Vπ)`. The manufacturer likewise describes the amplitude
//! modulator as `sin²`; a 50 % bias only *approximately* linearises the small
//! signal. A pure DAC sine therefore does **not** produce a sinusoidal optical
//! target — it must be pre-warped by inverting the transfer:
//!
//! ```text
//! u(t) = (I_d(t) - I_floor) / (I_ceil - I_floor)          // normalised target
//! V(u) = V_null + (2 Vπ / π) · arcsin(√u)                 // increasing lobe
//! ```
//!
//! Two optical targets are supported (the drive picks one):
//! - [`OpticalTarget::LogSine`] — `ln I_d = ln I_g + (a/2) sin ωt`, the clean A1
//!   input because the event camera responds to changes in `ln I`.
//! - [`OpticalTarget::LinearSine`] — `I_d = I_c (1 + m sin ωt)`, `m = tanh(a/2)`.
//!
//! The lobe is configured as the two **DAC codes an operator can observe** —
//! where the light is dimmest (`V_null`) and where it is brightest (`V_peak`) —
//! and `Vπ` is derived from the pair by [`LobeInversion::resolve`]. The engineer
//! should not rely on nominal `Vπ` but sweep settled constant DAC codes, measure
//! the actual optical transfer, and freeze those two codes. A fully measured
//! lookup table can replace this analytic inversion later behind the same
//! interface.

use std::f64::consts::PI;

/// Warp-table length played back over one modulation period.
pub const WARP_TABLE_LEN: usize = 256;
/// Full-scale DAC code (12-bit).
pub const DAC_FULL_SCALE: u16 = 4_095;

/// Shallowest optical depth the UI offers. Below this the warp table is
/// indistinguishable from a constant drive.
pub const DEPTH_A_MIN: f64 = 0.01;
/// Deepest optical depth the UI offers, before the lobe is consulted.
pub const DEPTH_A_MAX: f64 = 6.0;
/// Dimmest cycle-mean lobe point the UI offers.
pub const MEAN_U_MIN: f64 = 0.01;

/// Modified Bessel function `I₀(x)` for the Stage-A depth range (`|x| ≤ 3`).
///
/// The positive power series converges rapidly here and avoids adding a
/// special-functions dependency to the plugin/firmware parameter path.
fn modified_bessel_i0(x: f64) -> f64 {
    let y = 0.25 * x * x;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..=32 {
        term *= y / (k as f64 * k as f64);
        sum += term;
        if term <= f64::EPSILON * sum {
            break;
        }
    }
    sum
}

/// Geometric pedestal that makes a log-sine's cycle-mean normalized lobe
/// coordinate equal `mean_u`:
///
/// `u(t) = u_g exp[(a/2) sin(ωt)]`, `u_g = mean_u / I₀(a/2)`.
pub fn log_sine_geometric_pedestal(mean_u: f64, depth_a: f64) -> f64 {
    mean_u / modified_bessel_i0(0.5 * depth_a)
}

pub fn log_sine_cycle_mean(pedestal_u: f64, depth_a: f64) -> f64 {
    pedestal_u * modified_bessel_i0(0.5 * depth_a)
}

/// Optical intensity target the drive should reproduce, swung around the
/// dimensionless lobe point `u`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpticalTarget {
    /// Recommended A1 log-intensity sine in normalized, floor-subtracted lobe
    /// coordinate: `ln u = ln u_g + (a/2) sin ωt`.
    LogSine,
    /// Literal linear-intensity sine: `u = u_c (1 + m sin ωt)`,
    /// `m = tanh(a/2)`.
    LinearSine,
}

/// Frozen inversion of one monotonic Pockels/PBS lobe, in DAC codes.
///
/// Built from the two codes an operator can actually observe on the bench via
/// [`LobeInversion::resolve`], never from a typed-in distance — see the error
/// type for why.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LobeInversion {
    /// DAC code where the excitation light is at its minimum (`sin² = 0`).
    pub v_null_dac: f64,
    /// DAC-code half-wave-voltage span from `v_null` to the excitation maximum.
    pub v_pi_dac: f64,
}

/// One monotonic lobe resolved from a measured `(min, max)` pair of codes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLobe {
    pub inversion: LobeInversion,
    /// The observed pair ran *downward* in code, so the drive uses the
    /// equivalent ascending branch — the one that rises into the very maximum
    /// that was measured. Worth reporting: the codes driven are not the ones
    /// the operator typed.
    pub folded: bool,
}

/// Why two observed codes do not name a drivable lobe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LobeError {
    /// The two codes coincide: no measurable lobe, so nothing to invert.
    Degenerate { code: f64 },
    /// Neither the observed branch nor its ascending equivalent fits inside
    /// `0..=max_code`.
    Unreachable {
        v_null: f64,
        v_peak: f64,
        max_code: f64,
    },
}

impl std::fmt::Display for LobeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Degenerate { code } => write!(
                f,
                "V_null and V_peak are both {code:.0}: sweep the DAC and read off the codes where \
                 the light is dimmest and brightest"
            ),
            Self::Unreachable {
                v_null,
                v_peak,
                max_code,
            } => write!(
                f,
                "no monotonic lobe between V_null {v_null:.0} and V_peak {v_peak:.0} fits inside \
                 0..={max_code:.0}; raise the max limit or pick a lobe further down the range"
            ),
        }
    }
}

impl std::error::Error for LobeError {}

impl LobeInversion {
    /// Resolves the two codes an operator can *observe* — the DAC code at the
    /// excitation minimum and the one at the excitation maximum — into the
    /// ascending lobe the drive inverts.
    ///
    /// Both inputs are absolute codes, deliberately. The earlier form paired an
    /// absolute `V_null` with `Vπ` as a *distance* from it, and a distance is
    /// not what an operator reads off a sweep: entering the brightest **code**
    /// as `Vπ` doubles the half wave whenever the null sits near half the peak
    /// code, which puts maximum light at `u ≈ 0.5` and a null back at
    /// `u = 1`. Two observed codes cannot be mixed up that way, and they make
    /// `u = 1` land exactly on the measured maximum by construction.
    pub fn resolve(v_null: f64, v_peak: f64, max_code: f64) -> Result<ResolvedLobe, LobeError> {
        if !v_null.is_finite() || !v_peak.is_finite() {
            return Err(LobeError::Degenerate { code: v_null });
        }
        let span = v_peak - v_null;
        // Sub-code separation is meaningless on a 12-bit DAC.
        if span.abs() < 1.0 {
            return Err(LobeError::Degenerate { code: v_null });
        }
        let v_pi_dac = span.abs();
        let fits = |null: f64| null >= -0.5 && null + v_pi_dac <= max_code + 0.5;
        // `sin²` repeats every `2Vπ` and every branch is a mirror of its
        // neighbour, so a pair measured running downward in code names the same
        // physical lobe as the ascending branch one full period below — which
        // ends on the maximum that was actually measured. Prefer that one; fall
        // back to the branch rising out of the observed null only if it is what
        // fits inside the commandable range.
        let (v_null_dac, folded) = if span > 0.0 && fits(v_null) {
            (v_null, false)
        } else if fits(v_peak - v_pi_dac) {
            (v_peak - v_pi_dac, true)
        } else if fits(v_null) {
            (v_null, true)
        } else {
            return Err(LobeError::Unreachable {
                v_null,
                v_peak,
                max_code,
            });
        };
        Ok(ResolvedLobe {
            inversion: Self {
                v_null_dac: v_null_dac.clamp(0.0, (max_code - v_pi_dac).max(0.0)),
                v_pi_dac,
            },
            folded,
        })
    }

    /// DAC code at the excitation maximum: where normalized lobe coordinate
    /// `u = 1` lands.
    pub fn v_peak_dac(&self) -> f64 {
        self.v_null_dac + self.v_pi_dac
    }

    /// Normalised optical intensity produced by `code` on the configured lobe:
    /// `u = sin²(π(code - V_null) / (2 Vπ))`.
    pub fn u_for_dac(&self, code: f64) -> f64 {
        let alpha = PI / (2.0 * self.v_pi_dac);
        (alpha * (code - self.v_null_dac)).sin().powi(2)
    }

    /// DAC code producing normalised optical intensity `u ∈ [0, 1]` on the
    /// increasing lobe.
    pub fn dac_for_u(&self, u: f64) -> f64 {
        self.v_null_dac + (2.0 * self.v_pi_dac / PI) * u.clamp(0.0, 1.0).sqrt().asin()
    }

    /// Highest normalised intensity a drive may peak at without exceeding the
    /// operator's DAC ceiling `max_code`.
    ///
    /// `u = 1` sits at `v_peak`; a ceiling below that clips the lobe short, and
    /// the drive has to stay under whatever `u` the ceiling code produces.
    pub fn peak_intensity_ceiling(&self, max_code: f64) -> f64 {
        if max_code >= self.v_peak_dac() {
            return 1.0;
        }
        if max_code <= self.v_null_dac {
            return 0.0;
        }
        self.u_for_dac(max_code)
    }
}

/// How the peak normalised intensity of a drive follows from its requested
/// cycle mean `ū` and depth `a`.
///
/// Every calibrated mode has one of these, and they are the *only* thing that
/// limits `a` and `ū`: the swing has to stay under the top of the lobe (and
/// under the operator's DAC ceiling, expressed as the same `u_max`). Solving
/// one relation for each variable in turn gives the achievable ranges the UI
/// shows — and clamps against, instead of refusing the edit and snapping the
/// control back, which told the operator nothing about where the boundary was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeakLaw {
    /// A constant hold modulates nothing, so the peak *is* the mean and `a`
    /// does not enter.
    Constant,
    /// Bare DAC sine/square on a calibrated band: `ū · e^{a/2}`.
    LogSwing,
    /// [`OpticalTarget::LogSine`], whose pedestal preserves the cycle mean:
    /// `ū · e^{a/2} / I₀(a/2)`.
    LogSine,
    /// [`OpticalTarget::LinearSine`]: `ū · (1 + tanh(a/2))`.
    LinearSine,
}

impl PeakLaw {
    pub fn of(target: OpticalTarget) -> Self {
        match target {
            OpticalTarget::LogSine => Self::LogSine,
            OpticalTarget::LinearSine => Self::LinearSine,
        }
    }

    /// Peak normalised intensity of the drive, in lobe coordinate.
    pub fn peak(self, mean_u: f64, depth_a: f64) -> f64 {
        let depth_a = depth_a.max(0.0);
        mean_u * self.swing(depth_a)
    }

    /// Factor the peak sits above the requested cycle mean. Monotonically
    /// non-decreasing in `a` in every variant, which is what makes the
    /// inversions below well defined.
    fn swing(self, depth_a: f64) -> f64 {
        match self {
            Self::Constant => 1.0,
            Self::LogSwing => (0.5 * depth_a).exp(),
            Self::LogSine => (0.5 * depth_a).exp() / modified_bessel_i0(0.5 * depth_a),
            Self::LinearSine => 1.0 + (0.5 * depth_a).tanh(),
        }
    }

    /// Deepest `a` expressible at this cycle mean under the ceiling `u_max`.
    pub fn max_depth_for_mean(self, mean_u: f64, u_max: f64) -> f64 {
        // Written through `partial_cmp` so a NaN is rejected rather than
        // silently passing a negated comparison.
        let usable = |value: f64| value.partial_cmp(&0.0) == Some(std::cmp::Ordering::Greater);
        if !usable(mean_u) || !usable(u_max) || mean_u > u_max {
            return 0.0;
        }
        let headroom = u_max / mean_u;
        match self {
            // Nothing swings, so the UI limit is the only bound.
            Self::Constant => DEPTH_A_MAX,
            Self::LogSwing => (2.0 * headroom.ln()).clamp(0.0, DEPTH_A_MAX),
            // Below twice the mean the swing never reaches the ceiling.
            Self::LinearSine => {
                let m = headroom - 1.0;
                if m >= 1.0 {
                    DEPTH_A_MAX
                } else {
                    (2.0 * m.atanh()).clamp(0.0, DEPTH_A_MAX)
                }
            }
            // No closed form (I₀ grows like e^x/√(2πx)), but `swing` is
            // monotonic, so bisect it.
            Self::LogSine => {
                if self.swing(DEPTH_A_MAX) <= headroom {
                    return DEPTH_A_MAX;
                }
                let (mut lo, mut hi) = (0.0_f64, DEPTH_A_MAX);
                for _ in 0..64 {
                    let mid = 0.5 * (lo + hi);
                    if self.swing(mid) <= headroom {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                lo
            }
        }
    }

    /// Brightest cycle mean the requested depth leaves room for, under the same
    /// ceiling. The counterpart of [`PeakLaw::max_depth_for_mean`].
    pub fn max_mean_for_depth(self, depth_a: f64, u_max: f64) -> f64 {
        if u_max.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
            return 0.0;
        }
        (u_max / self.swing(depth_a.max(0.0))).clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpticalDrive {
    pub target: OpticalTarget,
    /// Optical log-modulation depth `a = ln(I_max / I_min)`, must be positive.
    pub depth_a: f64,
    /// Dimensionless, floor-subtracted lobe point in `(0, 1]`. It is the
    /// geometric pedestal `u_g` for [`OpticalTarget::LogSine`] and the
    /// arithmetic centre `u_c` for [`OpticalTarget::LinearSine`]. This is not
    /// the physical A1 flux point `I_k`.
    pub operating_point: f64,
    pub inversion: LobeInversion,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WarpError {
    /// `a` is not finite or not positive.
    InvalidDepth,
    /// The operating point is not in `(0, 1]`.
    InvalidOperatingPoint,
    /// `Vπ` is not finite or not positive.
    InvalidInversion,
    /// The peak optical target exceeds the lobe ceiling: the internal
    /// pedestal/centre is too bright for this depth and would saturate.
    Saturates { peak: f64 },
    /// A computed DAC code falls outside `0..=4095`: the inversion parameters do
    /// not fit the requested depth on this lobe. Clamping would silently distort
    /// the optical target, so the drive is refused instead.
    OutOfRange { index: usize, code: f64 },
}

impl std::fmt::Display for WarpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDepth => f.write_str("optical depth a must be finite and positive"),
            Self::InvalidOperatingPoint => f.write_str("operating point must be in (0, 1]"),
            Self::InvalidInversion => f.write_str("Vπ must be finite and positive"),
            Self::Saturates { peak } => write!(
                f,
                "peak optical target u = {peak:.3} exceeds the lobe ceiling; lower a or the operating point"
            ),
            Self::OutOfRange { index, code } => write!(
                f,
                "warp sample {index} = {code:.1} DAC leaves 0..=4095; reduce a or re-measure the lobe"
            ),
        }
    }
}

impl std::error::Error for WarpError {}

impl OpticalDrive {
    /// Derives the target-law parameters that make an optical waveform span
    /// the intensities produced by the supplied DAC band.
    pub fn from_dac_band(
        target: OpticalTarget,
        inversion: LobeInversion,
        lo: f64,
        hi: f64,
    ) -> Self {
        let u_lo = inversion.u_for_dac(lo);
        let u_hi = inversion.u_for_dac(hi);
        let (operating_point, depth_a) = match target {
            OpticalTarget::LogSine => ((u_lo * u_hi).sqrt(), (u_hi / u_lo).ln()),
            OpticalTarget::LinearSine => {
                let operating_point = 0.5 * (u_lo + u_hi);
                let modulation = (u_hi - u_lo) / (u_hi + u_lo);
                (operating_point, 2.0 * modulation.atanh())
            }
        };
        Self {
            target,
            depth_a,
            operating_point,
            inversion,
        }
    }

    /// Normalised optical target `u(φ)` for phase fraction `φ ∈ [0, 1)`, swung
    /// around the internal `u_g`/`u_c` point (not peak-normalised).
    pub fn normalised_intensity(&self, phase: f64) -> f64 {
        let sine = (2.0 * PI * phase).sin();
        match self.target {
            // ln u = ln u_g + (a/2) sin ωt.
            OpticalTarget::LogSine => self.operating_point * (0.5 * self.depth_a * sine).exp(),
            // u = u_c (1 + m sin ωt), m = tanh(a/2).
            OpticalTarget::LinearSine => {
                let m = (0.5 * self.depth_a).tanh();
                self.operating_point * (1.0 + m * sine)
            }
        }
    }

    /// Peak normalised optical target over one period.
    fn peak_intensity(&self) -> f64 {
        match self.target {
            OpticalTarget::LogSine => self.operating_point * (0.5 * self.depth_a).exp(),
            OpticalTarget::LinearSine => self.operating_point * (1.0 + (0.5 * self.depth_a).tanh()),
        }
    }

    /// Builds the `WARP_TABLE_LEN`-entry DAC warp table for one period.
    pub fn warp_table(&self) -> Result<Vec<u16>, WarpError> {
        if !self.depth_a.is_finite() || self.depth_a <= 0.0 {
            return Err(WarpError::InvalidDepth);
        }
        if !self.operating_point.is_finite()
            || !(0.0..=1.0).contains(&self.operating_point)
            || self.operating_point <= 0.0
        {
            return Err(WarpError::InvalidOperatingPoint);
        }
        if !self.inversion.v_pi_dac.is_finite() || self.inversion.v_pi_dac <= 0.0 {
            return Err(WarpError::InvalidInversion);
        }
        let peak = self.peak_intensity();
        if peak > 1.0 + 1e-9 {
            return Err(WarpError::Saturates { peak });
        }
        let mut table = Vec::with_capacity(WARP_TABLE_LEN);
        for index in 0..WARP_TABLE_LEN {
            let phase = index as f64 / WARP_TABLE_LEN as f64;
            let code = self.inversion.dac_for_u(self.normalised_intensity(phase));
            if !code.is_finite() || code < -0.5 || code > f64::from(DAC_FULL_SCALE) + 0.5 {
                return Err(WarpError::OutOfRange { index, code });
            }
            table.push(code.round().clamp(0.0, f64::from(DAC_FULL_SCALE)) as u16);
        }
        Ok(table)
    }
}

/// Forward Pockels/PBS transfer used to verify a warp table reproduces the
/// intended optical target: `u = sin²(α (code - V_null))`, `α = π / (2 Vπ)`.
#[cfg(test)]
pub fn lobe_transmission(code: f64, inversion: &LobeInversion) -> f64 {
    inversion.u_for_dac(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inversion() -> LobeInversion {
        // Null at code 200, one half-wave-voltage span later at peak light.
        LobeInversion {
            v_null_dac: 200.0,
            v_pi_dac: 1_600.0,
        }
    }

    /// Peak-normalised operating point (max light at the lobe ceiling) so the
    /// range/round-trip assertions exercise the full swing.
    fn peak_operating_point(target: OpticalTarget, depth_a: f64) -> f64 {
        match target {
            OpticalTarget::LogSine => (-0.5 * depth_a).exp(),
            OpticalTarget::LinearSine => 1.0 / (1.0 + (0.5 * depth_a).tanh()),
        }
    }

    fn drive(target: OpticalTarget, depth_a: f64) -> OpticalDrive {
        OpticalDrive {
            target,
            depth_a,
            operating_point: peak_operating_point(target, depth_a),
            inversion: inversion(),
        }
    }

    #[test]
    fn two_observed_codes_put_the_light_maximum_at_u_one() {
        // The property the endpoint form exists to guarantee: whatever pair of
        // codes was measured, u = 1 lands on the measured maximum, u = 0 on
        // the measured minimum, and nothing turns over in between.
        for (null, peak) in [(200.0, 1_800.0), (0.0, 4_095.0), (1_600.0, 3_200.0)] {
            let lobe = LobeInversion::resolve(null, peak, 4_095.0).expect("a real lobe");
            assert!(!lobe.folded);
            let inversion = lobe.inversion;
            assert!((inversion.dac_for_u(1.0) - peak).abs() < 1e-9);
            assert!((inversion.dac_for_u(0.0) - null).abs() < 1e-9);
            assert!((inversion.u_for_dac(peak) - 1.0).abs() < 1e-9);
            let mut previous = f64::MIN;
            for step in 0..=100 {
                let u = f64::from(step) / 100.0;
                let light = inversion.u_for_dac(inversion.dac_for_u(u));
                assert!(light >= previous - 1e-9, "light turned over at u = {u}");
                previous = light;
            }
        }
    }

    #[test]
    fn the_brightest_code_typed_as_v_pi_is_what_used_to_peak_at_half() {
        // Regression witness for the bench report of 2026-07-28. With the null
        // at half the brightest code, feeding the *absolute* brightest code in
        // as the half-wave-voltage distance peaks the light at u = 0.5 and
        // returns it to the null at u = 1 — exactly what was observed.
        let (null, peak) = (1_600.0, 3_200.0);
        let truth = LobeInversion::resolve(null, peak, 4_095.0)
            .expect("a real lobe")
            .inversion;
        let mistake = LobeInversion {
            v_null_dac: null,
            v_pi_dac: peak, // the distance field filled with a code
        };
        let light = |u: f64| truth.u_for_dac(mistake.dac_for_u(u));
        assert!(light(0.5) > 0.99, "peak light at u = 0.5: {}", light(0.5));
        assert!(light(1.0) < 0.01, "null light at u = 1: {}", light(1.0));
        // And the endpoint form is immune to the same typo, because there is no
        // distance to type: the brightest code *is* the field.
        assert!((truth.u_for_dac(truth.dac_for_u(1.0)) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_pair_measured_downward_folds_onto_the_branch_into_the_same_peak() {
        // Peak below null: the same physical lobe, approached from below. The
        // ascending equivalent must end on the measured maximum.
        let lobe = LobeInversion::resolve(3_000.0, 2_000.0, 4_095.0).expect("a real lobe");
        assert!(lobe.folded);
        assert_eq!(lobe.inversion.v_pi_dac, 1_000.0);
        assert!((lobe.inversion.v_peak_dac() - 2_000.0).abs() < 1e-9);
        assert!(lobe.inversion.v_null_dac >= 0.0);
    }

    #[test]
    fn a_downward_pair_with_no_room_below_rises_out_of_the_observed_null() {
        // 500 → 100 would fold to a null at −300; the branch above the observed
        // null is the one that fits.
        let lobe = LobeInversion::resolve(500.0, 100.0, 4_095.0).expect("a real lobe");
        assert!(lobe.folded);
        assert_eq!(lobe.inversion.v_null_dac, 500.0);
        assert_eq!(lobe.inversion.v_peak_dac(), 900.0);
    }

    #[test]
    fn refuses_a_degenerate_or_unreachable_pair() {
        assert!(matches!(
            LobeInversion::resolve(1_000.0, 1_000.0, 4_095.0),
            Err(LobeError::Degenerate { .. })
        ));
        // A lobe wider than the commandable range fits nowhere.
        assert!(matches!(
            LobeInversion::resolve(0.0, 3_000.0, 2_000.0),
            Err(LobeError::Unreachable { .. })
        ));
    }

    #[test]
    fn tables_stay_inside_the_dac_range_for_both_targets() {
        for target in [OpticalTarget::LogSine, OpticalTarget::LinearSine] {
            let table = drive(target, 1.0).warp_table().expect("in range");
            assert_eq!(table.len(), WARP_TABLE_LEN);
            assert!(table.iter().all(|&code| code <= DAC_FULL_SCALE));
        }
    }

    #[test]
    fn warp_table_reproduces_the_optical_target_through_the_sin2_transfer() {
        // Feeding the warp codes back through the sin² lobe must recover the
        // intended normalised intensity: that is the whole point of the warp.
        for target in [OpticalTarget::LogSine, OpticalTarget::LinearSine] {
            let drive = drive(target, 0.8);
            let table = drive.warp_table().expect("in range");
            for (index, &code) in table.iter().enumerate() {
                let phase = index as f64 / WARP_TABLE_LEN as f64;
                let recovered = lobe_transmission(f64::from(code), &drive.inversion);
                let target_u = drive.normalised_intensity(phase);
                assert!(
                    (recovered - target_u).abs() < 5e-3,
                    "{target:?} phase {phase}: recovered {recovered} vs target {target_u}"
                );
            }
        }
    }

    #[test]
    fn measured_log_contrast_matches_the_requested_depth_for_log_sine() {
        // The optical min/max of a log-sine table give back a = ln(max/min).
        let drive = drive(OpticalTarget::LogSine, 1.2);
        let table = drive.warp_table().expect("in range");
        let intensities: Vec<f64> = table
            .iter()
            .map(|&code| lobe_transmission(f64::from(code), &drive.inversion))
            .collect();
        let max = intensities.iter().cloned().fold(f64::MIN, f64::max);
        let min = intensities.iter().cloned().fold(f64::MAX, f64::min);
        let measured_a = (max / min).ln();
        assert!((measured_a - 1.2).abs() < 0.05, "measured a = {measured_a}");
    }

    #[test]
    fn drive_derived_from_a_dac_band_recovers_that_optical_span() {
        let inversion = inversion();
        let lo = 600.0;
        let hi = 1_500.0;
        let expected_lo = inversion.u_for_dac(lo);
        let expected_hi = inversion.u_for_dac(hi);

        for target in [OpticalTarget::LogSine, OpticalTarget::LinearSine] {
            let drive = OpticalDrive::from_dac_band(target, inversion, lo, hi);
            let table = drive.warp_table().expect("manual band is valid");
            let recovered: Vec<f64> = table
                .iter()
                .map(|&code| inversion.u_for_dac(f64::from(code)))
                .collect();
            let recovered_lo = recovered.iter().copied().fold(f64::MAX, f64::min);
            let recovered_hi = recovered.iter().copied().fold(f64::MIN, f64::max);
            assert!(
                (recovered_lo - expected_lo).abs() < 5e-3,
                "{target:?}: recovered lower intensity {recovered_lo} vs {expected_lo}"
            );
            assert!(
                (recovered_hi - expected_hi).abs() < 5e-3,
                "{target:?}: recovered upper intensity {recovered_hi} vs {expected_hi}"
            );
        }
    }

    #[test]
    fn deeper_depth_gives_more_optical_contrast() {
        let contrast = |a: f64| {
            let drive = drive(OpticalTarget::LinearSine, a);
            let table = drive.warp_table().expect("in range");
            let intensities: Vec<f64> = table
                .iter()
                .map(|&code| lobe_transmission(f64::from(code), &drive.inversion))
                .collect();
            let max = intensities.iter().cloned().fold(f64::MIN, f64::max);
            let min = intensities.iter().cloned().fold(f64::MAX, f64::min);
            (max / min).ln()
        };
        assert!(contrast(1.0) > contrast(0.5));
    }

    #[test]
    fn rejects_invalid_depth_and_inversion() {
        assert_eq!(
            drive(OpticalTarget::LogSine, 0.0).warp_table(),
            Err(WarpError::InvalidDepth)
        );
        let mut bad = drive(OpticalTarget::LogSine, 1.0);
        bad.inversion.v_pi_dac = 0.0;
        assert_eq!(bad.warp_table(), Err(WarpError::InvalidInversion));
    }

    #[test]
    fn refuses_an_inversion_that_overruns_the_lobe() {
        // The reachable optical maximum sits at v_null + Vπ; pushing that past
        // the top rail must be refused rather than silently clamped.
        let drive = OpticalDrive {
            target: OpticalTarget::LogSine,
            depth_a: 1.0,
            operating_point: peak_operating_point(OpticalTarget::LogSine, 1.0),
            inversion: LobeInversion {
                v_null_dac: 200.0,
                v_pi_dac: 4_000.0, // peak light would land at code 4200
            },
        };
        assert!(matches!(
            drive.warp_table(),
            Err(WarpError::OutOfRange { .. })
        ));
    }

    #[test]
    fn refuses_an_operating_point_too_bright_for_the_depth() {
        let drive = OpticalDrive {
            target: OpticalTarget::LogSine,
            depth_a: 1.0,
            operating_point: 0.9, // 0.9 * exp(0.5) = 1.48 > 1 -> saturates
            inversion: inversion(),
        };
        assert!(matches!(
            drive.warp_table(),
            Err(WarpError::Saturates { .. })
        ));
    }

    #[test]
    fn fixed_internal_log_pedestal_stays_at_phase_zero_while_sweeping_a() {
        // The low-level OpticalDrive takes the geometric pedestal u_g. At
        // phase 0 (sin = 0), that pedestal stays put while contrast grows.
        // The plugin wrapper adjusts u_g with I₀(a/2) when its requested
        // cycle-mean ū is held fixed.
        let u_g = 0.3;
        let drive = |a: f64| OpticalDrive {
            target: OpticalTarget::LogSine,
            depth_a: a,
            operating_point: u_g,
            inversion: inversion(),
        };
        for a in [0.2, 0.6, 1.0] {
            // At phase 0 the log-sine sits exactly at the operating point.
            assert!((drive(a).normalised_intensity(0.0) - u_g).abs() < 1e-12);
            let table = drive(a).warp_table().expect("in range");
            let intensities: Vec<f64> = table
                .iter()
                .map(|&code| lobe_transmission(f64::from(code), &inversion()))
                .collect();
            let max = intensities.iter().cloned().fold(f64::MIN, f64::max);
            let min = intensities.iter().cloned().fold(f64::MAX, f64::min);
            assert!(((max / min).ln() - a).abs() < 0.05, "a={a}");
        }
    }

    #[test]
    fn bessel_normalization_keeps_the_log_sine_cycle_mean() {
        for depth_a in [0.2, 0.4, 1.0, 2.0, 6.0] {
            let mean_u = 0.2;
            let pedestal = log_sine_geometric_pedestal(mean_u, depth_a);
            let sample_mean = (0..65_536)
                .map(|index| {
                    let phase = 2.0 * PI * index as f64 / 65_536.0;
                    pedestal * (0.5 * depth_a * phase.sin()).exp()
                })
                .sum::<f64>()
                / 65_536.0;
            assert!(
                (sample_mean - mean_u).abs() < 1e-12,
                "a={depth_a}: mean={sample_mean}"
            );
        }
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    /// The whole point of the range helpers: what they report as the boundary
    /// has to be exactly where `warp_table` stops accepting the drive. If they
    /// disagree, the UI either offers a drive that is refused or hides one that
    /// would work.
    fn table_is_buildable(target: OpticalTarget, mean_u: f64, depth_a: f64) -> bool {
        let inversion = LobeInversion {
            v_null_dac: 200.0,
            v_pi_dac: 1_600.0,
        };
        let operating_point = match target {
            OpticalTarget::LogSine => log_sine_geometric_pedestal(mean_u, depth_a),
            OpticalTarget::LinearSine => mean_u,
        };
        OpticalDrive {
            target,
            depth_a,
            operating_point,
            inversion,
        }
        .warp_table()
        .is_ok()
    }

    #[test]
    fn the_reported_max_depth_is_exactly_where_the_table_stops_building() {
        for target in [OpticalTarget::LogSine, OpticalTarget::LinearSine] {
            for mean_u in [0.2, 0.5, 0.8, 0.95] {
                let max_a = PeakLaw::of(target).max_depth_for_mean(mean_u, 1.0);
                if max_a >= DEPTH_A_MAX {
                    continue;
                }
                assert!(
                    table_is_buildable(target, mean_u, max_a - 1e-4),
                    "{target:?} mean_u={mean_u} refused a just inside the reported max {max_a}"
                );
                assert!(
                    !table_is_buildable(target, mean_u, max_a + 1e-2),
                    "{target:?} mean_u={mean_u} accepted a past the reported max {max_a}"
                );
            }
        }
    }

    #[test]
    fn the_reported_max_mean_is_exactly_where_the_table_stops_building() {
        for target in [OpticalTarget::LogSine, OpticalTarget::LinearSine] {
            for depth_a in [0.1, 0.5, 1.5, 3.0] {
                let max_u = PeakLaw::of(target).max_mean_for_depth(depth_a, 1.0);
                assert!(
                    table_is_buildable(target, max_u - 1e-4, depth_a),
                    "{target:?} a={depth_a} refused a mean just inside the reported max {max_u}"
                );
                if max_u < 1.0 - 1e-3 {
                    assert!(
                        !table_is_buildable(target, max_u + 1e-2, depth_a),
                        "{target:?} a={depth_a} accepted a mean past the reported max {max_u}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_two_helpers_are_inverses_of_each_other() {
        for target in [OpticalTarget::LogSine, OpticalTarget::LinearSine] {
            for mean_u in [0.3, 0.6, 0.9] {
                let max_a = PeakLaw::of(target).max_depth_for_mean(mean_u, 1.0);
                if max_a >= DEPTH_A_MAX {
                    continue;
                }
                let back = PeakLaw::of(target).max_mean_for_depth(max_a, 1.0);
                assert!(
                    (back - mean_u).abs() < 1e-4,
                    "{target:?}: mean {mean_u} → a {max_a} → mean {back}"
                );
            }
        }
    }

    #[test]
    fn a_dac_ceiling_below_v_peak_lowers_the_reachable_intensity() {
        let inversion = LobeInversion {
            v_null_dac: 200.0,
            v_pi_dac: 1_600.0,
        };
        // The ceiling at the peak code imposes no limit at all.
        assert_eq!(inversion.peak_intensity_ceiling(1_800.0), 1.0);
        assert_eq!(inversion.peak_intensity_ceiling(4_095.0), 1.0);
        // Halfway up the lobe in code is sin²(π/4) = 0.5 in intensity.
        let half = inversion.peak_intensity_ceiling(1_000.0);
        assert!(
            (half - 0.5).abs() < 1e-9,
            "u at the half-span code = {half}"
        );
        // A ceiling at or below the null leaves nothing drivable.
        assert_eq!(inversion.peak_intensity_ceiling(200.0), 0.0);
    }

    #[test]
    fn the_log_swing_law_matches_the_calibrated_dac_sine_band() {
        // A calibrated DAC_SINE/SQUARE spans u in [ū·e^{-a/2}, ū·e^{+a/2}], so
        // its ceiling is reached at exactly a = 2 ln(u_max/ū).
        let law = PeakLaw::LogSwing;
        let max_a = law.max_depth_for_mean(0.5, 1.0);
        assert!((max_a - 2.0 * 2.0_f64.ln()).abs() < 1e-9, "max a = {max_a}");
        assert!((law.peak(0.5, max_a) - 1.0).abs() < 1e-9);
        assert!((law.max_mean_for_depth(max_a, 1.0) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_constant_hold_is_limited_only_by_its_own_brightness() {
        // CONST modulates nothing, so `a` must not restrict it — requiring the
        // modulated band here is what used to freeze a calibrated constant
        // drive at its last accepted code.
        let law = PeakLaw::Constant;
        assert_eq!(law.max_depth_for_mean(1.0, 1.0), DEPTH_A_MAX);
        assert_eq!(law.max_mean_for_depth(5.0, 1.0), 1.0);
        assert_eq!(law.peak(0.8, 3.0), 0.8);
    }

    #[test]
    fn a_mean_above_the_ceiling_reports_no_usable_depth() {
        // Not a panic and not a silently huge number: the operator has to see
        // that this operating point is simply out of reach.
        assert_eq!(PeakLaw::LogSine.max_depth_for_mean(0.9, 0.5), 0.0);
        assert_eq!(PeakLaw::LinearSine.max_depth_for_mean(0.9, 0.5), 0.0);
        assert_eq!(PeakLaw::LogSwing.max_depth_for_mean(0.9, 0.5), 0.0);
    }
}
