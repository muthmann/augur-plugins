//! Auto-windowed Bernoulli response probability `q_p(a, f)`.
//!
//! With the firmware phase-0 `EXT_TRIGGER` anchoring the camera phase, ON and OFF
//! events fall in opposite half-cycles, so the ON/OFF phase windows can be found
//! directly from the current fold — no separate bright "pilot" capture is needed.
//!
//! For each polarity we anchor a window on its phase-histogram peak and grow it
//! outward while the histogram stays above a floor (a fraction of the peak) **and**
//! that polarity still dominates the opposite one. The window therefore ends out in
//! the opposite half-cycle, where the polarity's events have died away, and can
//! never bleed into the other polarity's cluster.
//!
//! The response probability is then, per pixel `i` and cycle `c`:
//!
//! ```text
//! z_{i,c,p} = 1 if pixel i fires at least once in W_p during cycle c, else 0
//! q_p(a,f)  = (1 / (N_valid · M)) · Σ_i Σ_c z_{i,c,p}
//! ```
//!
//! computed independently for ON and OFF, where `M` is the number of complete
//! valid cycles and `N_valid` is the ROI minus masked pixels.
//!
//! This is the **live quicklook** definition. The authoritative `q_p(a, f)` fit
//! freezes the windows once (from the brightest recording) and applies them to all
//! amplitudes offline — auto-windowing per fold is deliberately not amplitude-frozen.

use std::collections::HashSet;

use crate::phase::PhaseFold;
use crate::types::Polarity;

/// Phase-histogram resolution used for window detection.
pub const HIST_BINS: usize = 64;

/// Circular phase window `[start, end)` in cycle fraction. When `start > end`
/// the window wraps past 1.0.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhaseWindow {
    pub start: f64,
    pub end: f64,
}

impl PhaseWindow {
    pub fn contains(&self, phase: f64) -> bool {
        let p = phase.rem_euclid(1.0);
        if self.start <= self.end {
            p >= self.start && p < self.end
        } else {
            p >= self.start || p < self.end
        }
    }
}

/// Region of interest in pixel coordinates; `x1`/`y1` are exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Roi {
    pub x0: u16,
    pub y0: u16,
    pub x1: u16,
    pub y1: u16,
}

impl Roi {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1
    }

    pub fn area(&self) -> usize {
        usize::from(self.x1.saturating_sub(self.x0)) * usize::from(self.y1.saturating_sub(self.y0))
    }
}

/// One recorded response-curve point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResponsePoint {
    pub measured_a: f64,
    pub q_on: f64,
    pub q_off: f64,
    pub cycles: usize,
    pub valid_pixels: usize,
}

/// ON/OFF phase histogram of a fold.
pub fn phase_histogram(fold: &PhaseFold, polarity: Polarity) -> Vec<f64> {
    let mut histogram = vec![0.0; HIST_BINS];
    for event in fold
        .events
        .iter()
        .filter(|event| event.polarity == polarity)
    {
        let phase = event.phase.rem_euclid(1.0);
        let bin = ((phase * HIST_BINS as f64) as usize).min(HIST_BINS - 1);
        histogram[bin] += 1.0;
    }
    histogram
}

/// Grows a circular window out from `hist`'s peak while the peak-relative floor is
/// met and this polarity keeps dominating `other`. Returns `None` on an empty
/// histogram.
fn grow_window(hist: &[f64], other: &[f64], floor_fraction: f64) -> Option<PhaseWindow> {
    let bins = hist.len();
    let peak = hist.iter().copied().fold(0.0_f64, f64::max);
    if peak <= 0.0 || bins == 0 {
        return None;
    }
    let floor = peak * floor_fraction;
    let peak_bin = hist
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(index, _)| index)?;

    // A bin belongs to this window when it clears the floor and this polarity is
    // at least as strong as the opposite one there.
    let keep = |index: usize| hist[index] >= floor && hist[index] >= other[index];

    // The peak anchors the window; grow right then left until a bin fails.
    let mut right = peak_bin;
    for step in 1..bins {
        let index = (peak_bin + step) % bins;
        if keep(index) {
            right = index;
        } else {
            break;
        }
    }
    let mut left = peak_bin;
    for step in 1..bins {
        let index = (peak_bin + bins - step) % bins;
        if keep(index) {
            left = index;
        } else {
            break;
        }
    }

    Some(PhaseWindow {
        start: left as f64 / bins as f64,
        end: ((right + 1) % bins) as f64 / bins as f64,
    })
}

/// Detects the ON and OFF phase windows directly from a fold's histograms.
pub fn auto_windows(fold: &PhaseFold, floor_fraction: f64) -> Option<(PhaseWindow, PhaseWindow)> {
    let on = phase_histogram(fold, Polarity::On);
    let off = phase_histogram(fold, Polarity::Off);
    let window_on = grow_window(&on, &off, floor_fraction)?;
    let window_off = grow_window(&off, &on, floor_fraction)?;
    Some((window_on, window_off))
}

/// Computes the ON/OFF Bernoulli response probabilities for one fold against the
/// given windows. `masked` holds pixels excluded inside the ROI. Returns `None`
/// when there are no complete cycles or no valid pixels.
pub fn response_probability(
    fold: &PhaseFold,
    window_on: PhaseWindow,
    window_off: PhaseWindow,
    roi: Roi,
    masked: &HashSet<(u16, u16)>,
) -> Option<(f64, f64, usize, usize)> {
    let cycles = fold.validation.cycle_count;
    if cycles == 0 {
        return None;
    }
    let masked_in_roi = masked.iter().filter(|(x, y)| roi.contains(*x, *y)).count();
    let valid_pixels = roi.area().saturating_sub(masked_in_roi);
    if valid_pixels == 0 {
        return None;
    }

    let mut on_hits: HashSet<(usize, u16, u16)> = HashSet::new();
    let mut off_hits: HashSet<(usize, u16, u16)> = HashSet::new();
    for event in &fold.events {
        if !roi.contains(event.x, event.y) || masked.contains(&(event.x, event.y)) {
            continue;
        }
        match event.polarity {
            Polarity::On if window_on.contains(event.phase) => {
                on_hits.insert((event.cycle_index, event.x, event.y));
            }
            Polarity::Off if window_off.contains(event.phase) => {
                off_hits.insert((event.cycle_index, event.x, event.y));
            }
            _ => {}
        }
    }

    let denom = valid_pixels as f64 * cycles as f64;
    Some((
        on_hits.len() as f64 / denom,
        off_hits.len() as f64 / denom,
        cycles,
        valid_pixels,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase::fold_events_free_running;
    use crate::types::CameraEvent;

    fn event(timestamp_us: u64, x: u16, y: u16, polarity: Polarity) -> CameraEvent {
        CameraEvent {
            timestamp_us,
            x,
            y,
            polarity,
        }
    }

    /// Builds a fold: every pixel in an N-wide ROI fires ON near phase 0.2 and
    /// OFF near phase 0.7, for `cycles` cycles at period 1000 us.
    fn respond_fold(cycles: u64, pixels: u16) -> PhaseFold {
        let mut events = Vec::new();
        for cycle in 0..cycles {
            let base = cycle * 1_000;
            for x in 0..pixels {
                events.push(event(base + 200, x, 0, Polarity::On));
                events.push(event(base + 700, x, 0, Polarity::Off));
            }
        }
        // One trailing event so the free-running fold spans `cycles` whole cycles.
        events.push(event(cycles * 1_000 + 10, 0, 0, Polarity::On));
        fold_events_free_running(&events, 1_000.0).expect("whole cycles")
    }

    fn windows_disjoint(on: &PhaseWindow, off: &PhaseWindow) -> bool {
        (0..HIST_BINS).all(|bin| {
            let phase = (bin as f64 + 0.5) / HIST_BINS as f64;
            !(on.contains(phase) && off.contains(phase))
        })
    }

    #[test]
    fn auto_windows_are_separated_and_classify_a_full_response() {
        let fold = respond_fold(20, 4);
        let (on, off) = auto_windows(&fold, 0.1).expect("windows");
        assert!(windows_disjoint(&on, &off));
        assert_ne!(on, off);
        // Free-running fold anchors phase 0 to the first event (an ON), so the ON
        // cluster sits at phase 0.0 and the OFF cluster half a cycle later at 0.5.
        assert!(on.contains(0.0) && off.contains(0.5));
        assert!(!on.contains(0.5) && !off.contains(0.0));

        let roi = Roi {
            x0: 0,
            y0: 0,
            x1: 4,
            y1: 1,
        };
        let (q_on, q_off, _, valid) =
            response_probability(&fold, on, off, roi, &HashSet::new()).expect("counts");
        assert_eq!(valid, 4);
        assert!(q_on > 0.98 && q_off > 0.98, "q_on={q_on} q_off={q_off}");
    }

    #[test]
    fn partial_pixel_response_gives_proportional_probability() {
        let roi = Roi {
            x0: 0,
            y0: 0,
            x1: 4,
            y1: 1,
        };
        let (on, off) = auto_windows(&respond_fold(20, 4), 0.1).expect("windows");

        // Only 2 of 4 ROI pixels respond every cycle -> q_on ~ 0.5.
        let weak = respond_fold(20, 2);
        let (q_on_weak, _, _, valid) =
            response_probability(&weak, on, off, roi, &HashSet::new()).expect("counts");
        assert_eq!(valid, 4);
        assert!((q_on_weak - 0.5).abs() < 0.05, "q_on_weak={q_on_weak}");
    }

    #[test]
    fn masked_pixels_are_subtracted_from_valid_count() {
        let roi = Roi {
            x0: 0,
            y0: 0,
            x1: 4,
            y1: 1,
        };
        let mut masked = HashSet::new();
        masked.insert((3_u16, 0_u16));
        let fold = respond_fold(10, 4);
        let (on, off) = auto_windows(&fold, 0.1).expect("windows");
        let (_, _, _, valid) = response_probability(&fold, on, off, roi, &masked).expect("counts");
        assert_eq!(valid, 3);
    }
}
