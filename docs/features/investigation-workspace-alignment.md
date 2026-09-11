# Investigation Workspace Alignment

## Summary

This pass aligns the in-tree plugins in `augur-plugins` with the host-owned investigation workspace now implemented in `augur-rs`.

The goal is not plugin-specific UI. The goal is to expose better generic data contracts so the host can link:

- 2D preview points
- 3D inspection layers
- host-rendered tables

## What Changed

- `evesmlm-candidates` now publishes two generic raw-event investigation datasets:
  - accepted candidate events
  - rejected candidate events
- those candidate datasets carry:
  - stable row ids
  - `timestamp_us`
  - 2D coordinates
  - 3D coordinates using time as the `z` axis
  - layer/display metadata for distinct accepted vs rejected styling
- accepted candidate rows can now intentionally share a `cluster_id` row key so one centroid overlay can select every event in that cluster
- candidate datasets must register table views as well as 3D views when the workflow expects row-wise inspection and linked selection
- `evesmlm-fitting` and `evesmlm-postproc` now keep the shared `augur.evesmlm.current_localizations` contract aligned with:
  - stable row ids
  - `timestamp_us`
  - 2D and 3D coordinate metadata
  - shared layer/display metadata
  - linked marker overlays carrying stable ids
- `evesmlm-fitting` also publishes `augur.evesmlm.rejected_fits` for rejected candidates with timestamps, positions, metrics, and rejection reasons
- matching ids across different datasets still do not link automatically because the host selection model keys rows by dataset id plus stable row id
- `reconstruction` now exposes the accumulated localization dataset as a fuller investigation dataset with:
  - stable row ids
  - `timestamp_us`
  - 3D scatter metadata
  - layer/display metadata
- repo-local docs and the template guidance now describe stable ids, dataset/layer metadata, and overlays as supplemental rather than primary integration surfaces

## Important Contracts

### Candidate Event Layers

The candidate-finding stage now surfaces accepted and rejected raw events from the active analysis window as separate host datasets instead of hiding that distinction inside plugin-local logic or centroid-only overlays.

That makes it possible to tune candidate parameters while seeing:

- which events survived into clusters
- which events were rejected
- how those two groups distribute over time in the 3D view

### Shared EVE Current Localizations

`evesmlm-fitting` and `evesmlm-postproc` intentionally reuse the same dataset id and view ids for current localizations.

To keep host-side linking trustworthy, those reused descriptors must stay identical across both providers:

- same schema
- same row-id column
- same coordinate/time metadata
- same layer metadata
- same view descriptors

The later enabled provider can then replace the dataset payload without breaking selection, styling, or view resolution.

### Reconstruction

The reconstruction plugin remains generic. It still publishes one accumulated dataset as the source of truth, but that dataset now participates in the linked investigation model instead of acting only as a density-view backing store.

## Verification

```bash
cargo check -p augur-plugin-evesmlm-candidates
cargo test -p augur-plugin-evesmlm-candidates
cargo test -p augur-plugin-evesmlm-fitting
cargo test -p augur-plugin-evesmlm-postproc
cargo test -p augur-plugin-reconstruction
```
