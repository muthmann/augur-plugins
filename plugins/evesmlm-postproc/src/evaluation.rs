use std::collections::BTreeMap;

use augur_plugin_evesmlm_fitting::{EveLocalization, EveLocalizationResults};

const DEFAULT_PSF_SIZE: usize = 9;
const TRACK_LINK_RADIUS_PX: f64 = 1.5;

#[derive(Debug, Clone)]
struct Track {
    x: f64,
    y: f64,
    length: usize,
}

#[derive(Debug, Clone)]
pub struct PsfAccumulator {
    size: usize,
    sum: Vec<f64>,
    count: usize,
}

impl Default for PsfAccumulator {
    fn default() -> Self {
        Self {
            size: DEFAULT_PSF_SIZE,
            sum: vec![0.0; DEFAULT_PSF_SIZE * DEFAULT_PSF_SIZE],
            count: 0,
        }
    }
}

impl PsfAccumulator {
    pub fn add_localization(&mut self, localization: &EveLocalization) {
        let radius = (self.size as isize - 1) / 2;
        let sigma_x = localization.sigma_x.max(1.0);
        let sigma_y = localization.sigma_y.max(1.0);
        let frac_x = localization.x.fract();
        let frac_y = localization.y.fract();

        for local_y in -radius..=radius {
            for local_x in -radius..=radius {
                let dx = local_x as f64 - frac_x;
                let dy = local_y as f64 - frac_y;
                let value = (-0.5 * (dx * dx / sigma_x.powi(2) + dy * dy / sigma_y.powi(2))).exp();
                let x = (local_x + radius) as usize;
                let y = (local_y + radius) as usize;
                self.sum[y * self.size + x] += value;
            }
        }
        self.count += 1;
    }

    pub fn mean_patch(&self) -> Option<Vec<f64>> {
        if self.count == 0 {
            return None;
        }
        Some(
            self.sum
                .iter()
                .map(|value| *value / self.count as f64)
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct EvaluationState {
    pub nn_distances_px: Vec<f64>,
    psf: PsfAccumulator,
    completed_track_lengths: Vec<usize>,
    active_tracks: Vec<Track>,
}

impl EvaluationState {
    pub fn update(&mut self, results: &EveLocalizationResults) {
        self.nn_distances_px
            .extend(nearest_neighbor_distances(&results.localizations));
        for localization in &results.localizations {
            self.psf.add_localization(localization);
        }
        self.update_tracks(&results.localizations);
    }

    pub fn enena_sigma_px(&self) -> Option<f64> {
        fit_rayleigh_sigma(&self.nn_distances_px)
    }

    pub fn enena_sigma_nm(&self, nm_per_pixel: f64) -> Option<f64> {
        self.enena_sigma_px().map(|value| value * nm_per_pixel)
    }

    pub fn mean_psf(&self) -> Option<(usize, Vec<f64>)> {
        self.psf.mean_patch().map(|patch| (self.psf.size, patch))
    }

    pub fn on_time_histogram(&self) -> Vec<(usize, usize)> {
        let mut counts = BTreeMap::new();
        for length in self
            .completed_track_lengths
            .iter()
            .copied()
            .chain(self.active_tracks.iter().map(|track| track.length))
        {
            *counts.entry(length).or_insert(0) += 1;
        }
        counts.into_iter().collect()
    }

    pub fn reset(&mut self) {
        self.nn_distances_px.clear();
        self.psf = PsfAccumulator::default();
        self.completed_track_lengths.clear();
        self.active_tracks.clear();
    }

    fn update_tracks(&mut self, localizations: &[EveLocalization]) {
        let mut assigned = vec![false; self.active_tracks.len()];
        let mut next_tracks = Vec::new();
        let max_distance2 = TRACK_LINK_RADIUS_PX * TRACK_LINK_RADIUS_PX;

        for localization in localizations {
            let mut best_match = None;
            let mut best_distance2 = max_distance2;
            for (index, track) in self.active_tracks.iter().enumerate() {
                if assigned[index] {
                    continue;
                }
                let dx = localization.x - track.x;
                let dy = localization.y - track.y;
                let distance2 = dx * dx + dy * dy;
                if distance2 <= best_distance2 {
                    best_distance2 = distance2;
                    best_match = Some(index);
                }
            }

            if let Some(index) = best_match {
                assigned[index] = true;
                let mut track = self.active_tracks[index].clone();
                track.x = localization.x;
                track.y = localization.y;
                track.length += 1;
                next_tracks.push(track);
            } else {
                next_tracks.push(Track {
                    x: localization.x,
                    y: localization.y,
                    length: 1,
                });
            }
        }

        for (index, track) in self.active_tracks.drain(..).enumerate() {
            if !assigned[index] {
                self.completed_track_lengths.push(track.length);
            }
        }

        self.active_tracks = next_tracks;
    }
}

pub fn nearest_neighbor_distances(localizations: &[EveLocalization]) -> Vec<f64> {
    let mut distances = Vec::new();
    for (index, localization) in localizations.iter().enumerate() {
        let mut nearest = f64::INFINITY;
        for (other_index, other) in localizations.iter().enumerate() {
            if index == other_index {
                continue;
            }
            let dx = localization.x - other.x;
            let dy = localization.y - other.y;
            nearest = nearest.min((dx * dx + dy * dy).sqrt());
        }
        if nearest.is_finite() {
            distances.push(nearest);
        }
    }
    distances
}

pub fn fit_rayleigh_sigma(distances: &[f64]) -> Option<f64> {
    if distances.is_empty() {
        return None;
    }
    let sigma = (distances.iter().map(|value| value * value).sum::<f64>()
        / (2.0 * distances.len() as f64))
        .sqrt();
    Some(sigma)
}
