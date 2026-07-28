# eveSMLM Pipeline

The eveSMLM pipeline is implemented as three focused plugins so each stage can be enabled, configured, and debugged independently.

## Stages

1. **EVE Candidate Finding** (`RawEvents`) clusters raw `CdEvent` samples into emitter candidates, can aggregate over retained event history, publishes only stable completed `EveCandidates`, and exposes accepted/rejected raw-event investigation layers plus boundary overlays.
2. **EVE Candidate Fitting** (`DerivedData`) converts each completed candidate into one or more sub-pixel localization estimates, republishes `EveLocalizationResults` and `LocalizationResults`, and exposes both the shared host-view dataset `augur.evesmlm.current_localizations` and the rejected-fit dataset `augur.evesmlm.rejected_fits`.
3. **EVE Post-Processing** (`DerivedData`) filters, drift-corrects, and evaluates the fitted localizations, then republishes the same host-view dataset id and view ids with the same schema and metadata.

## Why Three Plugins

- Keeps raw-event grouping separate from numerical fitting, so candidate quality can be inspected directly.
- Lets researchers compare accepted and rejected candidate-stage raw events while tuning clustering thresholds.
- Lets researchers compare fitting methods on a fixed candidate set.
- Allows post-processing to be toggled or replaced without touching candidate generation.
- Preserves compatibility with existing downstream plugins through `LocalizationResults`.

## Host View Resolution

- `EVE Candidate Finding` publishes two investigation datasets for the current analysis window:
  - accepted candidate events
  - rejected candidate events
- the accepted candidate-events dataset now keys rows by `cluster_id` so centroid overlays can select every event in a cluster at once.
- both candidate datasets now register host tables as well as 3D views, so the investigation workflow has visible table targets for selection and inspection.
- both candidate datasets include timestamps, 2D coordinates, and 3D scatter metadata so the host can color them separately in linked 2D/3D inspection.
- candidate host-view titles stay short (`Accepted Events`, `Rejected Events`) because the host renders
  table/window chips in narrow plugin cards; the full dataset ids remain stable.
- candidate table display metadata marks concise `X`, `Y`, `Time`, `Polarity`, and `Cluster`
  labels, with accepted events using `Cluster` as the compact-card headline.
- fitting also publishes a rejected-fit investigation dataset and 3D view so fit failures and threshold rejections can be inspected alongside accepted localizations.
- cross-dataset linking is still host-limited: matching `cluster_id` values do not automatically link candidate events to rejected fits because AugurRS selections are scoped by dataset id.
- The compact EVE localization panel is declared by both fitting and post-processing.
- the 3D current-localizations view is also declared by both fitting and post-processing
- The host resolves duplicate ids in plugin execution order.
- When **EVE Post-Processing** is enabled, it becomes the active provider for the panel view.
- When post-processing is disabled, the panel falls back automatically to **EVE Candidate Fitting**.
- fitting and post-processing must therefore keep the shared current-localization dataset/view descriptors identical

## Calibration Note

AugurRS now publishes host-owned calibration on `CTX_GLOBAL_SETTINGS` as `GlobalSettings`.

The fitting and post-processing stages now use that host `nm_per_pixel` value automatically when it is available, while retaining a hidden fallback for older hosts that do not publish `GlobalSettings` yet.

## Data Flow

`CdEvent` stream -> tracked / completed `EveCandidates` -> `EveLocalizationResults` (+ rejected-fit dataset) -> filtered / corrected `EveLocalizationResults`

## Installation

Build all three plugins and install them into `~/.augur/plugins/`:

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build \
  -p augur-plugin-evesmlm-candidates \
  -p augur-plugin-evesmlm-fitting \
  -p augur-plugin-evesmlm-postproc \
  --release

for name in evesmlm-candidates evesmlm-fitting evesmlm-postproc; do
  mkdir -p ~/.augur/plugins/$name
  cp plugins/$name/plugin.toml ~/.augur/plugins/$name/
  cp target/release/libaugur_plugin_${name//-/_}.dylib ~/.augur/plugins/$name/
done
```

Then open `augur-gui`, go to **Plugins → Scan for New Plugins**, and enable all three. The runtime loader discovers ordering from phase declarations (`RawEvents` then `DerivedData`), so no manual registration order is required.

## References

- Weber, L.M., Martens, K.J.A., Cabriel, C., Gates, J.J., Albecq, M., Vermeulen, F., Hein, K., Izeddin, I., & Endesfelder, U. (2024). "EVE is an open modular data analysis software for event-based localization microscopy." bioRxiv. https://doi.org/10.1101/2024.08.09.607224
