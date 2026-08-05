# ADR 005: Expose Generic Investigation Datasets From Plugins

## Status

Accepted

## Context

`augur-gui` now owns a generic linked investigation workspace across 2D preview, 3D inspection, and host-rendered tables.

That host model depends on richer plugin-side dataset metadata than the older window-centric host-view integration used:

- stable row ids
- optional 2D and 3D coordinates
- optional time columns
- layer ids and display metadata

The eveSMLM pipeline also needs stage-local investigation surfaces for tuning, especially at the candidate-finding stage where researchers need to compare accepted and rejected raw events directly.

## Decision

Plugins in this repository will align to the investigation workspace through generic structured datasets.

Rules:

1. Use table datasets as the primary linking surface for inspectable scientific outputs.
2. Provide `row_id_column` when the plugin can produce stable ids.
3. Provide `coordinate_space_2d`, `coordinate_space_3d`, and `time_column` when the data supports linked 2D/3D inspection.
4. Use `layer_id` plus `HostDatasetDescriptor.display` for visibility and styling defaults.
5. Keep intentionally shared dataset/view ids byte-for-byte identical across providers.
6. Use overlays only for supplemental 2D annotation or hit-testing, not as the primary data contract.
7. When one stage needs multiple logical layers, publish separate datasets/layer ids instead of keying style by plugin name.
8. It is acceptable for multiple rows to share the same stable row id when the intended interaction is "select the whole cluster" rather than "select one raw sample".
9. Stable row keys are dataset-scoped in the current host, so matching row ids across different datasets do not create cross-dataset selection on their own.

## Consequences

- the host can keep selection, styling, and filtering generic
- candidate-finding can expose accepted and rejected raw events as separate investigation layers
- candidate centroid overlays can select whole raw-event clusters by reusing `cluster_id` as the stable row key for accepted events
- fitting and post-processing can safely reuse the same current-localization ids without breaking host linking
- fitting can expose rejected fits as a first-class investigation dataset instead of hiding them behind aggregate counters
- plugins carry a little more schema metadata, but avoid plugin-specific host hooks
