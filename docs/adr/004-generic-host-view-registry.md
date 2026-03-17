# ADR 004: Replace Reconstruction-Specific Host Hooks With A Generic Host View Registry

## Status

Accepted

## Context

The first reconstruction integration added a dedicated `accumulated_localizations()` hook for one host-rendered window. That solved the immediate reconstruction use case, but it kept the host UI coupled to one plugin capability and offered no generic path for panel views or other host-rendered windows.

## Decision

Move plugin-owned host rendering metadata to a generic dataset/view registry:

1. plugins declare datasets and views through `host_views()`
2. plugins serve snapshots through `host_view_dataset(dataset_id)`
3. the host resolves duplicate ids in normal plugin execution order
4. multiple views can share one dataset, so reconstruction density and full-table windows stay aligned on the same accumulated source of truth

## Consequences

- reconstruction becomes one consumer of a generic host-view mechanism instead of a special case
- EVE stages can publish one shared compact panel view id, with the later active stage taking precedence
- host-rendered UI stays capability-gated by enabled plugins
- plugins and host must share the same schema metadata for reused dataset/view ids
