# Focus Metrics

Runtime plugin. Build `augur-plugin-focus-metrics` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/focus-metrics/`.

Live focus quality monitoring for event-camera imaging. Provides three complementary methods for assessing focus during acquisition, with a rolling history plot and traffic-light quality indicator.

## Methods

| Method | What it measures | Requires localization? |
|---|---|---|
| **Mean PSF sigma** | Average fitted Gaussian width across all accepted localizations. Lower sigma = better focus. | Yes |
| **FFT high frequency** | Integrated spectral power in a high-frequency ring filter applied to the downsampled preview frame. Higher power = sharper features. | No |
| **Astigmatic ratio** | Mean sigma_x / sigma_y across localizations. Ratio near 1.0 = symmetric PSF = good focus. | Yes |

## Configuration

| Setting | Default | Description |
|---|---|---|
| History depth | `120` | Number of frames for the rolling history plot |
| Scale | `65.0` nm/px | Pixel size used for nm-based filtering |
| Sigma range | `100–190` nm | Accepted sigma range for localizations used in metrics |

## Execution Phase

`DerivedData` — runs after Phase 2 plugins. Consumes `LocalizationResults` published on `augur.localization.results` for Mean PSF sigma and Astigmatic ratio. The FFT method operates independently on the preview frame.

## Published Data

None.

## Dependencies

None at the manifest level. The Mean PSF sigma and Astigmatic ratio methods require an upstream plugin to publish standard localization results, while the FFT method has no upstream requirement.
