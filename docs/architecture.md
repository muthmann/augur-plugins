# Plugin Architecture

This document describes the design rationale behind the AugurRS plugin system, how it compares to plugin systems in other scientific tools, and the tradeoffs involved.

## Design Goals

1. **Keep `augur-core` domain-free.** The camera SDK knows nothing about microscopy, localization, or any specific analysis domain. All domain logic lives in plugins.
2. **Runtime loading without recompilation.** Plugins build as `cdylib` crates and are loaded from `~/.augur/plugins/` at startup. Researchers install, enable, or swap a plugin without rebuilding `augur-gui`.
3. **Familiar to researchers.** The plugin API is designed to feel natural to anyone who has written an ImageJ plugin, a napari extension, or a Micro-Manager device adapter.
4. **Minimal install overhead.** Installing a plugin is: build the release library, drop it plus a `plugin.toml` into `~/.augur/plugins/<name>/`, and click **Scan for New Plugins**.

## Runtime Model

Plugins are compiled as `cdylib` crates that export a C-compatible vtable via `export_plugin!`. `augur-gui` loads them from:

```text
~/.augur/plugins/
  hotpixel/
    plugin.toml
    libaugur_plugin_hotpixel.dylib   # .so on Linux, .dll on Windows
```

The Plugin Manager in `augur-gui` can scan, enable, disable, and reload plugins without restarting the host application.

## Execution Model

```
Preview frame arrives
    │
    ├─ Phase 1: FrameOnly plugins (cheap, no event materialization)
    │     Hotpixel Detection, ROI Grid
    │
    ├─ Phase 2: RawEvents plugins (raw CdEvent stream available)
    │     EVE Candidate Finding
    │
    └─ Phase 3: DerivedData plugins (consume upstream results)
          Localization, EVE Fitting, EVE Post-Processing, Focus Metrics
```

The three-phase model ensures that upstream plugins always run before downstream consumers within the same frame. This is conceptually similar to ImageJ2's service ordering and napari's contribution layering, but enforced through `PluginInput` phase declarations rather than runtime annotation scanning.

## Context Bus

`HostContext` is a string-keyed publish/get API that plugins use to exchange JSON-serialized data within a single frame:

```rust
context.publish(CTX_LOCALIZATION_RESULTS, &results)?;
let upstream = context.get::<LocalizationResults>(CTX_LOCALIZATION_RESULTS)?;
```

Key design properties:

- String keys are stable across dynamic library boundaries (no `TypeId` mismatch between separately compiled crates)
- JSON serialization via `serde` keeps the API ABI-safe
- Well-known keys (e.g. `CTX_LOCALIZATION_RESULTS`) are declared in `augur-plugin-api` so any plugin can publish or consume the standard payload
- Context is cleared automatically between frames

The design draws on the SciJava parameter injection model, but uses explicit string-keyed registration instead of annotation-based classpath scanning.

## Host-View Registry

Plugins can expose structured outputs to the host through a generic dataset/view registry. Instead of adding domain-specific hooks to the FFI surface, plugins declare descriptors that the host renders generically:

- `host_views()` returns `HostViewRegistry` with dataset and view descriptors
- `host_view_dataset(dataset_id)` returns serialized data on demand

The host owns all rendering (compact tables, table windows, density maps) while plugins own the scientific data. Dataset payloads are fetched lazily. When multiple plugins declare the same descriptor id, later providers override earlier ones in execution order, enabling pipeline stages to share a single view.

See [augur-rs ADR 006](https://github.com/muthmann/augur-rs/blob/main/docs/adr/006-host-view-registry.md) for the full design rationale and [plugin-api.md](./plugin-api.md#host-views) for the API reference.

## Tradeoffs

| Decision | Benefit | Cost |
|---|---|---|
| Runtime loading | No recompilation to add/remove plugins | Vtable must remain ABI-stable; mismatched builds will fail to load |
| Phased execution | Deterministic ordering, no race conditions | Plugins cannot run concurrently within a frame |
| String-keyed context | Works across independently compiled cdylib boundaries | Publisher and consumer must agree on key strings and payload schema |
| Separate repository | Core SDK stays clean, plugins are opt-in | Two repositories to manage |

## Future Directions

- **Registry index:** A machine-readable index of available plugins with version and dependency metadata could enable automated resolution and a community hub similar to napari-hub.
- **Signed plugins:** Cryptographic signing of plugin manifests and libraries for distribution trust.
- **Hot-patching improvements:** Per-plugin state persistence across reloads, so plugin configuration survives a library swap during development.
