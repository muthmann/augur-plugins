# Molecule Localization

Runtime plugin. Build `augur-plugin-localization` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/localization/`.

Sub-pixel molecule localization from the live event camera stream, suitable for single-molecule localization microscopy (SMLM). The plugin implements a wavelet-filtered spot detection pipeline followed by least-squares elliptical Gaussian fitting.

## Pipeline

1. **Image reconstruction** — builds a time-weighted accumulation image from raw `CdEvent` polarity events (or falls back to decoded preview counts if raw events are unavailable)
2. **Wavelet denoising** — two-level B3 spline wavelet decomposition (G1 and G2 kernels) to separate signal from background
3. **Thresholding** — keeps wavelet coefficients exceeding `n * sigma(F1)` in the second detail level
4. **Local maxima detection** — finds 3x3 neighborhood maxima in the thresholded image (up to 256 candidates per frame)
5. **Center-of-mass seeding** — refines each candidate position using intensity-weighted center of mass
6. **Gaussian fitting** — Levenberg-Marquardt least-squares fit of a 2D elliptical Gaussian (6 parameters: x, y, sigma_x, sigma_y, amplitude, background) with 15 iterations
7. **Quality filtering** — rejects fits outside configurable sigma range, above maximum xy uncertainty, or with non-finite fit error
8. **Timestamp estimation** — assigns a distance-weighted mean timestamp from nearby raw events

## Configuration

| Setting | Default | Description |
|---|---|---|
| Wavelet threshold n | `1.5` | Multiplier applied to sigma(F1) for thresholding |
| Fit radius | `4` px | Radius of the Gaussian fit ROI (4 px = 9x9 window) |
| Initial sigma | `1.6` px | Starting sigma for the Gaussian fit |
| Scale | `65.0` nm/px | Pixel size used for nm-based filters |
| Sigma range | `100–190` nm | Accepted sigma range for valid localizations |
| Max xy uncertainty | `35.0` nm | Maximum localization uncertainty |
| Show overlay | `true` | Draw crosshair markers on accepted localizations |

## Execution Phase

`RawEvents` — uses the raw `CdEvent` stream for time-weighted image reconstruction. Falls back to preview counts when raw events are unavailable.

## Published Data

Publishes `LocalizationResults` on the context key `augur.localization.results`, which contains:

- `localizations: Vec<Localization>` — each with x, y, sigma_x, sigma_y, amplitude, background, timestamp_us, fit_error
- `frame_window_start_us`, `frame_window_end_us` — the time window of the analyzed frame

Downstream plugins (e.g., Focus Metrics and the eveSMLM post-processing chain) consume this data.

## Dependencies

None (but benefits from raw event transport being enabled).

## References

The wavelet-based spot detection follows the approach described in:

- Izeddin et al., "Wavelet analysis for single molecule localization microscopy," Optics Express 20(3):2081-2095, 2012.
