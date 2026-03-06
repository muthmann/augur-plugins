# ADR 002: Split eveSMLM Into Three Plugins

## Status

Accepted

## Context

The EVE workflow for eveSMLM is a multi-stage pipeline: candidate finding on raw events, sub-pixel fitting of those candidates, and post-processing / quality evaluation. A single monolithic plugin would hide the boundaries between those steps and make it difficult to compare alternative algorithms or to reuse only part of the pipeline.

## Decision

Implement the workflow as three plugins:

1. `augur-plugin-evesmlm-candidates`
2. `augur-plugin-evesmlm-fitting`
3. `augur-plugin-evesmlm-postproc`

Each plugin owns one analysis concern, publishes typed results through `PluginContext`, and depends only on the immediately preceding stage.

## Consequences

- Researchers can inspect candidate counts separately from fit counts and post-processing rejections.
- New fitting or post-processing methods can be added without rewriting candidate discovery.
- The existing `LocalizationResults` type can still be published for compatibility with current downstream plugins.
- The integration cost in `augur-gui` stays low: one dependency and one registration line per plugin.
