# Clickable 2D Overlays via Marker `source_row`

## Summary

The augur-plugin-api ABI (bumped to 4) adds `source_dataset_id` and
`source_row_id` to `FfiMarkerOverlayItem`. Host-side, the viewer uses these
fields — when set — as the authoritative `StableRowKey` on click, instead of
falling back to the `(overlay.dataset_id, marker.stable_id)` pair. This lets a
plugin emit markers on one layer while pointing clicks at rows in a
*different* dataset.

In-tree plugins now populate `source_row` explicitly:

- `evesmlm-fitting` — accepted-localization crosses point at
  `current_localizations`; rejected-fit diamonds point at `rejected_fits`.
- `evesmlm-postproc` — drift-corrected localization crosses point at
  `current_localizations`.
- `evesmlm-candidates` — cluster centroid markers leave `source_row` empty
  pending a cluster-addressable dataset (future work).

## Effect on the EVE Failed-Fit Loop

Combined with the Stage-2 `rejection_reason` headline and row provenance on
`rejected_fits`, clicking a red diamond in the 2D viewer now:

1. selects the backing row in the rejected-fit `TableWindow`;
2. shows `rejection_reason` as the summary card heading;
3. auto-seeks the replay transport to the fit's anchor timestamp;
4. keeps the diamond visible while scrubbing inside the fit's declared span.

No per-frame result cache is involved — the host filters declared rows by
`[span_start_us, span_end_us]` against the current frame window.

## Code References

| Path | Role |
| --- | --- |
| `plugins/evesmlm-fitting/src/lib.rs` | Populates `source_row` on accepted crosses and rejected diamonds |
| `plugins/evesmlm-postproc/src/lib.rs` | Populates `source_row` on drift-corrected localization crosses |
| `plugins/evesmlm-candidates/src/lib.rs` | Pending: cluster-addressable dataset for centroid markers |

## Related

- [TableV1 Declarative Metadata](./tablev1-declarative-metadata.md)
- [Investigation Workspace Alignment](./investigation-workspace-alignment.md)
- [eveSMLM Pipeline](./evesmlm.md)
