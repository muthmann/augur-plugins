# EVE Candidate Fitting

Runtime plugin. Build `augur-plugin-evesmlm-fitting` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/evesmlm-fitting/`.

Sub-pixel localization for eveSMLM candidate clusters. The plugin consumes `EveCandidates`, fits each cluster with one of several estimators, and republishes the results both as EVE-native `EveLocalizationResults` and as `LocalizationResults` for compatibility with downstream plugins such as Focus Metrics.

## Methods

| Method | Output | Notes |
|---|---|---|
| **Log-Gaussian** | `x`, `y`, `sigma_x`, `sigma_y`, residual | Closed-form weighted least squares on the logarithmic Gaussian model |
| **Gaussian** | `x`, `y`, `sigma_x`, `sigma_y`, residual | Iterative elliptical Gaussian fit |
| **Radial Symmetry** | `x`, `y`, residual | Fast gradient-line intersection, no sigma output |
| **Phasor** | `x`, `y`, residual | First Fourier-mode phase localization, no sigma output |
| **Mean XY** | `x`, `y` | Weighted centroid baseline |

## Configuration

| Setting | Default | Description |
|---|---|---|
| Fit method | `Log-Gaussian` | Candidate fitting backend |
| Scale | `65.0` nm/px | Pixel size used for sigma filtering |
| Sigma min | `80.0` nm | Lower accepted sigma bound for sigma-producing methods |
| Sigma max | `200.0` nm | Upper accepted sigma bound for sigma-producing methods |
| Max fit residual | `0.5` | Reject fits above this residual |
| Show overlay | `true` | Highlight accepted localization positions |

## Execution Phase

`DerivedData` — consumes `EveCandidates` published by **EVE Candidate Finding**.

## Published Data

- `EveLocalizationResults` on `augur.evesmlm.localization_results`
- `LocalizationResults` on `augur.localization.results` for compatibility with plugins such as Focus Metrics

## Dependencies

Depends on **EVE Candidate Finding**.

## References

- Weber et al., "eveSMLM: event-based vision for single molecule localization microscopy," bioRxiv, 2024.
- Parthasarathy, "Rapid, accurate particle tracking by calculation of radial symmetry centers," Nature Methods, 2012.
