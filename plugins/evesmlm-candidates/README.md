# EVE Candidate Finding

Runtime plugin. Build `augur-plugin-evesmlm-candidates` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/evesmlm-candidates/`.

Raw-event candidate discovery for eveSMLM. This plugin groups `CdEvent` samples into per-emitter candidate clusters without first collapsing them into a conventional image, which keeps the pipeline aligned with the EVE approach described by Weber et al. (2024, https://doi.org/10.1101/2024.08.09.607224).

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
| Lookback | `66_000` us | Retained-history window used for temporal clustering; set to `0` for single-frame behavior |
| Stable frames | `2` | Number of consecutive no-growth frames before a cluster is published |
| Max spatial extent | `5.0` px | Eigenfeature upper bound on the major covariance axis |
| Min isotropy | `0.2` | Eigenfeature lower bound on `lambda2 / lambda1` |
| Threshold factor | `1.5` | Wavelet threshold multiplier for frame-based mode |
| Fit radius | `4` px | Event gathering radius in frame-based mode |
| Max candidates | `512` | Safety cap on published candidates |
| Show centroids | `true` | Draw clickable centroid markers linked to accepted candidate events |
| Show boundaries | `true` | Draw 2-sigma ellipses or bounding boxes around visible clusters |
| Show provisional | `true` | Keep still-growing clusters visible in the overlay |

## Execution Phase

`RawEvents` — consumes the raw `CdEvent` stream for the current preview window and can optionally gather retained events from earlier frames.

## Published Data

Publishes `EveCandidates` on the context key `augur.evesmlm.candidates`, containing:

- `clusters: Vec<EveCluster>` with stable `cluster_id`, raw events, per-pixel histograms, centroid, bounds, and optional boundary metadata
- `frame_window_start_us`, `frame_window_end_us`
- `n_events_processed`
- `finding_method`

It also exposes two host investigation datasets for the current analysis window:

- accepted candidate events
- rejected candidate events

Both datasets carry stable row ids, timestamps, 2D coordinates, and 3D scatter metadata so the host can render accepted and rejected raw events as separate layers during live parameter tuning.

The plugin now also registers compact and windowed host tables for both datasets, so centroid selection has a visible table target inside AugurRS without requiring plugin-specific UI.

Accepted candidate-event rows intentionally use the string form of `cluster_id` as the row-id column so one centroid click can select the whole cluster in the accepted-events table and its 3D view.

That selection is dataset-local: it links the centroid marker to the accepted-events dataset, but it does not cross-select unrelated datasets such as rejected fits because AugurRS stable row keys include the dataset id.

## Dependencies

None.

## References

- Weber et al., "EVE is an open modular data analysis software for event-based localization microscopy," bioRxiv, 2024. https://doi.org/10.1101/2024.08.09.607224
