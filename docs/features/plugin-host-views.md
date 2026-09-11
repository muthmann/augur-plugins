# Plugin Host Views

`augur-plugin-api` lets plugins publish host-rendered datasets and views through:

- `host_views()`
- `host_view_dataset()`
- optional `host_view_dataset_generation()`

That keeps scientific state in the plugin while letting the host own rendering, export, caching, and window management.

## What Plugins Can Declare

- datasets with stable ids and explicit schema metadata
- stable row ids, time columns, and 2D/3D coordinate metadata for linked investigation
- analysis-panel views rendered by the host
- standalone windows rendered by the host
- multiple views backed by the same dataset
- layer/display metadata for host-owned visibility and styling defaults
- supplemental marker overlays for 2D hit-testing when datasets alone are not enough
- optional generation counters for cache invalidation

## Current In-Tree Usage

- `Localization Reconstruction` publishes `augur.localization.accumulated` once and lets the host render:
  - a `Localization Table` window
  - a `Reconstruction` density window
  - a `Localization Cloud` 3D view
- `EVE Candidate Finding` publishes accepted and rejected raw-event datasets as separate investigation layers
- `EVE Candidate Fitting` and `EVE Post-Processing` both publish `augur.evesmlm.current_localizations` with the same schema and the same view ids

Because the host resolves duplicate ids in plugin execution order, `EVE Post-Processing` becomes the active provider whenever it is enabled; otherwise the shared current-localizations dataset falls back to `EVE Candidate Fitting`.

`Scatter3dFromTable` descriptors are consumed by AugurRS as main investigation 3D scene layers.
Plugins should still declare them with stable ids and coordinate metadata, but should not rely on
them appearing as separate dock/window chips.

## Why The Split Matters

- plugins keep scientific accumulation and stage-specific data ownership
- the host keeps rendering, export, caching, and window state
- one dataset can drive multiple host views without duplicate plugin state
- later-compatible providers can reuse the same dataset/view ids when the metadata matches exactly
- generation counters let the host skip reloading unchanged datasets
