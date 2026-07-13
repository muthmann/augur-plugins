# My Plugin

> One-line summary of what this plugin does.

## Overview

Describe what your plugin analyzes, detects, or computes. Include the scientific context if applicable.

## Configuration

| Setting | Default | Description |
|---|---|---|
| Example threshold | `1.0` | What this threshold controls |

If the plugin needs host-owned values such as pixel scale, sensor geometry, acquisition time, or EventStore budget, read `GlobalSettings` from `CTX_GLOBAL_SETTINGS` instead of hardcoding those defaults in the plugin.

## Published Data

If this plugin publishes results to `HostContext` for downstream consumers, describe the key, payload type, and whether it also republishes any standard shared payloads.

## Host Views

If this plugin declares datasets or views through `host_views()`, document the dataset ids, view ids, and expected schema here.

For investigation-linked table datasets, also document:

- which column provides stable row identity
- whether 2D coordinates are exposed
- whether 3D coordinates and time are exposed
- which layer id and display defaults the host should expect

Prefer structured datasets for linked 2D/3D/table workflows. Use overlays as supplemental annotations rather than the only way to inspect results.

## Dependencies

List any hard upstream plugin dependencies this plugin declares through `dependencies()` (or "None").

## References

Cite relevant papers, algorithms, or prior implementations if applicable.
