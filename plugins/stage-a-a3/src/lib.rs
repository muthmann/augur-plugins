//! A3 runtime entry point using the shared sine acquisition coordinator.
use augur_plugin_api::Plugin;
pub use stage_a_sine_acquisition::StageAA3Plugin;
augur_plugin_api::export_plugin!(StageAA3Plugin);
