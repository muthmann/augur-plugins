//! AugurRS Plugin Template
//!
//! Copy this crate to start a new plugin. Rename the crate in Cargo.toml,
//! update plugin.toml with your metadata, and replace this implementation
//! with your analysis logic.
//!
//! See CONTRIBUTING.md in the repository root for the full guide.

use augur_core::{analysis::AnalysisOutput, pipeline::PreviewFrame};

// Re-export the plugin types your consumers need.
// For the AnalysisPlugin trait, PluginContext, and PluginInput,
// these are provided by augur-gui and will be available when your
// plugin is compiled into the host binary.

/// Plugin state. All mutable state lives here.
#[derive(Default)]
pub struct TemplatePlugin {
    enabled: bool,
    // Add your settings and state fields here.
}

// NOTE: The AnalysisPlugin trait implementation goes here.
// Because the trait is defined in augur-gui (not augur-core), the actual
// `impl AnalysisPlugin for TemplatePlugin` block is written when the plugin
// is integrated into the augur-gui build. During standalone development,
// you can write your analysis logic as regular methods on your struct and
// test them independently.
//
// See the existing plugins (hotpixel, roi-grid, localization, focus-metrics)
// for complete trait implementation examples.

impl TemplatePlugin {
    /// Plugin name as it appears in the Analysis panel.
    pub fn name(&self) -> &str {
        "Template Plugin"
    }

    /// Short description shown below the plugin name.
    pub fn description(&self) -> &str {
        "A starting point for new plugins."
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Core analysis logic. Called once per preview frame when the plugin is enabled.
    pub fn analyze(&mut self, frame: &PreviewFrame, output: &mut AnalysisOutput) {
        // Your analysis goes here.
        // Access frame.pixels (decoded preview), frame.width, frame.height.
        // Push overlays or warnings into output.
        let _ = (frame, output);
    }

    /// Reset all mutable state. Called when the plugin is disabled or the session restarts.
    pub fn reset(&mut self) {
        // Clear your buffers, histories, caches, etc.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_plugin_default_is_disabled() {
        let plugin = TemplatePlugin::default();
        assert!(!plugin.enabled());
    }
}
