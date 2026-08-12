# Plugin Architecture

This document summarizes how the current AugurRS runtime plugin system maps onto the crates in this repository.

For the full host-side contract, use the upstream authoring guide:

- [`augur-rs/docs/features/plugin-authoring-guide.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md)

## Repository Role

- `augur-rs` owns `augur-plugin-api`, the loader, Plugin Manager, host views, and host-owned built-in tools.
- `augur-plugins` owns the runtime plugin implementations and the template crate used to start new plugins.
- Shared domain payloads should live in companion crates when multiple plugins need the same types.

## Runtime Packaging

Each installed runtime plugin ships as:

- a `plugin.toml` manifest
- one platform library (`.dylib`, `.so`, or `.dll`)

Installed layout:

```text
~/.augur/plugins/
  localization/
    plugin.toml
    libaugur_plugin_localization.dylib
```

The Plugin Manager can scan, enable, disable, and reload plugins without recompiling `augur-gui`.

## Execution Model

Plugins still declare their per-frame phase through `input_kind()`:

1. `FrameOnly`
2. `RawEvents`
3. `DerivedData`

Current in-tree examples:

- `RawEvents`: `localization`, `evesmlm-candidates`
- `DerivedData`: `reconstruction`, `focus-metrics`, `evesmlm-fitting`, `evesmlm-postproc`

Raw-event access and retained history are separate concerns. A plugin may:

- request current-frame raw events with `PluginInput::RawEvents`
- request host-retained history with `PluginCapabilities { retained_event_history: true }`

This keeps the default host path cheap when no enabled plugin needs retained history.

## Shared Data And Global Settings

`HostContext` is the string-keyed JSON bus that plugins use to exchange typed payloads within a frame.

Key properties:

- string keys are stable across dynamic-library boundaries
- JSON payloads keep the ABI surface small
- standard shared payloads such as `CTX_LOCALIZATION_RESULTS` can live in companion crates such as `augur-plugin-types`
- host-owned experiment settings are published on `CTX_GLOBAL_SETTINGS` as `GlobalSettings`

`GlobalSettings` currently includes:

- `nm_per_pixel`
- `sensor_width`
- `sensor_height`
- `acq_time_ms`
- `event_store_budget_bytes`
- `record_sensor_telemetry`
- active ROI and masked pixels
- event-filter state

New plugins should prefer this shared host contract over duplicating pixel scale or sensor geometry in plugin-local defaults.

## Frame-Independent Plugin Services

Hardware workflows use the host-routed control plane rather than the per-frame
JSON context. One canonical live-worker instance owns effects; GUI mirrors,
replay, and offline instances remain fail-closed. Requests target stable manifest
IDs and carry semantic operation names, request IDs, leases, and explicit
responses. Device plugins validate and execute their own operations; the host is
only the router.

Stage-A uses a serde-only companion contract so `stage-a-a1` can orchestrate
`stage-a-modulation` and `stage-a-photodiode` without linking their implementation
crates or opening their serial ports. See
[`docs/adr/007-stage-a-owner-orchestration.md`](./adr/007-stage-a-owner-orchestration.md).

Not every cross-plugin dependency needs that machinery. Because the host
broadcasts each plugin's `control_snapshots()` to every plugin's inbox, a plugin
that only needs to *read* another's published state can do so directly — no
lease, no service request, no router round-trip. The Pockels transfer
calibration reads photodiode levels this way while driving only its own DAC:
[`docs/adr/011-stage-a-pockels-transfer-calibration.md`](./adr/011-stage-a-pockels-transfer-calibration.md).
Reserve the leased service path for *commanding* hardware someone else owns.

A published field is part of that contract, so its **meaning must not depend on
the publisher's UI state**. The photodiode plugin's display toggle used to
select the optical geometry the published log-contrast was computed in, which
silently retargeted A1's amplitude sweep whenever the chart was left on its
default. Geometry follows the bench, not the display:
[`docs/adr/012-stage-a-contrast-geometry-is-bench-not-display.md`](./adr/012-stage-a-contrast-geometry-is-bench-not-display.md).

## Host Views

Plugins declare host-rendered datasets and views through:

- `host_views()`
- `host_view_dataset(dataset_id)`
- optional `host_view_dataset_generation(dataset_id)`

The host owns:

- analysis-panel rendering
- linked 2D/3D investigation state
- standalone windows
- dataset caching
- exports
- window state

This repository currently uses that mechanism for:

- reconstruction table, density, and 3D localization inspection
- candidate-stage accepted/rejected raw-event layers for live tuning
- the shared EVE current-localization datasets that can be provided by fitting or post-processing

For investigation-linked table datasets, the important host-consumed metadata is:

- stable row ids via `row_id_column`
- 2D and 3D coordinates
- optional time columns
- layer ids and semantic labels
- dataset display metadata for title, default visibility, color, marker shape, and size

Overlays remain useful for supplemental 2D annotations, but the host now treats structured datasets as the primary linking surface.

## Tradeoffs

| Decision | Benefit | Cost |
|---|---|---|
| Runtime loading | Plugins can be built and installed independently of the host | Host and plugin builds must remain ABI-compatible |
| String-keyed JSON context | Works cleanly across dynamic-library boundaries | Producer and consumer must agree on key names and payload schema |
| Declarative host views | Plugins keep scientific state; the host keeps rendering/export UX | Plugin and host must share identical dataset metadata |
| Host-owned global settings | One source of truth for calibration and runtime settings | Plugins must tolerate `None` when run against older hosts |

## Local Authoring Guidance

- Start from `plugin-template/`.
- Keep each plugin focused on one analysis concern.
- Reuse standard payloads where possible.
- Document context keys, host views, and any calibration assumptions in the plugin README.
