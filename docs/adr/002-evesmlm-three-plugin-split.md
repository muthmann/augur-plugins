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

Each plugin owns one analysis concern, publishes results through `HostContext` under well-known string keys, and depends only on the immediately preceding stage.

## Consequences

- Researchers can inspect candidate counts separately from fit counts and post-processing rejections.
- New fitting or post-processing methods can be added without rewriting candidate discovery.
- The standard `CTX_LOCALIZATION_RESULTS` payload is republished by the fitting stage for compatibility with downstream plugins such as Focus Metrics.
- Each plugin installs independently as a `cdylib` into `~/.augur/plugins/`; no `augur-gui` source changes are needed.
