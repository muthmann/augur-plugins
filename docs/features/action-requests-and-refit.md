# Action Requests And Single-Cluster Refit

## Summary

Plugins can declare host-rendered action buttons and consume the requests
the host publishes when the user triggers one. The eveSMLM fitting plugin
is the first concrete consumer: it exposes **Re-fit cluster…**,
**Commit refit**, and **Discard refit preview**. The re-fit action opens a
host-rendered modal driven by the plugin's `param_schema`, runs a
single-cluster fit with the captured parameters, and emits the result as a
separate `augur.evesmlm.refit_preview` dataset so it is visually distinct
from the main pipeline output.

## Plugin Contract

Refit is plumbed through the generic host action bus (see
`augur-rs/docs/features/investigation-action-requests.md`). In short:

- Add `HostActionDescriptor` entries to `HostViewRegistry.actions` in
  `host_views()`. Each descriptor declares:
  - `id` — stable identifier used to route the request in `process_frame`.
  - `title` — button label.
  - `scope` — one of `Dataset`, `Row`, `Cluster` with the target
    `dataset_id` (and `group_column` for `Cluster`).
  - `param_schema: Option<serde_json::Value>` — typically
    `serde_json::to_value(my_settings_schema())`. Pass `None` when the
    action takes no parameters.
- Read the persistent queue at `CTX_INVESTIGATION_ACTION_REQUESTS`
  (`HostActionRequestQueue`). Filter by your cached
  `last_consumed_action_request_id` so each request runs exactly once.
- For `Cluster` actions, expect the host to snapshot the selected rows into
  `params["__augur_cluster_rows"]`. Plugins can reconstruct the selected
  cluster from those rows instead of depending on the next frame to still
  contain the same cluster.
- Emit side effects. Publish overlays/datasets for visual preview, or
  mutate owned state for commit/discard.

## Fitting Plugin Implementation

- Three actions registered in `host_views()`:
  - `augur.evesmlm.refit_cluster` — `Cluster` scope on
    `augur.evesmlm.candidates.accepted_events` with
    `group_column = "cluster_id"`. `param_schema` covers `fit_method`,
    `sigma_min_nm`, `sigma_max_nm`, `max_fit_residual`.
  - `augur.evesmlm.commit_refit` — `Row` scope on
    `augur.evesmlm.refit_preview`, no params.
  - `augur.evesmlm.discard_refit` — `Dataset` scope on
    `augur.evesmlm.refit_preview`, no params.
- New persistent plugin state:
  - `host_results: EveLocalizationResults` / `host_rejected_fits: Vec<RejectedFitRow>` —
    host-visible history keyed by `cluster_id`, used for persistent tables,
    3D views, and post-commit durability across frames.
  - `refit_preview_results: EveLocalizationResults` — preview rows.
  - `refit_preview_replaces: Vec<Option<u64>>` — parallel vec mapping each
    preview row to the current-frame row it replaces on commit (or
    `None` to append).
  - `last_consumed_action_request_id: u64` — dedupe cursor.
- `process_frame` runs the normal analysis, merges the frame into the
  host-visible history, drains the queue, then publishes
  `CTX_EVE_LOCALIZATION_RESULTS`. Host tables/3D views therefore keep
  committed rows and historical rejected fits visible across frames, while
  the reconstruction-facing context publish stays frame-local.
- Preview rows render with a yellow filled-circle marker via
  `add_marker_overlay`, distinct from accepted (green cross) and rejected
  (red diamond).

## Scope Resolution Details

- **Re-fit** reconstructs the selected cluster from the host-supplied
  `__augur_cluster_rows` snapshot when available, and only falls back to
  the current frame's `EveCandidates` if no snapshot is present. This lets
  the action work from persistent/historical selections instead of only the
  latest frame.
- **Commit** matches the preview row by `row_id` parsed from the scope
  payload, upserts the committed localization into the host-visible
  history, drops any matching rejected-fit row for that cluster, and
  updates the current frame-local results only if that cluster is still
  present in the current frame.
- **Discard** clears the preview list. No other plugin state is touched.

## Byte-Identical On Discard

A targeted unit test
(`discard_clears_preview_without_touching_current_results`) clones
`current_results` before discard and asserts byte-identical JSON equality
after. The main pipeline output for the next frame is therefore unchanged
when a request is discarded.

## References

- `augur-rs/docs/adr/018-host-action-bus.md`
- `augur-rs/docs/features/investigation-action-requests.md`
- `plugins/evesmlm-fitting/src/lib.rs`
