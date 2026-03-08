//! eveSMLM Candidate Finding Plugin
//!
//! Clusters raw event-camera `CdEvent` samples into per-emitter candidate
//! groups using raw-event DBSCAN, eigenfeature filtering, or a frame-based
//! fallback built on a wavelet-thresholded accumulation image.

pub mod dbscan;
pub mod eigenfeature;
pub mod types;

use std::collections::HashMap;

use augur_plugin_api::{
    export_plugin, AnalysisSeverity, FfiCdEvent, FfiPixel, HostContext, HostOutput, Plugin,
    PluginFrame, PluginInput, SettingItem, SettingKind, SettingsSchema, SettingsSection,
    StatusEntry,
};
use serde_json::{json, Value};

pub use types::{CandidateFindingMethod, EveCandidates, EveCluster, EveEvent, CTX_EVE_CANDIDATES};

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
    fn label(self) -> &'static str {
        match self {
            Self::Positive => "Positive",
            Self::Negative => "Negative",
            Self::Both => "Both",
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Positive,
            1 => Self::Negative,
            _ => Self::Both,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Positive => 0,
            Self::Negative => 1,
            Self::Both => 2,
        }
    }

    fn include_event(self, event: &EveEvent) -> bool {
        match self {
            Self::Positive => event.polarity,
            Self::Negative => !event.polarity,
            Self::Both => true,
        }
    }
}

impl CandidateFindingMethod {
    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Eigenfeature,
            2 => Self::FrameBased,
            _ => Self::Dbscan,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Dbscan => 0,
            Self::Eigenfeature => 1,
            Self::FrameBased => 2,
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
    fn analyze_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        raw_events: &[FfiCdEvent],
        output: &mut HostOutput<'_>,
    ) -> EveCandidates {
        if raw_events.is_empty() {
            self.last_candidate_count = 0;
            self.last_event_count = 0;
            self.last_status = "Raw events are unavailable for this preview frame.".into();
            Self::warning(
                output,
                AnalysisSeverity::Info,
                "EVE candidate finding requires the raw event stream.",
            );
            return empty_candidates(frame, self.settings.finding_method);
        }

        let filtered_events: Vec<EveEvent> = raw_events
            .iter()
            .copied()
            .map(EveEvent::from)
            .filter(|event| self.settings.polarity.include_event(event))
            .collect();
        self.last_event_count = filtered_events.len();
        if filtered_events.is_empty() {
            self.last_candidate_count = 0;
            self.last_status = "No events passed the configured polarity filter.".into();
            return EveCandidates {
                clusters: Vec::new(),
                frame_window_start_us: frame.window_start_us(),
                frame_window_end_us: frame.window_end_us(),
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
            let pixels: Vec<FfiPixel> = clusters
                .iter()
                .map(|cluster| FfiPixel {
                    x: cluster.centroid_x.round().max(0.0) as u16,
                    y: cluster.centroid_y.round().max(0.0) as u16,
                })
                .collect();
            output.add_highlight_pixels(&pixels, OVERLAY_COLOR);
        }

        EveCandidates {
            clusters,
            frame_window_start_us: frame.window_start_us(),
            frame_window_end_us: frame.window_end_us(),
            n_events_processed: filtered_events.len(),
            finding_method: self.settings.finding_method,
        }
    }

    pub fn reset(&mut self) {
        self.last_candidate_count = 0;
        self.last_event_count = 0;
        self.last_status = "Waiting for the next preview frame.".into();
    }

    fn parse_usize(value: Value) -> Option<usize> {
        value.as_u64().and_then(|value| usize::try_from(value).ok())
    }

    fn warning(output: &mut HostOutput<'_>, severity: AnalysisSeverity, message: &str) {
        output.add_warning("EVE Candidate Finding", severity, message);
    }
}

impl Plugin for EveSmlmCandidatePlugin {
    fn name(&self) -> &'static str {
        "EVE Candidate Finding"
    }

    fn description(&self) -> &'static str {
        "Candidate finding directly on raw event streams using DBSCAN, eigenfeatures, or a frame-based fallback."
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.reset();
        }
    }

    fn reset(&mut self) {
        EveSmlmCandidatePlugin::reset(self);
    }

    fn input_kind(&self) -> PluginInput {
        PluginInput::RawEvents
    }

    fn process_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
    ) {
        let candidates = self.analyze_frame(frame, frame.events(), output);
        if let Err(err) = context.publish(CTX_EVE_CANDIDATES, &candidates) {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Publishing EVE candidates failed: {err}"),
            );
        }
    }

    fn settings_schema(&self) -> SettingsSchema {
        SettingsSchema {
            sections: vec![
                SettingsSection {
                    label: "Candidate finding".into(),
                    description: Some(
                        "Cluster raw events directly, or fall back to a wavelet-thresholded accumulation image."
                            .into(),
                    ),
                    default_open: true,
                    items: vec![
                        SettingItem {
                            key: "finding_method".into(),
                            label: "Method".into(),
                            tooltip: Some("Choose the clustering backend used to propose emitter candidates.".into()),
                            kind: SettingKind::Enum {
                                variants: vec![
                                    CandidateFindingMethod::Dbscan.label().into(),
                                    CandidateFindingMethod::Eigenfeature.label().into(),
                                    CandidateFindingMethod::FrameBased.label().into(),
                                ],
                                default: self.settings.finding_method.index(),
                            },
                        },
                        SettingItem {
                            key: "polarity".into(),
                            label: "Polarity".into(),
                            tooltip: Some("Restrict candidate finding to positive, negative, or all events.".into()),
                            kind: SettingKind::Enum {
                                variants: vec![
                                    PolarityMode::Positive.label().into(),
                                    PolarityMode::Negative.label().into(),
                                    PolarityMode::Both.label().into(),
                                ],
                                default: self.settings.polarity.index(),
                            },
                        },
                        SettingItem {
                            key: "epsilon_px".into(),
                            label: "DBSCAN radius".into(),
                            tooltip: Some("Neighborhood radius in pixels for DBSCAN and the eigenfeature seed step.".into()),
                            kind: SettingKind::F64Slider {
                                min: 1.0,
                                max: 10.0,
                                default: self.settings.epsilon_px,
                                suffix: Some(" px".into()),
                            },
                        },
                        SettingItem {
                            key: "min_events".into(),
                            label: "Min events".into(),
                            tooltip: Some("Minimum number of events required for a cluster to be kept.".into()),
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 64,
                                default: i64::try_from(self.settings.min_events).unwrap_or(5),
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "max_candidates".into(),
                            label: "Max candidates".into(),
                            tooltip: Some("Safety cap on the number of candidate clusters published per frame.".into()),
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 2048,
                                default: i64::try_from(self.settings.max_candidates).unwrap_or(512),
                                suffix: None,
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Refinement".into(),
                    description: Some(
                        "Frame-based mode and eigenfeature filtering use these thresholds to reject broad or anisotropic clusters."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "max_spatial_extent_px".into(),
                            label: "Max extent".into(),
                            tooltip: Some("Largest allowed principal-axis spread for eigenfeature filtering.".into()),
                            kind: SettingKind::F64Slider {
                                min: 1.0,
                                max: 20.0,
                                default: self.settings.max_spatial_extent_px,
                                suffix: Some(" px".into()),
                            },
                        },
                        SettingItem {
                            key: "min_isotropy".into(),
                            label: "Min isotropy".into(),
                            tooltip: Some("Minimum lambda2/lambda1 ratio for keeping a DBSCAN seed cluster.".into()),
                            kind: SettingKind::F64Slider {
                                min: 0.0,
                                max: 1.0,
                                default: self.settings.min_isotropy,
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "threshold_factor".into(),
                            label: "Threshold factor".into(),
                            tooltip: Some("Wavelet threshold multiplier used by the frame-based fallback.".into()),
                            kind: SettingKind::F64Slider {
                                min: 0.5,
                                max: 6.0,
                                default: self.settings.threshold_factor,
                                suffix: None,
                            },
                        },
                        SettingItem {
                            key: "fit_radius_px".into(),
                            label: "Gather radius".into(),
                            tooltip: Some("How far the frame-based mode reaches out from a detected maximum when collecting events.".into()),
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 16,
                                default: i64::try_from(self.settings.fit_radius_px).unwrap_or(4),
                                suffix: Some(" px".into()),
                            },
                        },
                        SettingItem {
                            key: "show_overlay".into(),
                            label: "Show overlay".into(),
                            tooltip: Some("Highlight candidate centroids on the preview.".into()),
                            kind: SettingKind::Bool {
                                default: self.settings.show_overlay,
                            },
                        },
                    ],
                },
            ],
        }
    }

    fn get_setting(&self, key: &str) -> Option<Value> {
        match key {
            "finding_method" => Some(json!(self.settings.finding_method.index())),
            "polarity" => Some(json!(self.settings.polarity.index())),
            "epsilon_px" => Some(json!(self.settings.epsilon_px)),
            "min_events" => Some(json!(self.settings.min_events)),
            "max_spatial_extent_px" => Some(json!(self.settings.max_spatial_extent_px)),
            "min_isotropy" => Some(json!(self.settings.min_isotropy)),
            "threshold_factor" => Some(json!(self.settings.threshold_factor)),
            "fit_radius_px" => Some(json!(self.settings.fit_radius_px)),
            "max_candidates" => Some(json!(self.settings.max_candidates)),
            "show_overlay" => Some(json!(self.settings.show_overlay)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        match key {
            "finding_method" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("finding_method must be an integer".into());
                };
                self.settings.finding_method = CandidateFindingMethod::from_index(value);
            }
            "polarity" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("polarity must be an integer".into());
                };
                self.settings.polarity = PolarityMode::from_index(value);
            }
            "epsilon_px" => {
                let Some(value) = value.as_f64() else {
                    return Err("epsilon_px must be numeric".into());
                };
                self.settings.epsilon_px = value.clamp(1.0, 10.0);
            }
            "min_events" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("min_events must be an integer".into());
                };
                self.settings.min_events = value.clamp(1, 64);
            }
            "max_spatial_extent_px" => {
                let Some(value) = value.as_f64() else {
                    return Err("max_spatial_extent_px must be numeric".into());
                };
                self.settings.max_spatial_extent_px = value.clamp(1.0, 20.0);
            }
            "min_isotropy" => {
                let Some(value) = value.as_f64() else {
                    return Err("min_isotropy must be numeric".into());
                };
                self.settings.min_isotropy = value.clamp(0.0, 1.0);
            }
            "threshold_factor" => {
                let Some(value) = value.as_f64() else {
                    return Err("threshold_factor must be numeric".into());
                };
                self.settings.threshold_factor = value.clamp(0.5, 6.0);
            }
            "fit_radius_px" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("fit_radius_px must be an integer".into());
                };
                self.settings.fit_radius_px = value.clamp(1, 16);
            }
            "max_candidates" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("max_candidates must be an integer".into());
                };
                self.settings.max_candidates = value.clamp(1, 2048);
            }
            "show_overlay" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_overlay must be a boolean".into());
                };
                self.settings.show_overlay = value;
            }
            _ => return Err(format!("unknown setting: {key}")),
        }

        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        vec![
            StatusEntry::Text(self.last_status.clone()),
            StatusEntry::LabeledValue {
                label: "Events".into(),
                value: self.last_event_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Candidates".into(),
                value: self.last_candidate_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Method".into(),
                value: self.settings.finding_method.label().into(),
                color: None,
            },
        ]
    }
}

fn empty_candidates(frame: &PluginFrame<'_>, method: CandidateFindingMethod) -> EveCandidates {
    EveCandidates {
        clusters: Vec::new(),
        frame_window_start_us: frame.window_start_us(),
        frame_window_end_us: frame.window_end_us(),
        n_events_processed: 0,
        finding_method: method,
    }
}

fn clusters_from_indices(events: &[EveEvent], cluster_indices: Vec<Vec<usize>>) -> Vec<EveCluster> {
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
    frame: &PluginFrame<'_>,
    events: &[EveEvent],
    settings: &CandidateSettings,
) -> Vec<Vec<usize>> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
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

fn build_analysis_image(frame: &PluginFrame<'_>, events: &[EveEvent]) -> Vec<f64> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    if width == 0 || height == 0 {
        return Vec::new();
    }

    let mut image = vec![0.0; width * height];
    for event in events {
        if event.x >= frame.width() || event.y >= frame.height() {
            continue;
        }
        let index = event.y as usize * width + event.x as usize;
        let weight = event
            .timestamp
            .saturating_sub(frame.window_start_us())
            .max(1) as f64;
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

    fn event(x: u16, y: u16, polarity: bool, timestamp: u64) -> EveEvent {
        EveEvent {
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

        let mut clusters = dbscan::cluster_event_indices(&events, 1.5, 4);
        clusters.sort_by_key(|cluster| cluster[0]);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].len(), 5);
        assert_eq!(clusters[1].len(), 5);
    }

    #[test]
    fn eigenfeature_rejects_line_like_cluster() {
        let line_events = vec![
            event(10, 10, true, 1),
            event(11, 10, true, 2),
            event(12, 10, true, 3),
            event(13, 10, true, 4),
            event(14, 10, true, 5),
        ];
        let compact_events = vec![
            event(20, 20, true, 1),
            event(20, 21, true, 2),
            event(21, 20, true, 3),
            event(21, 21, true, 4),
            event(22, 20, true, 5),
        ];

        let mut events = line_events.clone();
        events.extend(compact_events.clone());
        let clusters = vec![
            (0..line_events.len()).collect::<Vec<_>>(),
            (line_events.len()..events.len()).collect::<Vec<_>>(),
        ];

        let filtered = eigenfeature::filter_clusters(&events, clusters, 4.0, 0.25);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].len(), compact_events.len());
    }

    #[test]
    fn clusters_from_indices_computes_histogram_and_centroid() {
        let events = vec![
            event(10, 10, true, 1),
            event(10, 10, false, 2),
            event(11, 10, true, 3),
            event(10, 11, true, 4),
        ];

        let clusters = clusters_from_indices(&events, vec![vec![0, 1, 2, 3]]);
        assert_eq!(clusters.len(), 1);
        let cluster = &clusters[0];
        assert_eq!(cluster.event_count(), 4);
        assert_eq!(cluster.positive_event_count(), 3);
        assert_eq!(cluster.negative_event_count(), 1);
        assert_eq!(cluster.pixel_histogram.len(), 3);
        assert!((cluster.centroid_x - 10.25).abs() < 1e-6);
        assert!((cluster.centroid_y - 10.25).abs() < 1e-6);
    }

    #[test]
    fn build_analysis_image_weights_recent_events_more_strongly() {
        let events = vec![event(2, 1, true, 10), event(2, 1, true, 20)];
        let frame = TestFrame {
            width: 6,
            height: 4,
            window_start_us: 5,
        };

        let image = build_analysis_image(&frame.into_plugin_frame(), &events);
        let index = 1usize * 6 + 2usize;
        assert_eq!(image[index], 20.0);
    }

    #[test]
    fn local_maxima_detects_peaks() {
        let image = vec![
            0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 3.0, 1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 1.0, 0.0, 0.0, 0.0,
            1.0, 4.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
        ];

        let maxima = find_local_maxima(&image, 5, 5);
        assert_eq!(maxima.len(), 2);
        assert!(maxima.iter().any(|(x, y, _)| (*x, *y) == (1, 1)));
        assert!(maxima.iter().any(|(x, y, _)| (*x, *y) == (3, 3)));
    }

    struct TestFrame {
        width: u16,
        height: u16,
        window_start_us: u64,
    }

    impl TestFrame {
        fn into_plugin_frame(self) -> PluginFrame<'static> {
            let raw = Box::leak(Box::new(augur_plugin_api::FfiPreviewFrame {
                width: self.width,
                height: self.height,
                pixels: augur_plugin_api::FfiSlice::from_slice(&[] as &[u16]),
                events: augur_plugin_api::FfiSlice::from_slice(
                    &[] as &[augur_plugin_api::FfiCdEvent]
                ),
                window_start_us: self.window_start_us,
                window_end_us: self.window_start_us + 1,
            }));
            PluginFrame::new(raw)
        }
    }
}

export_plugin!(EveSmlmCandidatePlugin);
