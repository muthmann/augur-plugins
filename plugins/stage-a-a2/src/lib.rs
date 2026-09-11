//! A2 runtime entry point. The recorder is shared with A5.
use augur_plugin_api::Plugin;
pub use stage_a_step_acquisition::*;
augur_plugin_api::export_plugin!(StageAA2Plugin);
