# Plugin Host Views

`augur-plugin-api` lets plugins publish host-rendered datasets and views through:

- `host_views()`
- `host_view_dataset()`
- optional `host_view_dataset_generation()`

That keeps scientific state in the plugin while letting the host own rendering, export, caching, and window management.

## What Plugins Can Declare

- datasets with stable ids and explicit schema metadata
- analysis-panel views rendered by the host
- standalone windows rendered by the host
- multiple views backed by the same dataset
- optional generation counters for cache invalidation

## Current In-Tree Usage

- `Localization Reconstruction` publishes `augur.localization.accumulated` once and lets the host render both:
  - a `Localization Table` window
  - a `Reconstruction` density window
- `EVE Candidate Fitting` and `EVE Post-Processing` both publish `augur.evesmlm.current_localizations` with the same schema and the same compact panel view id

Because the host resolves duplicate ids in plugin execution order, `EVE Post-Processing` becomes the active provider whenever it is enabled; otherwise the compact table falls back to `EVE Candidate Fitting`.

## Why The Split Matters

- plugins keep scientific accumulation and stage-specific data ownership
- the host keeps rendering, export, caching, and window state
- one dataset can drive multiple host views without duplicate plugin state
- later-compatible providers can reuse the same dataset/view ids when the metadata matches exactly
- generation counters let the host skip reloading unchanged datasets
