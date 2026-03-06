# EVE Candidate Finding

Raw-event candidate discovery for eveSMLM. This plugin groups `CdEvent` samples into per-emitter candidate clusters without first collapsing them into a conventional image, which keeps the pipeline aligned with the EVE approach described by Weber et al. (2024).

## Methods

| Method | What it does | Best use |
|---|---|---|
| **DBSCAN** | Grid-accelerated spatial clustering directly on raw events | General-purpose candidate finding |
| **Eigenfeature** | DBSCAN followed by covariance eigenvalue filtering | Reject elongated streaks and anisotropic noise |
| **Frame-based** | Wavelet-threshold local maxima on an accumulated count image, then gathers nearby events | Backstop method for comparison with image-like pipelines |

## Configuration

| Setting | Default | Description |
|---|---|---|
| Finding method | `DBSCAN` | Candidate generation algorithm |
| Polarity | `Both` | Use positive, negative, or all events |
| Epsilon | `3.0` px | Neighborhood radius for DBSCAN |
| Min events | `5` | Minimum cluster size |
| Max spatial extent | `5.0` px | Eigenfeature upper bound on the major covariance axis |
| Min isotropy | `0.2` | Eigenfeature lower bound on `lambda2 / lambda1` |
| Threshold factor | `1.5` | Wavelet threshold multiplier for frame-based mode |
| Fit radius | `4` px | Event gathering radius in frame-based mode |
| Max candidates | `512` | Safety cap on published candidates |
| Show overlay | `true` | Highlight candidate centroids in the preview |

## Execution Phase

`RawEvents` — consumes the raw `CdEvent` stream for the current preview window.

## Published Data

Publishes `EveCandidates` to the `PluginContext`, containing:

- `clusters: Vec<EveCluster>` with raw events, per-pixel histograms, centroid, and bounds
- `frame_window_start_us`, `frame_window_end_us`
- `n_events_processed`
- `finding_method`

## Dependencies

None.

## References

- Weber et al., "eveSMLM: event-based vision for single molecule localization microscopy," bioRxiv, 2024.
