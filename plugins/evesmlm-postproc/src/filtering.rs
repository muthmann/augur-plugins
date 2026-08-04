use evesmlm_types::{EveLocalization, EveLocalizationResults};

pub fn filter_results(
    results: &EveLocalizationResults,
    min_n_events: usize,
    max_polarity_imbalance: f64,
    max_fit_residual: f64,
) -> EveLocalizationResults {
    EveLocalizationResults {
        localizations: results
            .localizations
            .iter()
            .filter(|localization| {
                passes_filters(
                    localization,
                    min_n_events,
                    max_polarity_imbalance,
                    max_fit_residual,
                )
            })
            .cloned()
            .collect(),
        frame_window_start_us: results.frame_window_start_us,
        frame_window_end_us: results.frame_window_end_us,
    }
}

pub fn passes_filters(
    localization: &EveLocalization,
    min_n_events: usize,
    max_polarity_imbalance: f64,
    max_fit_residual: f64,
) -> bool {
    localization.n_events >= min_n_events
        && localization.polarity_balance.abs() <= max_polarity_imbalance
        && localization.fit_residual <= max_fit_residual
}
