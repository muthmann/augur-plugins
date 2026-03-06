//! eveSMLM Candidate Finding Plugin
//!
//! Clusters raw event-camera `CdEvent` samples into per-emitter candidate
//! groups using raw-event DBSCAN, eigenfeature filtering, or a frame-based
//! fallback built on a wavelet-thresholded accumulation image.

pub mod dbscan;
pub mod eigenfeature;
pub mod types;

use std::collections::HashMap;

use augur_core::{
    analysis::{AnalysisOutput, AnalysisSeverity, AnalysisWarning, Overlay, Pixel},
    pipeline::{CdEvent, PreviewFrame},
};

pub use types::{CandidateFindingMethod, EveCandidates, EveCluster};

const KERNEL_G1: [f64; 5] = [1.0 / 16.0, 0.25, 3.0 / 8.0, 0.25, 1.0 / 16.0];
const KERNEL_G2: [f64; 9] = [
    1.0 / 16.0,
    0.0,
    0.25,
    0.0,
    3.0 / 8.0,
    0.0,
    0.25,
    0.0,
    1.0 / 16.0,
];
const OVERLAY_COLOR: [u8; 4] = [255, 210, 32, 220];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolarityMode {
    Positive,
    Negative,
    #[default]
    Both,
}

impl PolarityMode {
    fn include_event(self, event: &CdEvent) -> bool {
        match self {
            Self::Positive => event.polarity,
            Self::Negative => !event.polarity,
            Self::Both => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CandidateSettings {
    pub finding_method: CandidateFindingMethod,
    pub polarity: PolarityMode,
    pub epsilon_px: f64,
    pub min_events: usize,
    pub max_spatial_extent_px: f64,
    pub min_isotropy: f64,
    pub threshold_factor: f64,
    pub fit_radius_px: usize,
    pub max_candidates: usize,
    pub show_overlay: bool,
}

impl Default for CandidateSettings {
    fn default() -> Self {
        Self {
            finding_method: CandidateFindingMethod::Dbscan,
            polarity: PolarityMode::Both,
            epsilon_px: 3.0,
            min_events: 5,
            max_spatial_extent_px: 5.0,
            min_isotropy: 0.2,
            threshold_factor: 1.5,
            fit_radius_px: 4,
            max_candidates: 512,
            show_overlay: true,
        }
    }
}

pub struct EveSmlmCandidatePlugin {
    enabled: bool,
    settings: CandidateSettings,
    last_candidate_count: usize,
    last_event_count: usize,
    last_status: String,
}

impl Default for EveSmlmCandidatePlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: CandidateSettings::default(),
            last_candidate_count: 0,
            last_event_count: 0,
            last_status: "Enable the plugin to cluster raw eveSMLM events into candidates.".into(),
        }
    }
}

impl EveSmlmCandidatePlugin {
    pub fn name(&self) -> &str {
        "EVE Candidate Finding"
    }

    pub fn description(&self) -> &str {
        "Candidate finding directly on raw event streams using DBSCAN, eigenfeatures, or a frame-based fallback."
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    pub fn settings(&self) -> &CandidateSettings {
        &self.settings
    }

    pub fn settings_mut(&mut self) -> &mut CandidateSettings {
        &mut self.settings
    }

    pub fn last_candidate_count(&self) -> usize {
        self.last_candidate_count
    }

    pub fn last_event_count(&self) -> usize {
        self.last_event_count
    }

    pub fn last_status(&self) -> &str {
        &self.last_status
    }

    pub fn analyze_frame(
        &mut self,
        frame: &PreviewFrame,
        raw_events: Option<&[CdEvent]>,
        output: &mut AnalysisOutput,
    ) -> EveCandidates {
        let Some(events) = raw_events else {
            self.last_candidate_count = 0;
            self.last_event_count = 0;
            self.last_status = "Raw events are unavailable for this preview frame.".into();
            output.warnings.push(AnalysisWarning {
                source: self.name().to_owned(),
                severity: AnalysisSeverity::Info,
                message: "EVE candidate finding requires the raw event stream.".into(),
            });
            return empty_candidates(frame, self.settings.finding_method);
        };

        let filtered_events = filter_events_by_polarity(events, self.settings.polarity);
        self.last_event_count = filtered_events.len();
        if filtered_events.is_empty() {
            self.last_candidate_count = 0;
            self.last_status = "No events passed the configured polarity filter.".into();
            return EveCandidates {
                clusters: Vec::new(),
                frame_window_start_us: frame.window_start_us,
                frame_window_end_us: frame.window_end_us,
                n_events_processed: 0,
                finding_method: self.settings.finding_method,
            };
        }

        let cluster_indices = match self.settings.finding_method {
            CandidateFindingMethod::Dbscan => dbscan::cluster_event_indices(
                &filtered_events,
                self.settings.epsilon_px,
                self.settings.min_events,
            ),
            CandidateFindingMethod::Eigenfeature => {
                let relaxed_min_events = self.settings.min_events.saturating_sub(2).max(2);
                let seed_clusters = dbscan::cluster_event_indices(
                    &filtered_events,
                    self.settings.epsilon_px,
                    relaxed_min_events,
                );
                eigenfeature::filter_clusters(
                    &filtered_events,
                    seed_clusters,
                    self.settings.max_spatial_extent_px,
                    self.settings.min_isotropy,
                )
            }
            CandidateFindingMethod::FrameBased => {
                frame_based_clusters(frame, &filtered_events, &self.settings)
            }
        };

        let mut clusters = clusters_from_indices(&filtered_events, cluster_indices);
        clusters.sort_by_key(|cluster| std::cmp::Reverse(cluster.event_count()));
        if clusters.len() > self.settings.max_candidates {
            clusters.truncate(self.settings.max_candidates);
        }

        self.last_candidate_count = clusters.len();
        self.last_status = format!(
            "{} candidates from {} events using {}.",
            self.last_candidate_count,
            self.last_event_count,
            self.settings.finding_method.label()
        );

        if self.settings.show_overlay && !clusters.is_empty() {
            output.overlays.push(Overlay::HighlightPixels {
                pixels: clusters
                    .iter()
                    .map(|cluster| Pixel {
                        x: cluster.centroid_x.round().max(0.0) as u16,
                        y: cluster.centroid_y.round().max(0.0) as u16,
                    })
                    .collect(),
                color: OVERLAY_COLOR,
            });
        }

        EveCandidates {
            clusters,
            frame_window_start_us: frame.window_start_us,
            frame_window_end_us: frame.window_end_us,
            n_events_processed: filtered_events.len(),
            finding_method: self.settings.finding_method,
        }
    }

    pub fn reset(&mut self) {
        self.last_candidate_count = 0;
        self.last_event_count = 0;
        self.last_status = "Waiting for the next preview frame.".into();
    }
}

fn empty_candidates(frame: &PreviewFrame, method: CandidateFindingMethod) -> EveCandidates {
    EveCandidates {
        clusters: Vec::new(),
        frame_window_start_us: frame.window_start_us,
        frame_window_end_us: frame.window_end_us,
        n_events_processed: 0,
        finding_method: method,
    }
}

fn filter_events_by_polarity(events: &[CdEvent], polarity: PolarityMode) -> Vec<CdEvent> {
    events
        .iter()
        .copied()
        .filter(|event| polarity.include_event(event))
        .collect()
}

fn clusters_from_indices(events: &[CdEvent], cluster_indices: Vec<Vec<usize>>) -> Vec<EveCluster> {
    cluster_indices
        .into_iter()
        .filter_map(|indices| {
            if indices.is_empty() {
                return None;
            }

            let mut pixel_histogram: HashMap<(u16, u16), (u32, u32)> = HashMap::new();
            let mut cluster_events = Vec::with_capacity(indices.len());
            let mut sum_x = 0.0;
            let mut sum_y = 0.0;
            let mut x_min = u16::MAX;
            let mut x_max = 0;
            let mut y_min = u16::MAX;
            let mut y_max = 0;

            for index in indices {
                let event = events[index];
                cluster_events.push(event);
                sum_x += f64::from(event.x);
                sum_y += f64::from(event.y);
                x_min = x_min.min(event.x);
                x_max = x_max.max(event.x);
                y_min = y_min.min(event.y);
                y_max = y_max.max(event.y);

                let counts = pixel_histogram.entry((event.x, event.y)).or_insert((0, 0));
                if event.polarity {
                    counts.0 += 1;
                } else {
                    counts.1 += 1;
                }
            }

            let count = cluster_events.len() as f64;
            let mut histogram_entries: Vec<_> = pixel_histogram
                .into_iter()
                .map(|((x, y), (positive, negative))| (x, y, positive, negative))
                .collect();
            histogram_entries.sort_by_key(|entry| (entry.1, entry.0));

            Some(EveCluster {
                pixel_histogram: histogram_entries,
                events: cluster_events,
                centroid_x: sum_x / count,
                centroid_y: sum_y / count,
                x_min,
                x_max,
                y_min,
                y_max,
            })
        })
        .collect()
}

fn frame_based_clusters(
    frame: &PreviewFrame,
    events: &[CdEvent],
    settings: &CandidateSettings,
) -> Vec<Vec<usize>> {
    let width = frame.width as usize;
    let height = frame.height as usize;
    let image = build_analysis_image(frame, events);
    if image.is_empty() || width < 3 || height < 3 {
        return Vec::new();
    }

    let v1 = smooth_image(&image, width, height, &KERNEL_G1);
    let v2 = smooth_image(&v1, width, height, &KERNEL_G2);
    let f1: Vec<f64> = image.iter().zip(&v1).map(|(a, b)| a - b).collect();
    let f2: Vec<f64> = v1.iter().zip(&v2).map(|(a, b)| a - b).collect();
    let sigma = standard_deviation(&f1);
    let threshold = settings.threshold_factor * sigma;
    let filtered: Vec<f64> = f2
        .iter()
        .map(|value| if *value > threshold { *value } else { 0.0 })
        .collect();

    let mut maxima = find_local_maxima(&filtered, width, height);
    maxima.sort_by(|left, right| right.2.total_cmp(&left.2));
    if maxima.len() > settings.max_candidates {
        maxima.truncate(settings.max_candidates);
    }

    let radius2 = (settings.fit_radius_px as f64).powi(2);
    maxima
        .into_iter()
        .filter_map(|(x, y, _)| {
            let cluster: Vec<usize> = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| {
                    let dx = f64::from(event.x) - x as f64;
                    let dy = f64::from(event.y) - y as f64;
                    if dx * dx + dy * dy <= radius2 {
                        Some(index)
                    } else {
                        None
                    }
                })
                .collect();
            if cluster.len() >= settings.min_events {
                Some(cluster)
            } else {
                None
            }
        })
        .collect()
}

fn build_analysis_image(frame: &PreviewFrame, events: &[CdEvent]) -> Vec<f64> {
    let mut image = vec![0.0; frame.pixels.len()];
    for event in events {
        if event.x >= frame.width || event.y >= frame.height {
            continue;
        }
        let index = event.y as usize * frame.width as usize + event.x as usize;
        let weight = event.timestamp.saturating_sub(frame.window_start_us).max(1) as f64;
        image[index] += weight;
    }
    image
}

fn smooth_image(input: &[f64], width: usize, height: usize, kernel: &[f64]) -> Vec<f64> {
    let radius = kernel.len() / 2;
    let mut horizontal = vec![0.0; input.len()];
    let mut output = vec![0.0; input.len()];

    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0;
            for (offset, weight) in kernel.iter().enumerate() {
                let source_x = clamp_index(x as isize + offset as isize - radius as isize, width);
                acc += input[y * width + source_x] * *weight;
            }
            horizontal[y * width + x] = acc;
        }
    }

    for y in 0..height {
        for x in 0..width {
            let mut acc = 0.0;
            for (offset, weight) in kernel.iter().enumerate() {
                let source_y = clamp_index(y as isize + offset as isize - radius as isize, height);
                acc += horizontal[source_y * width + x] * *weight;
            }
            output[y * width + x] = acc;
        }
    }

    output
}

fn find_local_maxima(image: &[f64], width: usize, height: usize) -> Vec<(usize, usize, f64)> {
    let mut maxima = Vec::new();
    if width < 3 || height < 3 {
        return maxima;
    }

    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let value = image[y * width + x];
            if value <= 0.0 {
                continue;
            }

            let mut is_maximum = true;
            for ny in y - 1..=y + 1 {
                for nx in x - 1..=x + 1 {
                    if nx == x && ny == y {
                        continue;
                    }
                    if image[ny * width + nx] > value {
                        is_maximum = false;
                        break;
                    }
                }
                if !is_maximum {
                    break;
                }
            }

            if is_maximum {
                maxima.push((x, y, value));
            }
        }
    }

    maxima
}

fn standard_deviation(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| {
            let delta = *value - mean;
            delta * delta
        })
        .sum::<f64>()
        / values.len() as f64;
    variance.sqrt()
}

fn clamp_index(index: isize, limit: usize) -> usize {
    index.clamp(0, limit.saturating_sub(1) as isize) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(x: u16, y: u16, polarity: bool, timestamp: u64) -> CdEvent {
        CdEvent {
            x,
            y,
            timestamp,
            polarity,
        }
    }

    #[test]
    fn dbscan_finds_a_single_cluster() {
        let events = vec![
            event(10, 10, true, 1),
            event(10, 11, true, 2),
            event(11, 10, true, 3),
            event(11, 11, true, 4),
            event(9, 10, true, 5),
            event(10, 9, true, 6),
            event(9, 9, true, 7),
            event(11, 9, true, 8),
            event(9, 11, true, 9),
            event(12, 10, true, 10),
        ];

        let clusters = dbscan::cluster_event_indices(&events, 1.5, 5);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].len(), events.len());
    }

    #[test]
    fn dbscan_separates_distant_clusters() {
        let mut events = vec![
            event(10, 10, true, 1),
            event(10, 11, true, 2),
            event(11, 10, true, 3),
            event(11, 11, true, 4),
            event(9, 10, true, 5),
        ];
        events.extend([
            event(40, 40, true, 11),
            event(41, 40, true, 12),
            event(40, 41, true, 13),
            event(41, 41, true, 14),
            event(42, 40, true, 15),
        ]);

        let clusters = dbscan::cluster_event_indices(&events, 1.5, 4);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn dbscan_discards_noise_points() {
        let events = vec![
            event(10, 10, true, 1),
            event(20, 20, true, 2),
            event(30, 30, true, 3),
        ];

        let clusters = dbscan::cluster_event_indices(&events, 1.5, 2);
        assert!(clusters.is_empty());
    }

    #[test]
    fn eigenfeature_rejects_elongated_clusters() {
        let mut events = vec![
            event(5, 5, true, 1),
            event(6, 5, true, 2),
            event(7, 5, true, 3),
            event(8, 5, true, 4),
            event(9, 5, true, 5),
        ];
        let elongated_indices: Vec<usize> = (0..events.len()).collect();

        let circular_offset = events.len();
        events.extend([
            event(20, 20, true, 11),
            event(21, 20, true, 12),
            event(20, 21, true, 13),
            event(21, 21, true, 14),
            event(20, 19, true, 15),
            event(19, 20, true, 16),
        ]);
        let circular_indices: Vec<usize> = (circular_offset..events.len()).collect();

        let filtered = eigenfeature::filter_clusters(
            &events,
            vec![elongated_indices, circular_indices.clone()],
            5.0,
            0.4,
        );

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0], circular_indices);
    }

    #[test]
    fn polarity_filter_keeps_the_requested_subset() {
        let events = vec![
            event(1, 1, true, 1),
            event(1, 2, false, 2),
            event(2, 1, true, 3),
            event(2, 2, false, 4),
        ];

        let positive = filter_events_by_polarity(&events, PolarityMode::Positive);
        let negative = filter_events_by_polarity(&events, PolarityMode::Negative);

        assert_eq!(positive.len(), 2);
        assert!(positive.iter().all(|event| event.polarity));
        assert_eq!(negative.len(), 2);
        assert!(negative.iter().all(|event| !event.polarity));
    }
}
