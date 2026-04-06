# ADR 003: Keep Reconstruction Accumulation In The Plugin And Rendering In The Host

## Status

Accepted

Superseded in part by ADR 004 for the concrete transport mechanism.

## Context

Reconstruction needs to accumulate localizations across many frames, render them into a separate viewport, and export both tables and images. The per-frame `HostContext` is not a good place to shuttle an ever-growing reconstruction table back through the pipeline every update.

## Decision

Keep reconstruction as a split responsibility:

1. `augur-plugin-reconstruction` owns accumulation of `LocalizationResults`.
2. `augur-gui` owns rendering, viewport state, file dialogs, and export formats.
3. The concrete host/plugin transport for the accumulated data is the generic host-view dataset path documented in ADR 004, not a reconstruction-only callback.

## Consequences

- The reconstruction table stays close to the scientific logic that creates it.
- The GUI can add rendering controls and export formats without changing the per-frame plugin context.
- The feature still requires a matching host/API build; the plugin alone is not sufficient.
