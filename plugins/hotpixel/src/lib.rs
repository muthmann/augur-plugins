//! Hotpixel Detection Plugin
//!
//! Identifies persistently noisy pixels using an exponential moving average
//! of per-pixel event counts. Flagged pixels can be pushed into the IMX636
//! hardware DEM mask.
//!
//! Execution phase: FrameOnly
//! Dependencies: None
//! Published data: None

use augur_core::{
    analysis::{
        hotpixel::{HotpixelConfig, HotpixelDetector},
        AnalysisOutput, Analyzer,
    },
    pipeline::PreviewFrame,
};

pub struct HotpixelPlugin {
    config: HotpixelConfig,
    detector: HotpixelDetector,
}

impl Default for HotpixelPlugin {
    fn default() -> Self {
        let config = HotpixelConfig::default();
        Self {
            detector: HotpixelDetector::new(config.clone()),
            config,
        }
    }
}

impl HotpixelPlugin {
    fn rebuild_detector(&mut self) {
        self.detector = HotpixelDetector::new(self.config.clone());
    }

    pub fn name(&self) -> &str {
        "Hotpixel Detection"
    }

    pub fn description(&self) -> &str {
        "GUI-only analysis. Identifies pixels that fire at abnormally high rates regardless of scene activity."
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        if self.config.enabled != enabled {
            self.config.enabled = enabled;
            self.rebuild_detector();
        }
    }

    pub fn process_frame(&mut self, frame: &PreviewFrame, output: &mut AnalysisOutput) {
        let plugin_output = self.detector.process_frame(frame);
        output.overlays.extend(plugin_output.overlays);
        output.warnings.extend(plugin_output.warnings);
    }

    pub fn reset(&mut self) {
        self.detector.reset();
    }

    pub fn config(&self) -> &HotpixelConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut HotpixelConfig {
        &mut self.config
    }
}
