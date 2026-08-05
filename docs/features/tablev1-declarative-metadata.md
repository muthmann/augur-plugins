# TableV1 Declarative Metadata For Plugins

## Summary

Plugins that expose `HostDatasetKind::TableV1` now describe row provenance, cross-dataset
relations, and per-column display formatting declaratively. The host consumes these
descriptors to render timestamps as `mm:ss.uuu`, size columns sensibly, drive summary cards,
auto-seek replay to the anchor timestamp of a selected row, and resolve derived-row selections
back to contributing raw events for 3D emphasis.

This replaces implicit conventions (where the host guessed from type or column name) with
explicit, serializable metadata carried on `TableSchema` and `HostDatasetDescriptor`.

## What Plugins Populate

On `TableSchema`:

- `provenance: Some(TableRowProvenance { anchor_time_column, span_start_column, span_end_column })`
  — typically `anchor_time_column: Some("timestamp_us")`. Spans are used by the host for
  span-based visibility and anchor fallback, so `span_start_column` / `span_end_column` should
  describe the real contributing interval rather than repeating the anchor timestamp.
- `column_display: Vec<TableColumnDisplayEntry>` — one entry per column you want formatted:
  - timestamp columns → `TableColumnDisplayFormat::TimestampMicros`
  - positions, widths, residuals → `FixedPrecision { digits: N }`
  - `row_id` columns → `Identifier` with `hidden: true`
  - enum-like columns (methods, reasons) → `Category` (promote with `headline: true` in
    failure-result schemas to make the reason the summary-card heading)
  - Width priority: `High` (~160px) for labels and text; `Medium` (~100px) for numeric;
    `Low` (~60px) for compact identifiers.

On `HostDatasetDescriptor`:

- `relations: Vec<HostDatasetRelation { target_dataset_id, via_column, target_column }>` —
  declare joins from this dataset's row to another dataset. Example: candidate-event rows
  relate to localizations via `cluster_id`. The host can follow these joins transitively to map a
  selected derived row back to raw accepted-event identities.

All new fields are additive with serde defaults; omitting them keeps the prior behavior.

## Implemented Datasets

- `evesmlm-fitting`: `augur.evesmlm.current_localizations`, `augur.evesmlm.rejected_fits` — full
  provenance with real `span_start_us` / `span_end_us`, per-column formatting, cluster relations
  back to accepted candidate events, and `rejection_reason` marked `headline: true` for rejected
  fits.
- `evesmlm-candidates`: accepted/rejected candidate events — provenance on `timestamp_us`,
  relation to `current_localizations` via `cluster_id` on accepted events.

## Descriptor Parity

`evesmlm-postproc` re-exports the `current_localizations` registry builder from
`evesmlm-fitting`, so the descriptor is structurally identical by construction. A parity test
in `plugins/evesmlm-postproc/src/lib.rs` serializes both registries to JSON and asserts
equality to catch accidental divergence.

## Related Host Behavior

See the companion host feature brief: [Investigation Table Trustworthiness](https://github.com/muthmann/augur-rs/blob/main/docs/features/investigation-table-trustworthiness.md)
and [ADR 017](https://github.com/muthmann/augur-rs/blob/main/docs/adr/017-declarative-tablev1-metadata.md).
