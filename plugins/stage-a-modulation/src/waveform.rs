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
//! The inversion parameters `V_null` and `Vπ` are expressed in **DAC codes** and
//! are settable: the engineer should not rely on nominal `Vπ` but sweep settled
//! constant DAC codes, measure the actual optical transfer, and enter the frozen
//! `V_null` / `Vπ` of one monotonic lobe. A fully measured lookup table can
//! replace this analytic inversion later behind the same interface.

use std::f64::consts::PI;

/// Warp-table length played back over one modulation period.
pub const WARP_TABLE_LEN: usize = 256;
/// Full-scale DAC code (12-bit).
pub const DAC_FULL_SCALE: u16 = 4_095;

/// Optical intensity target the drive should reproduce, swung around the
/// operating point `u_k`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpticalTarget {
    /// Recommended A1 log-intensity sine: `ln I = ln I_k + (a/2) sin ωt`.
    LogSine,
    /// Literal linear-intensity sine: `I = I_k (1 + m sin ωt)`, `m = tanh(a/2)`.
    LinearSine,
}

/// Frozen inversion of one monotonic Pockels/PBS lobe, in DAC codes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LobeInversion {
    /// DAC code where the excitation light is at its minimum (`sin² = 0`).
    pub v_null_dac: f64,
    /// DAC-code distance from `v_null` to the excitation maximum (quarter wave).
    pub v_pi_dac: f64,
}

impl LobeInversion {
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OpticalDrive {
    pub target: OpticalTarget,
    /// Optical log-modulation depth `a = ln(I_max / I_min)`, must be positive.
    pub depth_a: f64,
    /// Operating illumination `I_k` as a normalised lobe intensity `u_k ∈ (0, 1]`:
    /// the geometric-mean point the modulation swings around. Held fixed while
    /// `a` is swept, so one response curve keeps `I_k` constant.
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
    /// The peak optical target exceeds the lobe ceiling (`u_k · peak > 1`): the
    /// operating point is too bright for this depth and would saturate.
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
    /// around the operating point `u_k` (not peak-normalised).
    pub fn normalised_intensity(&self, phase: f64) -> f64 {
        let sine = (2.0 * PI * phase).sin();
        match self.target {
            // ln I = ln I_k + (a/2) sin ωt.
            OpticalTarget::LogSine => self.operating_point * (0.5 * self.depth_a * sine).exp(),
            // I = I_k (1 + m sin ωt), m = tanh(a/2).
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
        // Null at code 200, quarter wave 1600 codes later (peak light at 1800).
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
    fn fixed_operating_point_keeps_i_k_while_sweeping_a() {
        // One response curve: fix u_k, vary a. The geometric-mean intensity at
        // phase 0 (sin = 0) stays put; only the contrast grows with a.
        let u_k = 0.3;
        let drive = |a: f64| OpticalDrive {
            target: OpticalTarget::LogSine,
            depth_a: a,
            operating_point: u_k,
            inversion: inversion(),
        };
        for a in [0.2, 0.6, 1.0] {
            // At phase 0 the log-sine sits exactly at the operating point.
            assert!((drive(a).normalised_intensity(0.0) - u_k).abs() < 1e-12);
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
}
