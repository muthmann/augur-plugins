# ADR 003: Keep Reconstruction Accumulation In The Plugin And Rendering In The Host

## Status

Accepted

## Context

Reconstruction needs to accumulate localizations across many frames, render them into a separate viewport, and export both tables and images. The existing `HostContext` is scoped to one frame and is not a good place to shuttle an ever-growing reconstruction table back through the pipeline every update.

## Decision

Implement reconstruction as a split responsibility:

1. `augur-plugin-reconstruction` accumulates `LocalizationResults` and exposes a serialized `LocalizationTable` through a new runtime plugin hook.
2. `augur-gui` queries enabled runtime plugins for that accumulated table and owns reconstruction rendering, viewport state, file dialogs, and export formats.

## Consequences

- The reconstruction table stays close to the scientific logic that creates it.
- The GUI can add rendering controls and export formats without changing the per-frame plugin context.
- The feature requires a matching host/API build; the plugin alone is not sufficient.
