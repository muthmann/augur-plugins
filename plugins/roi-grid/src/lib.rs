//! ROI Grid Plugin
//!
//! Partitions the sensor around masked hotpixels and finds the largest
//! rectangular regions free of hotpixels. These become ROI candidates.
//!
//! Execution phase: FrameOnly
//! Dependencies: None
//! Published data: None (writes overlays and can mutate CameraConfig)

use std::sync::Arc;

use augur_core::{
    analysis::{
        roi_grid::{self, RoiGrid},
        AnalysisOutput, Overlay,
    },
    config::CameraConfig,
    pipeline::PreviewFrame,
};

const SENSOR_WIDTH: u16 = 1280;
const SENSOR_HEIGHT: u16 = 720;

pub struct RoiGridPlugin {
    enabled: bool,
    roi_grid: Option<Arc<RoiGrid>>,
    show_roi_grid: bool,
    roi_grid_top_n: usize,
    last_mask_snapshot: Vec<(u16, u16)>,
}

impl Default for RoiGridPlugin {
    fn default() -> Self {
        Self {
            enabled: true,
            roi_grid: None,
            show_roi_grid: false,
            roi_grid_top_n: 3,
            last_mask_snapshot: Vec::new(),
        }
    }
}

impl RoiGridPlugin {
    pub fn recompute_roi_grid(&mut self, config: &CameraConfig) {
        let grid = roi_grid::compute_roi_grid(
            &config.pixel_mask.masked_pixels,
            SENSOR_WIDTH,
            SENSOR_HEIGHT,
            self.roi_grid_top_n.max(1),
        );
        self.last_mask_snapshot = config.pixel_mask.masked_pixels.clone();
        self.roi_grid = Some(Arc::new(grid));
    }

    pub fn maybe_auto_recompute_roi_grid(&mut self, config: &CameraConfig) {
        if config.pixel_mask.masked_pixels == self.last_mask_snapshot {
            return;
        }
        if self.roi_grid.is_some() || self.show_roi_grid {
            self.recompute_roi_grid(config);
        } else {
            self.last_mask_snapshot = config.pixel_mask.masked_pixels.clone();
        }
    }

    pub fn name(&self) -> &str {
        "ROI Grid"
    }

    pub fn description(&self) -> &str {
        "Finds the largest hotpixel-free rectangular regions on the sensor."
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn process_frame(&mut self, _frame: &PreviewFrame, output: &mut AnalysisOutput) {
        if self.show_roi_grid {
            if let Some(grid) = &self.roi_grid {
                output.overlays.push(Overlay::RoiGrid {
                    grid: grid.clone(),
                    highlight_top_n: self.roi_grid_top_n,
                });
            }
        }
    }

    pub fn reset(&mut self) {
        self.roi_grid = None;
        self.show_roi_grid = false;
        self.last_mask_snapshot.clear();
    }

    pub fn roi_grid(&self) -> Option<&Arc<RoiGrid>> {
        self.roi_grid.as_ref()
    }

    pub fn show_roi_grid(&self) -> bool {
        self.show_roi_grid
    }

    pub fn set_show_roi_grid(&mut self, show: bool) {
        self.show_roi_grid = show;
    }

    pub fn roi_grid_top_n(&self) -> usize {
        self.roi_grid_top_n
    }

    pub fn set_roi_grid_top_n(&mut self, n: usize) {
        self.roi_grid_top_n = n;
    }
}
