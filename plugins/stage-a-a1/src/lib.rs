//! A1 runtime entry point. Acquisition and analysis are shared with A3.
use augur_plugin_api::Plugin;
pub use stage_a_sine_acquisition::*;
augur_plugin_api::export_plugin!(StageAA1Plugin);
