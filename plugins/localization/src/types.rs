//! Shared types published by the Localization plugin.
//!
//! These types are consumed by downstream plugins (e.g. Focus Metrics)
//! through the PluginContext.

#[derive(Debug, Clone)]
pub struct Localization {
    pub x: f64,
    pub y: f64,
    pub sigma_x: f64,
    pub sigma_y: f64,
    pub amplitude: f64,
    pub background: f64,
    pub timestamp_us: u64,
    pub fit_error: f64,
}

#[derive(Debug, Clone, Default)]
pub struct LocalizationResults {
    pub localizations: Vec<Localization>,
    pub frame_window_start_us: u64,
    pub frame_window_end_us: u64,
}
