# EVE Temporal Diagnostics

## Summary

This feature extends the in-tree eveSMLM pipeline with better live diagnostics for candidate tuning and fit rejection analysis.

The change adds:

- temporal candidate clustering over retained event history
- provisional versus complete cluster tracking
- cluster-boundary overlays with clickable centroid markers
- rejected-fit investigation datasets and overlays

## Candidate Finding

`EVE Candidate Finding` can now request retained event history from the host and cluster over a configurable temporal lookback window instead of only the current preview frame.

Tracked clusters keep a stable `cluster_id` while they are visible. A cluster is only published downstream once it has stopped growing for the configured number of stable frames. Until then it remains provisional.

The candidate overlay now includes:

- 2-sigma eigenfeature ellipses for DBSCAN and eigenfeature modes
- bounding boxes for frame-based mode
- clickable centroid markers linked to the accepted-events investigation dataset

Accepted candidate-event rows now intentionally use `cluster_id` as the row-id column so one centroid click can select all raw events that belong to that cluster across the host table and 3D inspection views.

This is intentionally scoped to the accepted candidate-events dataset. AugurRS still keys selection by `(dataset_id, row_id)`, so matching `cluster_id` values do not create automatic cross-dataset linking into rejected fits or other datasets.

## Candidate Fitting

`EVE Candidate Fitting` now records rejected fits with structured rejection reasons instead of only counting them.

Rejected fits are exposed as a separate host dataset:

- dataset id: `augur.evesmlm.rejected_fits`
- layer id: `augur.layer.evesmlm.rejected_fits`
- compact/table views for row-wise inspection
- linked 3D view: `augur.evesmlm.rejected_fits.scatter3d`

Each rejected row carries:

- stable `row_id`
- source `cluster_id`
- position and timestamp
- sigma values when available
- fit residual
- event count and polarity balance
- rejection reason

The fitting status output now reports the rejection breakdown across fit failures, sigma-bound rejections, and residual-bound rejections.

## Investigation Contracts

This feature keeps the existing host-owned investigation model intact and extends it with two important conventions:

1. Candidate centroid overlays link into the accepted raw-event dataset by reusing `cluster_id` as the stable row key.
2. Rejected fits are exposed as a first-class structured dataset instead of being implicit in a status count.

## Verification

```bash
cargo test -p augur-plugin-evesmlm-candidates
cargo test -p augur-plugin-evesmlm-fitting
```
