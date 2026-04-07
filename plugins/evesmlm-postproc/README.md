# EVE Post-Processing

Runtime plugin. Build `augur-plugin-evesmlm-postproc` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/evesmlm-postproc/`.

Post-processing and quality assessment for eveSMLM localization streams. The plugin consumes `EveLocalizationResults`, applies configurable filters, estimates per-frame drift against a rolling reference, and keeps lightweight evaluation summaries for precision, PSF shape, and fluorescent on-time.

## Pipeline

1. **Filtering** — removes localizations that fail event-count, polarity-balance, or residual thresholds
2. **Drift correction** — aligns the current frame to a rolling reference using sparse localization-map cross-correlation
3. **Evaluation** — updates eNeNA nearest-neighbor statistics, a rolling in situ PSF estimate, and greedy across-frame on-time tracks

## Configuration

| Setting | Default | Description |
|---|---|---|
| Min events | `3` | Minimum events per localization |
| Max polarity imbalance | `1.0` | Maximum allowed absolute polarity balance |
| Max fit residual | `inf` | Upper residual threshold |
| Drift correction | `false` | Enable rolling cross-correlation drift correction |
| Drift window | `50` frames | Number of corrected frames in the reference window |
| Show eNeNA | `true` | Track nearest-neighbor precision estimate |
| Show in situ PSF | `true` | Maintain a rolling PSF estimate |
| Show on-time | `true` | Maintain a greedy fluorescent on-time histogram |
| Show overlay | `true` | Highlight accepted, corrected localizations |

AugurRS now publishes host-owned calibration on `CTX_GLOBAL_SETTINGS` as `GlobalSettings`. This plugin uses the host `nm_per_pixel` value automatically when converting eNeNA-style metrics into nanometers, while retaining a hidden fallback for older hosts.

## Execution Phase

`DerivedData` — consumes `EveLocalizationResults` published by **EVE Candidate Fitting**.

## Published Data

Publishes filtered and drift-corrected `EveLocalizationResults` on `augur.evesmlm.localization_results`, republishes standard `LocalizationResults` on `augur.localization.results` for downstream compatibility, and serves the compact host-view dataset `augur.evesmlm.current_localizations`.

## Host View

This plugin deliberately reuses the same dataset id and compact panel view id as `EVE Candidate Fitting`. Because post-processing resolves later in the pipeline, it becomes the active provider whenever it is enabled.

## Dependencies

Depends on **EVE Candidate Fitting**.

## References

- Weber et al., "EVE is an open modular data analysis software for event-based localization microscopy," bioRxiv, 2024. https://doi.org/10.1101/2024.08.09.607224
- Endesfelder et al., "A simple method to estimate the average localization precision of a single-molecule localization microscopy experiment," Histochemistry and Cell Biology, 2014.
