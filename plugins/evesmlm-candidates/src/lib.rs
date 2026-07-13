//! eveSMLM Candidate Finding Plugin
//!
//! Clusters raw event-camera `CdEvent` samples into per-emitter candidate
//! groups using raw-event DBSCAN, eigenfeature filtering, or a frame-based
//! fallback built on a wavelet-thresholded accumulation image.

pub mod dbscan;
pub mod eigenfeature;
pub mod types;

use std::collections::{HashMap, HashSet};

use augur_plugin_api::{
    export_plugin, AnalysisSeverity, EventStoreHandle, FfiCdEvent, FfiColorRgba,
    FfiMarkerOverlayItem, FfiMarkerShape, FfiPixel, FfiString, HostContext, HostDatasetDescriptor,
    HostDatasetDisplayMetadata, HostDatasetKind, HostDatasetRelation, HostMarkerShape, HostOutput,
    HostViewDescriptor, HostViewKind, HostViewPlacement, HostViewRegistry, Plugin,
    PluginCapabilities, PluginFrame, PluginInput, PluginStateKind, SettingItem, SettingKind,
    SettingsSchema, SettingsSection, StatusEntry, TableColumn, TableColumnData,
    TableColumnDisplayEntry, TableColumnDisplayFormat, TableColumnDisplayMetadata,
    TableColumnValues, TableColumnWidthPriority, TableCoordinateSpace2d, TableCoordinateSpace3d,
    TableDatasetV1, TableRowProvenance, TableSchema, TableValueType,
};
use serde_json::{json, Value};

use types::TrackedCluster;
pub use types::{
    CandidateFindingMethod, ClusterBoundary, EveCandidates, EveCluster, EveEvent,
    CTX_EVE_CANDIDATES,
};

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
const ACCEPTED_EVENTS_COLOR: [u8; 4] = [60, 220, 140, 255];
const REJECTED_EVENTS_COLOR: [u8; 4] = [255, 110, 110, 235];
const COMPLETE_BOUNDARY_COLOR: [u8; 4] = [255, 255, 255, 60];
const PROVISIONAL_BOUNDARY_COLOR: [u8; 4] = [255, 255, 255, 28];
const COMPLETE_MARKER_COLOR: [u8; 4] = [255, 255, 255, 180];
const PROVISIONAL_MARKER_COLOR: [u8; 4] = [255, 255, 255, 110];
const ACCEPTED_EVENTS_DATASET_ID: &str = "augur.evesmlm.candidates.accepted_events";
const REJECTED_EVENTS_DATASET_ID: &str = "augur.evesmlm.candidates.rejected_events";
const ACCEPTED_EVENTS_LAYER_ID: &str = "augur.layer.evesmlm.accepted_events";
const REJECTED_EVENTS_LAYER_ID: &str = "augur.layer.evesmlm.rejected_events";
const ACCEPTED_EVENTS_COMPACT_VIEW_ID: &str = "augur.evesmlm.candidates.accepted_events.compact";
const REJECTED_EVENTS_COMPACT_VIEW_ID: &str = "augur.evesmlm.candidates.rejected_events.compact";
const ACCEPTED_EVENTS_TABLE_VIEW_ID: &str = "augur.evesmlm.candidates.accepted_events.table";
const REJECTED_EVENTS_TABLE_VIEW_ID: &str = "augur.evesmlm.candidates.rejected_events.table";
const ACCEPTED_EVENTS_3D_VIEW_ID: &str = "augur.evesmlm.candidates.accepted_events.scatter3d";
const REJECTED_EVENTS_3D_VIEW_ID: &str = "augur.evesmlm.candidates.rejected_events.scatter3d";
const CANDIDATE_FINDINGS_DATASET_ID: &str = "augur.evesmlm.candidates.candidate_findings";
const CANDIDATE_FINDING_PIXELS_DATASET_ID: &str =
    "augur.evesmlm.candidates.candidate_finding_pixels";
const CANDIDATE_FINDINGS_LAYER_ID: &str = "augur.layer.evesmlm.candidate_findings";
const CANDIDATE_FINDINGS_COMPACT_VIEW_ID: &str =
    "augur.evesmlm.candidates.candidate_findings.compact";
const CANDIDATE_FINDINGS_TABLE_VIEW_ID: &str = "augur.evesmlm.candidates.candidate_findings.table";
const CANDIDATE_FINDING_PIXELS_TABLE_VIEW_ID: &str =
    "augur.evesmlm.candidates.candidate_finding_pixels.table";

#[derive(Debug, Clone)]
struct CandidateEventRow {
    event_id: u64,
    x_px: f64,
    y_px: f64,
    timestamp_us: u64,
    polarity: bool,
    cluster_id: String,
}

#[derive(Debug, Clone, Default)]
struct CandidateEventDatasets {
    accepted: Vec<CandidateEventRow>,
    rejected: Vec<CandidateEventRow>,
    sensor_dims: Option<(u16, u16)>,
    frame_window_start_us: u64,
    frame_window_end_us: u64,
}

#[derive(Debug, Clone)]
struct CandidateFinding {
    cluster: EveCluster,
    method: CandidateFindingMethod,
}

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
    pub lookback_us: u64,
    pub stable_frames: usize,
    pub max_spatial_extent_px: f64,
    pub min_isotropy: f64,
    pub threshold_factor: f64,
    pub fit_radius_px: usize,
    pub max_candidates: usize,
    pub show_overlay: bool,
    pub show_boundaries: bool,
    pub show_provisional: bool,
}

impl Default for CandidateSettings {
    fn default() -> Self {
        Self {
            finding_method: CandidateFindingMethod::Dbscan,
            polarity: PolarityMode::Both,
            epsilon_px: 3.0,
            min_events: 5,
            lookback_us: 66_000,
            stable_frames: 2,
            max_spatial_extent_px: 5.0,
            min_isotropy: 0.2,
            threshold_factor: 1.5,
            fit_radius_px: 4,
            max_candidates: 512,
            show_overlay: true,
            show_boundaries: true,
            show_provisional: true,
        }
    }
}

pub struct EveSmlmCandidatePlugin {
    enabled: bool,
    settings: CandidateSettings,
    current_event_datasets: CandidateEventDatasets,
    last_candidate_count: usize,
    last_complete_visible_count: usize,
    last_provisional_count: usize,
    last_event_count: usize,
    last_status: String,
    dataset_generation: u64,
    findings: Vec<CandidateFinding>,
    findings_generation: u64,
    frame_counter: u64,
    next_cluster_id: u64,
    tracked_clusters: Vec<TrackedCluster>,
    event_buffer: Vec<FfiCdEvent>,
}

impl Default for EveSmlmCandidatePlugin {
    fn default() -> Self {
        Self {
            enabled: false,
            settings: CandidateSettings::default(),
            current_event_datasets: CandidateEventDatasets::default(),
            last_candidate_count: 0,
            last_complete_visible_count: 0,
            last_provisional_count: 0,
            last_event_count: 0,
            last_status: "Enable the plugin to cluster raw eveSMLM events into candidates.".into(),
            dataset_generation: 0,
            findings: Vec::new(),
            findings_generation: 0,
            frame_counter: 0,
            next_cluster_id: 0,
            tracked_clusters: Vec::new(),
            event_buffer: Vec::new(),
        }
    }
}

impl EveSmlmCandidatePlugin {
    fn reset_tracking_state(&mut self) {
        self.frame_counter = 0;
        self.next_cluster_id = 0;
        self.tracked_clusters.clear();
        self.event_buffer.clear();
        self.findings.clear();
        self.findings_generation = self.findings_generation.wrapping_add(1);
    }

    fn append_findings(&mut self, clusters: &[EveCluster]) {
        if clusters.is_empty() {
            return;
        }

        let method = self.settings.finding_method;
        self.findings.extend(
            clusters
                .iter()
                .cloned()
                .map(|cluster| CandidateFinding { cluster, method }),
        );
        self.findings_generation = self.findings_generation.wrapping_add(1);
    }

    fn collect_analysis_events(
        &mut self,
        frame: &PluginFrame<'_>,
        event_store: &EventStoreHandle<'_>,
    ) -> (Vec<FfiCdEvent>, u64, u64, bool) {
        let mut analysis_events = std::mem::take(&mut self.event_buffer);
        let mut analysis_window_start = frame.window_start_us();
        let analysis_window_end = frame.window_end_us();
        let temporal_enabled = self.settings.lookback_us > 0 && event_store.frame_count() > 0;

        analysis_events.clear();
        if temporal_enabled {
            let buffered_start = analysis_window_end.saturating_sub(self.settings.lookback_us);
            analysis_window_start = buffered_start.max(event_store.oldest_timestamp_us());
            event_store.collect_events_in_range(
                analysis_window_start,
                analysis_window_end,
                &mut analysis_events,
            );
        }

        if analysis_events.is_empty() {
            analysis_events.extend_from_slice(frame.events());
            analysis_window_start = frame.window_start_us();
        }

        (
            analysis_events,
            analysis_window_start,
            analysis_window_end,
            temporal_enabled,
        )
    }

    fn analyze_frame(
        &mut self,
        frame: &PluginFrame<'_>,
        raw_events: &[FfiCdEvent],
        analysis_window_start_us: u64,
        analysis_window_end_us: u64,
        temporal_enabled: bool,
        output: &mut HostOutput<'_>,
    ) -> EveCandidates {
        if raw_events.is_empty() {
            self.current_event_datasets = CandidateEventDatasets {
                sensor_dims: Some((frame.width(), frame.height())),
                frame_window_start_us: analysis_window_start_us,
                frame_window_end_us: analysis_window_end_us,
                ..CandidateEventDatasets::default()
            };
            self.last_candidate_count = 0;
            self.last_complete_visible_count = 0;
            self.last_provisional_count = 0;
            self.last_event_count = 0;
            self.last_status = if temporal_enabled {
                "No retained raw events are available in the requested temporal lookback.".into()
            } else {
                "Raw events are unavailable for this preview frame.".into()
            };
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
            self.current_event_datasets = CandidateEventDatasets {
                sensor_dims: Some((frame.width(), frame.height())),
                frame_window_start_us: analysis_window_start_us,
                frame_window_end_us: analysis_window_end_us,
                ..CandidateEventDatasets::default()
            };
            self.last_candidate_count = 0;
            self.last_complete_visible_count = 0;
            self.last_provisional_count = 0;
            self.last_status = "No events passed the configured polarity filter.".into();
            return EveCandidates {
                clusters: Vec::new(),
                frame_window_start_us: analysis_window_start_us,
                frame_window_end_us: analysis_window_end_us,
                n_events_processed: 0,
                finding_method: self.settings.finding_method,
            };
        }

        let mut cluster_indices = match self.settings.finding_method {
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

        cluster_indices.sort_by_key(|indices| std::cmp::Reverse(indices.len()));
        if cluster_indices.len() > self.settings.max_candidates {
            cluster_indices.truncate(self.settings.max_candidates);
        }

        let detected_clusters = clusters_from_indices(
            &filtered_events,
            cluster_indices.clone(),
            self.settings.finding_method,
        );
        let (visible_clusters, published_clusters) =
            self.update_tracked_clusters(detected_clusters, temporal_enabled);
        self.append_findings(&published_clusters);

        self.current_event_datasets = build_candidate_event_datasets(
            (frame.width(), frame.height()),
            analysis_window_start_us,
            analysis_window_end_us,
            &filtered_events,
            &cluster_indices,
            &visible_clusters,
        );

        self.last_candidate_count = published_clusters.len();
        self.last_complete_visible_count = visible_clusters
            .iter()
            .filter(|cluster| cluster.complete)
            .count();
        self.last_provisional_count = visible_clusters
            .len()
            .saturating_sub(self.last_complete_visible_count);

        let window_span_us = analysis_window_end_us.saturating_sub(analysis_window_start_us);
        let boundary_summary = if self.settings.show_boundaries && !visible_clusters.is_empty() {
            format!(
                " Showing {} for {} visible clusters.",
                boundary_label(self.settings.finding_method),
                visible_clusters.len()
            )
        } else {
            String::new()
        };
        self.last_status = if temporal_enabled {
            format!(
                "{} published, {} complete visible, {} provisional from {} events using {} over {} us.{}",
                self.last_candidate_count,
                self.last_complete_visible_count,
                self.last_provisional_count,
                self.last_event_count,
                self.settings.finding_method.label(),
                window_span_us,
                boundary_summary
            )
        } else {
            format!(
                "{} published from {} events using {} in the current frame.{}",
                self.last_candidate_count,
                self.last_event_count,
                self.settings.finding_method.label(),
                boundary_summary
            )
        };

        self.render_cluster_overlay(frame, &visible_clusters, output);

        EveCandidates {
            clusters: published_clusters,
            frame_window_start_us: analysis_window_start_us,
            frame_window_end_us: analysis_window_end_us,
            n_events_processed: filtered_events.len(),
            finding_method: self.settings.finding_method,
        }
    }

    fn update_tracked_clusters(
        &mut self,
        mut detected_clusters: Vec<EveCluster>,
        temporal_enabled: bool,
    ) -> (Vec<EveCluster>, Vec<EveCluster>) {
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let stable_frames = self.settings.stable_frames.max(1);
        let retention_frames = stable_frames.saturating_mul(2).max(1);
        let matching_radius = self.settings.epsilon_px.max(0.5);

        let mut candidate_pairs = Vec::new();
        for (detected_index, cluster) in detected_clusters.iter().enumerate() {
            for (tracked_index, tracked) in self.tracked_clusters.iter().enumerate() {
                let dx = cluster.centroid_x - tracked.centroid_x;
                let dy = cluster.centroid_y - tracked.centroid_y;
                let distance = (dx * dx + dy * dy).sqrt();
                if distance <= matching_radius {
                    candidate_pairs.push((distance, detected_index, tracked_index));
                }
            }
        }
        candidate_pairs.sort_by(|left, right| left.0.total_cmp(&right.0));

        let mut detected_to_tracked = vec![None; detected_clusters.len()];
        let mut tracked_taken = vec![false; self.tracked_clusters.len()];
        for (_, detected_index, tracked_index) in candidate_pairs {
            if detected_to_tracked[detected_index].is_none() && !tracked_taken[tracked_index] {
                detected_to_tracked[detected_index] = Some(tracked_index);
                tracked_taken[tracked_index] = true;
            }
        }

        for (detected_index, cluster) in detected_clusters.iter_mut().enumerate() {
            if let Some(tracked_index) = detected_to_tracked[detected_index] {
                let tracked = &mut self.tracked_clusters[tracked_index];
                let current_count = cluster.event_count();
                tracked.centroid_x = cluster.centroid_x;
                tracked.centroid_y = cluster.centroid_y;
                tracked.last_seen_frame = self.frame_counter;

                if current_count > tracked.event_count {
                    tracked.event_count = current_count;
                    tracked.last_grown_frame = self.frame_counter;
                    tracked.frames_since_growth = 0;
                    tracked.cluster = cluster.clone();
                    if temporal_enabled {
                        tracked.complete = false;
                    }
                } else {
                    tracked.frames_since_growth = tracked.frames_since_growth.saturating_add(1);
                    if current_count == tracked.event_count {
                        tracked.cluster = cluster.clone();
                    }
                }

                if !temporal_enabled || tracked.frames_since_growth >= stable_frames {
                    tracked.complete = true;
                }

                tracked.cluster.cluster_id = tracked.id;
                tracked.cluster.complete = tracked.complete;
                cluster.cluster_id = tracked.id;
                cluster.complete = tracked.complete;
            } else {
                let cluster_id = self.next_cluster_id;
                self.next_cluster_id = self.next_cluster_id.wrapping_add(1);
                cluster.cluster_id = cluster_id;
                cluster.complete = !temporal_enabled;
                self.tracked_clusters.push(TrackedCluster {
                    id: cluster_id,
                    centroid_x: cluster.centroid_x,
                    centroid_y: cluster.centroid_y,
                    event_count: cluster.event_count(),
                    last_seen_frame: self.frame_counter,
                    last_grown_frame: self.frame_counter,
                    frames_since_growth: 0,
                    complete: cluster.complete,
                    emitted: false,
                    cluster: cluster.clone(),
                });
            }
        }

        for tracked in &mut self.tracked_clusters {
            if tracked.last_seen_frame != self.frame_counter {
                tracked.frames_since_growth = tracked.frames_since_growth.saturating_add(1);
                if temporal_enabled && tracked.frames_since_growth >= stable_frames {
                    tracked.complete = true;
                }
            }
            if !temporal_enabled {
                tracked.complete = true;
            }
            tracked.cluster.cluster_id = tracked.id;
            tracked.cluster.complete = tracked.complete;
        }

        let mut published_clusters = Vec::new();
        for tracked in &mut self.tracked_clusters {
            if tracked.complete && !tracked.emitted {
                tracked.emitted = true;
                let mut cluster = tracked.cluster.clone();
                cluster.cluster_id = tracked.id;
                cluster.complete = true;
                published_clusters.push(cluster);
            }
        }

        self.tracked_clusters.retain(|tracked| {
            self.frame_counter.saturating_sub(tracked.last_seen_frame) as usize <= retention_frames
        });

        (detected_clusters, published_clusters)
    }

    fn render_cluster_overlay(
        &self,
        frame: &PluginFrame<'_>,
        visible_clusters: &[EveCluster],
        output: &mut HostOutput<'_>,
    ) {
        let overlay_clusters: Vec<&EveCluster> = visible_clusters
            .iter()
            .filter(|cluster| cluster.complete || self.settings.show_provisional)
            .collect();

        if self.settings.show_boundaries && !overlay_clusters.is_empty() {
            let (complete_pixels, provisional_pixels) =
                boundary_pixels(&overlay_clusters, frame.width(), frame.height());
            if !complete_pixels.is_empty() {
                output.add_highlight_pixels(&complete_pixels, COMPLETE_BOUNDARY_COLOR);
            }
            if !provisional_pixels.is_empty() {
                output.add_highlight_pixels(&provisional_pixels, PROVISIONAL_BOUNDARY_COLOR);
            }
        }

        if self.settings.show_overlay && !overlay_clusters.is_empty() {
            let stable_ids: Vec<String> = overlay_clusters
                .iter()
                .map(|cluster| cluster.cluster_id.to_string())
                .collect();
            let markers: Vec<FfiMarkerOverlayItem> = overlay_clusters
                .iter()
                .zip(stable_ids.iter())
                .map(|(cluster, stable_id)| FfiMarkerOverlayItem {
                    x: cluster.centroid_x as f32,
                    y: cluster.centroid_y as f32,
                    shape: FfiMarkerShape::FilledCircle,
                    size: 4.0,
                    color: FfiColorRgba::from_rgba(if cluster.complete {
                        COMPLETE_MARKER_COLOR
                    } else {
                        PROVISIONAL_MARKER_COLOR
                    }),
                    timestamp_us: cluster
                        .events
                        .last()
                        .map(|event| event.timestamp)
                        .unwrap_or(frame.window_end_us()),
                    has_timestamp: !cluster.events.is_empty(),
                    stable_id: stable_id.as_str().into(),
                    source_dataset_id: FfiString::empty(),
                    source_row_id: FfiString::empty(),
                })
                .collect();
            output.add_marker_overlay(
                &markers,
                Some(ACCEPTED_EVENTS_DATASET_ID),
                Some(ACCEPTED_EVENTS_LAYER_ID),
                Some(self.name()),
            );
        }
    }

    pub fn reset(&mut self) {
        self.reset_tracking_state();
        self.current_event_datasets = CandidateEventDatasets::default();
        self.last_candidate_count = 0;
        self.last_complete_visible_count = 0;
        self.last_provisional_count = 0;
        self.last_event_count = 0;
        self.last_status = "Waiting for the next preview frame.".into();
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
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
        event_store: &EventStoreHandle<'_>,
    ) {
        let (analysis_events, analysis_window_start_us, analysis_window_end_us, temporal_enabled) =
            self.collect_analysis_events(frame, event_store);
        let candidates = self.analyze_frame(
            frame,
            &analysis_events,
            analysis_window_start_us,
            analysis_window_end_us,
            temporal_enabled,
            output,
        );
        self.event_buffer = analysis_events;
        self.dataset_generation = self.dataset_generation.wrapping_add(1);
        if let Err(err) = context.publish(CTX_EVE_CANDIDATES, &candidates) {
            Self::warning(
                output,
                AnalysisSeverity::Warning,
                &format!("Publishing EVE candidates failed: {err}"),
            );
        }
    }

    fn capabilities(&self) -> PluginCapabilities {
        PluginCapabilities {
            retained_event_history: self.settings.lookback_us > 0,
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
                    label: "Temporal aggregation".into(),
                    description: Some(
                        "Optionally cluster across retained event history and only publish clusters once they stop growing."
                            .into(),
                    ),
                    default_open: false,
                    items: vec![
                        SettingItem {
                            key: "lookback_us".into(),
                            label: "Lookback".into(),
                            tooltip: Some(
                                "How far back in retained event history to gather events before clustering. Set to 0 for single-frame behavior."
                                    .into(),
                            ),
                            kind: SettingKind::I64Slider {
                                min: 0,
                                max: 500_000,
                                default: i64::try_from(self.settings.lookback_us).unwrap_or(66_000),
                                suffix: Some(" us".into()),
                            },
                        },
                        SettingItem {
                            key: "stable_frames".into(),
                            label: "Stable frames".into(),
                            tooltip: Some(
                                "How many consecutive frames without cluster growth are required before a cluster is published to fitting."
                                    .into(),
                            ),
                            kind: SettingKind::I64Slider {
                                min: 1,
                                max: 8,
                                default: i64::try_from(self.settings.stable_frames).unwrap_or(2),
                                suffix: Some(" frames".into()),
                            },
                        },
                        SettingItem {
                            key: "show_provisional".into(),
                            label: "Show provisional".into(),
                            tooltip: Some(
                                "Show still-growing clusters in the preview overlay and boundary layer."
                                    .into(),
                            ),
                            kind: SettingKind::Bool {
                                default: self.settings.show_provisional,
                            },
                        },
                    ],
                },
                SettingsSection {
                    label: "Refinement".into(),
                    description: Some(
                        "Frame-based mode, eigenfeature filtering, and preview overlays use these thresholds and display controls."
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
                            label: "Show centroids".into(),
                            tooltip: Some(
                                "Draw clickable centroid markers that link into the accepted candidate-events dataset."
                                    .into(),
                            ),
                            kind: SettingKind::Bool {
                                default: self.settings.show_overlay,
                            },
                        },
                        SettingItem {
                            key: "show_boundaries".into(),
                            label: "Show boundaries".into(),
                            tooltip: Some(
                                "Draw 2-sigma eigenfeature ellipses or bounding boxes around visible clusters."
                                    .into(),
                            ),
                            kind: SettingKind::Bool {
                                default: self.settings.show_boundaries,
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
            "lookback_us" => Some(json!(self.settings.lookback_us)),
            "stable_frames" => Some(json!(self.settings.stable_frames)),
            "max_spatial_extent_px" => Some(json!(self.settings.max_spatial_extent_px)),
            "min_isotropy" => Some(json!(self.settings.min_isotropy)),
            "threshold_factor" => Some(json!(self.settings.threshold_factor)),
            "fit_radius_px" => Some(json!(self.settings.fit_radius_px)),
            "max_candidates" => Some(json!(self.settings.max_candidates)),
            "show_overlay" => Some(json!(self.settings.show_overlay)),
            "show_boundaries" => Some(json!(self.settings.show_boundaries)),
            "show_provisional" => Some(json!(self.settings.show_provisional)),
            _ => None,
        }
    }

    fn set_setting(&mut self, key: &str, value: Value) -> Result<(), String> {
        let mut reset_tracking = false;
        match key {
            "finding_method" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("finding_method must be an integer".into());
                };
                self.settings.finding_method = CandidateFindingMethod::from_index(value);
                reset_tracking = true;
            }
            "polarity" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("polarity must be an integer".into());
                };
                self.settings.polarity = PolarityMode::from_index(value);
                reset_tracking = true;
            }
            "epsilon_px" => {
                let Some(value) = value.as_f64() else {
                    return Err("epsilon_px must be numeric".into());
                };
                self.settings.epsilon_px = value.clamp(1.0, 10.0);
                reset_tracking = true;
            }
            "min_events" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("min_events must be an integer".into());
                };
                self.settings.min_events = value.clamp(1, 64);
                reset_tracking = true;
            }
            "lookback_us" => {
                let Some(value) = value.as_u64() else {
                    return Err("lookback_us must be an integer".into());
                };
                self.settings.lookback_us = value.min(500_000);
                reset_tracking = true;
            }
            "stable_frames" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("stable_frames must be an integer".into());
                };
                self.settings.stable_frames = value.clamp(1, 8);
                reset_tracking = true;
            }
            "max_spatial_extent_px" => {
                let Some(value) = value.as_f64() else {
                    return Err("max_spatial_extent_px must be numeric".into());
                };
                self.settings.max_spatial_extent_px = value.clamp(1.0, 20.0);
                reset_tracking = true;
            }
            "min_isotropy" => {
                let Some(value) = value.as_f64() else {
                    return Err("min_isotropy must be numeric".into());
                };
                self.settings.min_isotropy = value.clamp(0.0, 1.0);
                reset_tracking = true;
            }
            "threshold_factor" => {
                let Some(value) = value.as_f64() else {
                    return Err("threshold_factor must be numeric".into());
                };
                self.settings.threshold_factor = value.clamp(0.5, 6.0);
                reset_tracking = true;
            }
            "fit_radius_px" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("fit_radius_px must be an integer".into());
                };
                self.settings.fit_radius_px = value.clamp(1, 16);
                reset_tracking = true;
            }
            "max_candidates" => {
                let Some(value) = Self::parse_usize(value) else {
                    return Err("max_candidates must be an integer".into());
                };
                self.settings.max_candidates = value.clamp(1, 2048);
                reset_tracking = true;
            }
            "show_overlay" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_overlay must be a boolean".into());
                };
                self.settings.show_overlay = value;
            }
            "show_boundaries" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_boundaries must be a boolean".into());
                };
                self.settings.show_boundaries = value;
            }
            "show_provisional" => {
                let Some(value) = value.as_bool() else {
                    return Err("show_provisional must be a boolean".into());
                };
                self.settings.show_provisional = value;
            }
            _ => return Err(format!("unknown setting: {key}")),
        }

        if reset_tracking {
            self.reset_tracking_state();
        }

        Ok(())
    }

    fn status_entries(&self) -> Vec<StatusEntry> {
        let mut entries = vec![
            StatusEntry::Text(self.last_status.clone()),
            StatusEntry::LabeledValue {
                label: "Events".into(),
                value: self.last_event_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Published".into(),
                value: self.last_candidate_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Complete".into(),
                value: self.last_complete_visible_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Provisional".into(),
                value: self.last_provisional_count.to_string(),
                color: None,
            },
            StatusEntry::LabeledValue {
                label: "Method".into(),
                value: self.settings.finding_method.label().into(),
                color: None,
            },
        ];
        if self.settings.lookback_us > 0 {
            entries.push(StatusEntry::LabeledValue {
                label: "Lookback".into(),
                value: format!("{} us", self.settings.lookback_us),
                color: None,
            });
        }
        entries
    }

    fn host_views(&self) -> HostViewRegistry {
        candidate_event_registry(&self.current_event_datasets)
    }

    fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
        let dataset = match dataset_id {
            ACCEPTED_EVENTS_DATASET_ID => {
                candidate_events_dataset(&self.current_event_datasets.accepted)
            }
            REJECTED_EVENTS_DATASET_ID => {
                candidate_events_dataset(&self.current_event_datasets.rejected)
            }
            _ => return None,
        };
        serde_json::to_vec(&dataset).ok()
    }

    fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
        match dataset_id {
            ACCEPTED_EVENTS_DATASET_ID | REJECTED_EVENTS_DATASET_ID => self.dataset_generation,
            _ => 0,
        }
    }
}

fn boundary_label(method: CandidateFindingMethod) -> &'static str {
    match method {
        CandidateFindingMethod::FrameBased => "bounding boxes",
        CandidateFindingMethod::Dbscan | CandidateFindingMethod::Eigenfeature => {
            "2-sigma eigenfeature ellipses"
        }
    }
}

fn boundary_pixels(
    clusters: &[&EveCluster],
    width: u16,
    height: u16,
) -> (Vec<FfiPixel>, Vec<FfiPixel>) {
    let mut complete = HashSet::new();
    let mut provisional = HashSet::new();

    for cluster in clusters {
        let target = if cluster.complete {
            &mut complete
        } else {
            &mut provisional
        };
        rasterize_cluster_boundary(
            cluster.boundary.as_ref(),
            width,
            height,
            !cluster.complete,
            target,
        );
    }

    let to_pixels = |points: HashSet<(u16, u16)>| {
        let mut pixels: Vec<_> = points.into_iter().map(|(x, y)| FfiPixel { x, y }).collect();
        pixels.sort_by_key(|pixel| (pixel.y, pixel.x));
        pixels
    };

    (to_pixels(complete), to_pixels(provisional))
}

fn rasterize_cluster_boundary(
    boundary: Option<&ClusterBoundary>,
    width: u16,
    height: u16,
    dashed: bool,
    out: &mut HashSet<(u16, u16)>,
) {
    let Some(boundary) = boundary else {
        return;
    };

    match boundary {
        ClusterBoundary::BoundingBox {
            x_min,
            x_max,
            y_min,
            y_max,
        } => {
            for (step, x) in (*x_min..=*x_max).enumerate() {
                if !dashed || step % 2 == 0 {
                    push_boundary_pixel(out, width, height, x as f64, f64::from(*y_min));
                    push_boundary_pixel(out, width, height, x as f64, f64::from(*y_max));
                }
            }
            for (step, y) in (*y_min..=*y_max).enumerate() {
                if !dashed || step % 2 == 0 {
                    push_boundary_pixel(out, width, height, f64::from(*x_min), y as f64);
                    push_boundary_pixel(out, width, height, f64::from(*x_max), y as f64);
                }
            }
        }
        ClusterBoundary::Ellipse {
            cx,
            cy,
            semi_major,
            semi_minor,
            angle_rad,
        } => {
            let steps = ((semi_major.max(*semi_minor) * 10.0).ceil() as usize).clamp(24, 240);
            let cos_angle = angle_rad.cos();
            let sin_angle = angle_rad.sin();
            for step in 0..=steps {
                if dashed && step % 2 == 1 {
                    continue;
                }
                let theta = std::f64::consts::TAU * step as f64 / steps as f64;
                let ellipse_x = semi_major * theta.cos();
                let ellipse_y = semi_minor * theta.sin();
                let rotated_x = ellipse_x * cos_angle - ellipse_y * sin_angle;
                let rotated_y = ellipse_x * sin_angle + ellipse_y * cos_angle;
                push_boundary_pixel(out, width, height, cx + rotated_x, cy + rotated_y);
            }
        }
    }
}

fn push_boundary_pixel(out: &mut HashSet<(u16, u16)>, width: u16, height: u16, x: f64, y: f64) {
    let x = x.round();
    let y = y.round();
    if x < 0.0 || y < 0.0 {
        return;
    }

    let x = x as u16;
    let y = y as u16;
    if x < width && y < height {
        out.insert((x, y));
    }
}

fn candidate_event_registry(datasets: &CandidateEventDatasets) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![
            HostDatasetDescriptor {
                id: ACCEPTED_EVENTS_DATASET_ID.into(),
                title: "Accepted EVE events".into(),
                kind: HostDatasetKind::TableV1(candidate_events_schema(
                    datasets,
                    ACCEPTED_EVENTS_LAYER_ID,
                    "accepted candidate events",
                    "cluster_id",
                )),
                empty_message: "No accepted candidate events in the current analysis window."
                    .into(),
                display: Some(candidate_event_display_metadata(
                    "Accepted candidate events",
                    ACCEPTED_EVENTS_COLOR,
                )),
                relations: vec![HostDatasetRelation {
                    target_dataset_id: "augur.evesmlm.current_localizations".into(),
                    via_column: "cluster_id".into(),
                    target_column: "cluster_id".into(),
                }],
            },
            HostDatasetDescriptor {
                id: REJECTED_EVENTS_DATASET_ID.into(),
                title: "Rejected EVE events".into(),
                kind: HostDatasetKind::TableV1(candidate_events_schema(
                    datasets,
                    REJECTED_EVENTS_LAYER_ID,
                    "rejected candidate events",
                    "event_id",
                )),
                empty_message: "No rejected candidate events in the current analysis window."
                    .into(),
                display: Some(candidate_event_display_metadata(
                    "Rejected candidate events",
                    REJECTED_EVENTS_COLOR,
                )),
                relations: Vec::new(),
            },
        ],
        views: vec![
            HostViewDescriptor {
                id: ACCEPTED_EVENTS_COMPACT_VIEW_ID.into(),
                title: "Accepted Events".into(),
                dataset_id: ACCEPTED_EVENTS_DATASET_ID.into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            },
            HostViewDescriptor {
                id: REJECTED_EVENTS_COMPACT_VIEW_ID.into(),
                title: "Rejected Events".into(),
                dataset_id: REJECTED_EVENTS_DATASET_ID.into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            },
            HostViewDescriptor {
                id: ACCEPTED_EVENTS_TABLE_VIEW_ID.into(),
                title: "Accepted Events".into(),
                dataset_id: ACCEPTED_EVENTS_DATASET_ID.into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::TableWindow,
            },
            HostViewDescriptor {
                id: REJECTED_EVENTS_TABLE_VIEW_ID.into(),
                title: "Rejected Events".into(),
                dataset_id: REJECTED_EVENTS_DATASET_ID.into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::TableWindow,
            },
            HostViewDescriptor {
                id: ACCEPTED_EVENTS_3D_VIEW_ID.into(),
                title: "Accepted Events 3D".into(),
                dataset_id: ACCEPTED_EVENTS_DATASET_ID.into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::Scatter3dFromTable {
                    x_column: "x_px".into(),
                    y_column: "y_px".into(),
                    z_column: "timestamp_us".into(),
                },
            },
            HostViewDescriptor {
                id: REJECTED_EVENTS_3D_VIEW_ID.into(),
                title: "Rejected Events 3D".into(),
                dataset_id: REJECTED_EVENTS_DATASET_ID.into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::Scatter3dFromTable {
                    x_column: "x_px".into(),
                    y_column: "y_px".into(),
                    z_column: "timestamp_us".into(),
                },
            },
        ],
        actions: Vec::new(),
    }
}

fn candidate_event_display_metadata(
    layer_title: &str,
    color: [u8; 4],
) -> HostDatasetDisplayMetadata {
    HostDatasetDisplayMetadata {
        layer_title: Some(layer_title.into()),
        default_visibility: Some(true),
        default_color: Some(color),
        default_marker_shape: Some(HostMarkerShape::Point),
        default_size: Some(2.5),
    }
}

fn candidate_events_schema(
    datasets: &CandidateEventDatasets,
    layer_id: &str,
    semantic_label: &str,
    row_id_column: &str,
) -> TableSchema {
    TableSchema {
        columns: vec![
            TableColumn {
                id: "event_id".into(),
                title: "Event ID".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "timestamp_us".into(),
                title: "Timestamp (us)".into(),
                value_type: TableValueType::U64,
            },
            TableColumn {
                id: "x_px".into(),
                title: "X (px)".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "y_px".into(),
                title: "Y (px)".into(),
                value_type: TableValueType::F64,
            },
            TableColumn {
                id: "polarity".into(),
                title: "Polarity".into(),
                value_type: TableValueType::Bool,
            },
            TableColumn {
                id: "cluster_id".into(),
                title: "Cluster".into(),
                value_type: TableValueType::String,
            },
        ],
        coordinate_space_2d: datasets
            .sensor_dims
            .map(|(width, height)| TableCoordinateSpace2d {
                x_column: "x_px".into(),
                y_column: "y_px".into(),
                x_min: 0.0,
                x_max: f64::from(width),
                y_min: 0.0,
                y_max: f64::from(height),
            }),
        coordinate_space_3d: datasets
            .sensor_dims
            .map(|(width, height)| TableCoordinateSpace3d {
                x_column: "x_px".into(),
                y_column: "y_px".into(),
                z_column: "timestamp_us".into(),
                x_min: 0.0,
                x_max: f64::from(width),
                y_min: 0.0,
                y_max: f64::from(height),
                z_min: datasets.frame_window_start_us as f64,
                z_max: datasets
                    .frame_window_end_us
                    .max(datasets.frame_window_start_us) as f64,
            }),
        row_id_column: Some(row_id_column.into()),
        time_column: Some("timestamp_us".into()),
        layer_id: Some(layer_id.into()),
        semantic_label: Some(semantic_label.into()),
        provenance: Some(TableRowProvenance {
            anchor_time_column: Some("timestamp_us".into()),
            span_start_column: Some("timestamp_us".into()),
            span_end_column: Some("timestamp_us".into()),
            anchor_frame_column: None,
        }),
        column_display: vec![
            TableColumnDisplayEntry {
                column_id: "event_id".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Identifier),
                    width_priority: Some(TableColumnWidthPriority::Low),
                    hide_in_compact: true,
                    label: None,
                    headline: false,
                },
            },
            TableColumnDisplayEntry {
                column_id: "timestamp_us".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::TimestampMicros),
                    width_priority: Some(TableColumnWidthPriority::Medium),
                    hide_in_compact: false,
                    label: Some("Time".into()),
                    headline: false,
                },
            },
            TableColumnDisplayEntry {
                column_id: "x_px".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 1 }),
                    width_priority: Some(TableColumnWidthPriority::Low),
                    hide_in_compact: false,
                    label: Some("X".into()),
                    headline: false,
                },
            },
            TableColumnDisplayEntry {
                column_id: "y_px".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::FixedPrecision { digits: 1 }),
                    width_priority: Some(TableColumnWidthPriority::Low),
                    hide_in_compact: false,
                    label: Some("Y".into()),
                    headline: false,
                },
            },
            TableColumnDisplayEntry {
                column_id: "polarity".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Category),
                    width_priority: Some(TableColumnWidthPriority::Low),
                    hide_in_compact: false,
                    label: Some("Polarity".into()),
                    headline: false,
                },
            },
            TableColumnDisplayEntry {
                column_id: "cluster_id".into(),
                display: TableColumnDisplayMetadata {
                    format: Some(TableColumnDisplayFormat::Category),
                    width_priority: Some(TableColumnWidthPriority::Medium),
                    hide_in_compact: false,
                    label: Some("Cluster".into()),
                    headline: row_id_column == "cluster_id",
                },
            },
        ],
    }
}

fn candidate_events_dataset(rows: &[CandidateEventRow]) -> TableDatasetV1 {
    TableDatasetV1::new(vec![
        TableColumnData {
            column_id: "event_id".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.event_id).collect()),
        },
        TableColumnData {
            column_id: "timestamp_us".into(),
            values: TableColumnValues::U64(rows.iter().map(|row| row.timestamp_us).collect()),
        },
        TableColumnData {
            column_id: "x_px".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.x_px).collect()),
        },
        TableColumnData {
            column_id: "y_px".into(),
            values: TableColumnValues::F64(rows.iter().map(|row| row.y_px).collect()),
        },
        TableColumnData {
            column_id: "polarity".into(),
            values: TableColumnValues::Bool(rows.iter().map(|row| row.polarity).collect()),
        },
        TableColumnData {
            column_id: "cluster_id".into(),
            values: TableColumnValues::String(
                rows.iter().map(|row| row.cluster_id.clone()).collect(),
            ),
        },
    ])
    .expect("candidate event columns must stay aligned")
}

fn candidate_event_row_id(event: &EveEvent, occurrence: u32) -> u64 {
    event.timestamp
        ^ u64::from(event.x).rotate_left(11)
        ^ u64::from(event.y).rotate_left(23)
        ^ u64::from(event.polarity as u8).rotate_left(37)
        ^ u64::from(occurrence).rotate_left(47)
}

fn build_candidate_event_datasets(
    sensor_dims: (u16, u16),
    frame_window_start_us: u64,
    frame_window_end_us: u64,
    events: &[EveEvent],
    cluster_indices: &[Vec<usize>],
    visible_clusters: &[EveCluster],
) -> CandidateEventDatasets {
    let mut cluster_by_event = vec![None; events.len()];
    for (cluster, indices) in visible_clusters.iter().zip(cluster_indices.iter()) {
        for &event_index in indices {
            if let Some(slot) = cluster_by_event.get_mut(event_index) {
                *slot = Some(cluster.cluster_id.to_string());
            }
        }
    }

    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    let mut seen_occurrences = HashMap::new();
    for (event_index, event) in events.iter().enumerate() {
        let occurrence = seen_occurrences
            .entry((event.timestamp, event.x, event.y, event.polarity))
            .or_insert(0u32);
        let row = CandidateEventRow {
            event_id: candidate_event_row_id(event, *occurrence),
            x_px: f64::from(event.x),
            y_px: f64::from(event.y),
            timestamp_us: event.timestamp,
            polarity: event.polarity,
            cluster_id: cluster_by_event[event_index].clone().unwrap_or_default(),
        };
        *occurrence = occurrence.saturating_add(1);
        if cluster_by_event[event_index].is_some() {
            accepted.push(row);
        } else {
            rejected.push(row);
        }
    }

    CandidateEventDatasets {
        accepted,
        rejected,
        sensor_dims: Some(sensor_dims),
        frame_window_start_us,
        frame_window_end_us,
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

#[allow(clippy::too_many_arguments)]
fn cluster_boundary_for_indices(
    events: &[EveEvent],
    indices: &[usize],
    centroid_x: f64,
    centroid_y: f64,
    x_min: u16,
    x_max: u16,
    y_min: u16,
    y_max: u16,
    method: CandidateFindingMethod,
) -> ClusterBoundary {
    if matches!(
        method,
        CandidateFindingMethod::Dbscan | CandidateFindingMethod::Eigenfeature
    ) {
        if let Some(info) = eigenfeature::cluster_eigen_info(events, indices) {
            return ClusterBoundary::Ellipse {
                cx: centroid_x,
                cy: centroid_y,
                semi_major: (2.0 * info.lambda_1.max(0.0).sqrt()).max(1.0),
                semi_minor: (2.0 * info.lambda_2.max(0.0).sqrt()).max(1.0),
                angle_rad: info.angle_rad,
            };
        }
    }

    ClusterBoundary::BoundingBox {
        x_min,
        x_max,
        y_min,
        y_max,
    }
}

fn clusters_from_indices(
    events: &[EveEvent],
    cluster_indices: Vec<Vec<usize>>,
    method: CandidateFindingMethod,
) -> Vec<EveCluster> {
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

            for &index in &indices {
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

            let centroid_x = sum_x / count;
            let centroid_y = sum_y / count;
            let boundary = cluster_boundary_for_indices(
                events, &indices, centroid_x, centroid_y, x_min, x_max, y_min, y_max, method,
            );

            Some(EveCluster {
                cluster_id: 0,
                pixel_histogram: histogram_entries,
                events: cluster_events,
                centroid_x,
                centroid_y,
                x_min,
                x_max,
                y_min,
                y_max,
                complete: false,
                boundary: Some(boundary),
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

        let clusters = clusters_from_indices(
            &events,
            vec![vec![0, 1, 2, 3]],
            CandidateFindingMethod::Dbscan,
        );
        assert_eq!(clusters.len(), 1);
        let cluster = &clusters[0];
        assert_eq!(cluster.event_count(), 4);
        assert_eq!(cluster.positive_event_count(), 3);
        assert_eq!(cluster.negative_event_count(), 1);
        assert_eq!(cluster.pixel_histogram.len(), 3);
        assert!((cluster.centroid_x - 10.25).abs() < 1e-6);
        assert!((cluster.centroid_y - 10.25).abs() < 1e-6);
        assert!(cluster.boundary.is_some());
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
        let index = 6usize + 2usize;
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

    #[test]
    fn candidate_event_datasets_split_accepted_and_rejected_events() {
        let events = vec![
            event(10, 10, true, 101),
            event(11, 10, true, 102),
            event(12, 10, false, 103),
            event(30, 20, true, 104),
        ];
        let visible_clusters = vec![EveCluster {
            cluster_id: 42,
            pixel_histogram: vec![(10, 10, 1, 0), (12, 10, 0, 1)],
            events: vec![events[0], events[2]],
            centroid_x: 11.0,
            centroid_y: 10.0,
            x_min: 10,
            x_max: 12,
            y_min: 10,
            y_max: 10,
            complete: false,
            boundary: Some(ClusterBoundary::BoundingBox {
                x_min: 10,
                x_max: 12,
                y_min: 10,
                y_max: 10,
            }),
        }];

        let datasets = build_candidate_event_datasets(
            (32, 24),
            100,
            101,
            &events,
            &[vec![0, 2]],
            &visible_clusters,
        );
        assert_eq!(datasets.accepted.len(), 2);
        assert_eq!(datasets.rejected.len(), 2);
        assert_eq!(datasets.accepted[0].cluster_id, "42");
        assert_eq!(datasets.rejected[0].cluster_id, "");
    }

    #[test]
    fn candidate_event_registry_exposes_table_and_3d_views() {
        let registry = candidate_event_registry(&CandidateEventDatasets {
            accepted: Vec::new(),
            rejected: Vec::new(),
            sensor_dims: Some((128, 64)),
            frame_window_start_us: 10,
            frame_window_end_us: 20,
        });
        assert_eq!(registry.datasets.len(), 2);
        assert_eq!(registry.views.len(), 6);
        assert_eq!(registry.views[0].id, ACCEPTED_EVENTS_COMPACT_VIEW_ID);
        assert_eq!(registry.views[0].title, "Accepted Events");
        assert!(matches!(registry.views[0].kind, HostViewKind::CompactTable));
        assert_eq!(registry.views[2].id, ACCEPTED_EVENTS_TABLE_VIEW_ID);
        assert_eq!(registry.views[2].title, "Accepted Events");
        assert!(matches!(registry.views[2].kind, HostViewKind::TableWindow));
        assert_eq!(registry.views[4].id, ACCEPTED_EVENTS_3D_VIEW_ID);
        let schema = match &registry.datasets[0].kind {
            HostDatasetKind::TableV1(schema) => schema,
            other => panic!("unexpected dataset kind: {other:?}"),
        };
        assert_eq!(schema.row_id_column.as_deref(), Some("cluster_id"));
        let cluster_column = schema.column("cluster_id").expect("cluster id column");
        assert_eq!(cluster_column.value_type, TableValueType::String);
        assert_eq!(
            schema
                .column_display("cluster_id")
                .map(|display| display.headline),
            Some(true)
        );
        assert_eq!(
            schema
                .coordinate_space_3d
                .as_ref()
                .map(|space| space.z_column.as_str()),
            Some("timestamp_us")
        );
        let rejected_schema = match &registry.datasets[1].kind {
            HostDatasetKind::TableV1(schema) => schema,
            other => panic!("unexpected dataset kind: {other:?}"),
        };
        assert_eq!(rejected_schema.row_id_column.as_deref(), Some("event_id"));
    }

    #[test]
    fn temporal_tracking_waits_for_stable_frames_before_publishing() {
        let mut plugin = EveSmlmCandidatePlugin::default();
        plugin.settings.stable_frames = 2;

        let make_cluster = || EveCluster {
            cluster_id: 0,
            pixel_histogram: vec![(10, 10, 3, 0), (11, 10, 2, 0)],
            events: vec![
                event(10, 10, true, 1),
                event(10, 10, true, 2),
                event(10, 10, true, 3),
                event(11, 10, true, 4),
                event(11, 10, true, 5),
            ],
            centroid_x: 10.4,
            centroid_y: 10.0,
            x_min: 10,
            x_max: 11,
            y_min: 10,
            y_max: 10,
            complete: false,
            boundary: Some(ClusterBoundary::BoundingBox {
                x_min: 10,
                x_max: 11,
                y_min: 10,
                y_max: 10,
            }),
        };

        let (visible, published) = plugin.update_tracked_clusters(vec![make_cluster()], true);
        assert_eq!(visible.len(), 1);
        assert!(!visible[0].complete);
        assert!(published.is_empty());

        let (visible, published) = plugin.update_tracked_clusters(vec![make_cluster()], true);
        assert_eq!(visible.len(), 1);
        assert!(!visible[0].complete);
        assert!(published.is_empty());

        let (visible, published) = plugin.update_tracked_clusters(vec![make_cluster()], true);
        assert_eq!(visible.len(), 1);
        assert!(visible[0].complete);
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].cluster_id, visible[0].cluster_id);
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
                external_triggers: augur_plugin_api::FfiSlice::default(),
                window_start_us: self.window_start_us,
                window_end_us: self.window_start_us + 1,
            }));
            PluginFrame::new(raw)
        }
    }
}

export_plugin!(EveSmlmCandidatePlugin);
