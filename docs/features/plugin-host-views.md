# Plugin Host Views

`augur-plugin-api` now lets plugins publish host-rendered datasets and views through `host_views()` and `host_view_dataset()` instead of relying on reconstruction-specific host wiring.

## What Plugins Can Declare

- datasets with stable ids and explicit schema metadata
- analysis-panel views rendered by the host
- window views rendered by the host
- shared datasets reused by multiple views

## Current In-Tree Usage

- `Localization Reconstruction` publishes `augur.localization.accumulated` once and lets the host render both:
  - a `Localization Table` window
  - a `Reconstruction` density window
- `EVE Candidate Fitting` and `EVE Post-Processing` both publish `augur.evesmlm.current_localizations` with the same schema and the same compact panel view id

Because the host resolves duplicate ids in plugin execution order, `EVE Post-Processing` becomes the active provider whenever it is enabled; otherwise the compact table falls back to `EVE Candidate Fitting`.

## Why The Split Matters

- plugins keep scientific accumulation and stage-specific data ownership
- the host keeps rendering, scrolling, export, and window management
- one dataset can drive multiple host views without duplicate plugin state
- future localization plugins can reuse the same host-rendered table and density windows by publishing the same dataset contract
