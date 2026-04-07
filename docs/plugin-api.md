# Runtime Plugin API

This repository now follows the runtime-only plugin surface documented in `augur-rs`.

Use the upstream guide as the canonical contract:

- [`augur-rs/docs/features/plugin-authoring-guide.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md)

This page summarizes the parts authors working in `augur-plugins` touch most often.

## Core Types

- `Plugin`
- `export_plugin!`
- `PluginFrame`
- `HostOutput`
- `HostContext`
- `EventStoreHandle`
- `PluginCapabilities`
- `SettingsSchema` / `StatusEntry`
- `HostViewRegistry`
- `GlobalSettings`
- `TableDatasetV1`
- `Image2dV1`
- `Series1dV1`
- `CTX_GLOBAL_SETTINGS`

## Minimal Plugin

```rust
use augur_plugin_api::{
    export_plugin, EventStoreHandle, HostContext, HostOutput, Plugin, PluginFrame,
};

#[derive(Default)]
struct MyPlugin {
    enabled: bool,
}

impl Plugin for MyPlugin {
    fn name(&self) -> &'static str { "My Plugin" }
    fn enabled(&self) -> bool { self.enabled }
    fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }
    fn reset(&mut self) {}

    fn process_frame(
        &mut self,
        _frame: &PluginFrame<'_>,
        _output: &mut HostOutput<'_>,
        _context: &mut HostContext<'_>,
        _event_store: &EventStoreHandle<'_>,
    ) {
    }
}

export_plugin!(MyPlugin);
```

## Execution Model

`input_kind()` and retained history are separate concerns.

### Frame input phase

| Phase | When to use |
|---|---|
| `FrameOnly` | Preview-only overlays or pixel-domain work |
| `RawEvents` | Needs current-frame raw `CdEvent` input |
| `DerivedData` | Consumes data published by an upstream plugin |

### Optional retained history

Use `capabilities()` when the plugin needs host-retained event history:

```rust
use augur_plugin_api::PluginCapabilities;

fn capabilities(&self) -> PluginCapabilities {
    PluginCapabilities {
        retained_event_history: true,
    }
}
```

`PluginInput::RawEvents` means “give me current-frame raw events.”
`retained_event_history: true` means “keep the host-owned history buffer available.”

## Shared Data

Plugins exchange JSON-serialized payloads under string keys:

```rust
context.publish("my.plugin.results", &results)?;
let upstream = context.get::<MyResults>("my.plugin.results")?;
```

Prefer standard shared payloads such as `CTX_LOCALIZATION_RESULTS` when they exist. The standard localization payload now lives in `augur-plugin-types`. If several plugins need the same domain-specific type, put that type in a companion crate instead of copying it into multiple plugin crates.

Persistent helpers are still available for plugin-owned caches, but shared scientific outputs should normally stay on the per-frame context bus.

## Host-Owned Global Settings

The host now publishes shared runtime settings on the normal context bus:

- key: `CTX_GLOBAL_SETTINGS`
- type: `GlobalSettings`

Example:

```rust
use augur_plugin_api::{GlobalSettings, CTX_GLOBAL_SETTINGS};

if let Some(globals) = context.get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)? {
    let nm_per_pixel = globals.nm_per_pixel;
    let sensor_width = globals.sensor_width;
    let sensor_height = globals.sensor_height;
    let acq_time_ms = globals.acq_time_ms;
    let event_store_budget_bytes = globals.event_store_budget_bytes;
    let _ = (
        nm_per_pixel,
        sensor_width,
        sensor_height,
        acq_time_ms,
        event_store_budget_bytes,
    );
}
```

New plugins should prefer `GlobalSettings` over duplicating host-owned defaults such as pixel scale or sensor geometry.

Plugins must tolerate `None` when run against an older host build.

## Dependencies

Override `dependencies()` only when the plugin truly requires a specific upstream producer by name:

```rust
fn dependencies(&self) -> &[&'static str] {
    &["EVE Candidate Finding"]
}
```

If the plugin can degrade gracefully when an upstream payload is absent, prefer a runtime warning over a hard dependency declaration.

## Settings And Status

Plugins describe settings declaratively through:

- `settings_schema()`
- `get_setting()`
- `set_setting()`
- optional `status_entries()`

Common setting kinds include `Bool`, slider/drag values, and `Enum`. The host owns rendering and persistence of the UI state.

## Host Views

Plugins can declare host-rendered datasets and views through `host_views()` and serve snapshots through `host_view_dataset()`.

### Dataset Kinds

- `HostDatasetKind::TableV1`
- `HostDatasetKind::Image2dV1`
- `HostDatasetKind::Series1dV1`

### View Kinds

- `HostViewKind::CompactTable`
- `HostViewKind::TableWindow`
- `HostViewKind::Density2dFromTable`
- `HostViewKind::Scatter2dFromTable`
- `HostViewKind::ImageWindow`
- `HostViewKind::LineSeriesWindow`

### Example

```rust
fn host_views(&self) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: "example.table".into(),
            title: "Example Table".into(),
            kind: HostDatasetKind::TableV1(TableSchema {
                columns: vec![
                    TableColumn {
                        id: "x".into(),
                        title: "X".into(),
                        value_type: TableValueType::F64,
                    },
                    TableColumn {
                        id: "y".into(),
                        title: "Y".into(),
                        value_type: TableValueType::F64,
                    },
                ],
                coordinate_space_2d: None,
            }),
            empty_message: "No rows yet.".into(),
        }],
        views: vec![HostViewDescriptor {
            id: "example.table.compact".into(),
            title: "Current Rows".into(),
            dataset_id: "example.table".into(),
            placement: HostViewPlacement::AnalysisPanel,
            kind: HostViewKind::CompactTable,
        }],
    }
}

fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
    if dataset_id != "example.table" {
        return None;
    }

    let dataset = TableDatasetV1::new(vec![
        TableColumnData {
            column_id: "x".into(),
            values: TableColumnValues::F64(vec![1.0, 2.0]),
        },
        TableColumnData {
            column_id: "y".into(),
            values: TableColumnValues::F64(vec![3.0, 4.0]),
        },
    ]).ok()?;

    serde_json::to_vec(&dataset).ok()
}

fn host_view_dataset_generation(&self, dataset_id: &str) -> u64 {
    if dataset_id == "example.table" { 1 } else { 0 }
}
```

`host_view_dataset_generation()` is optional but recommended when the host should invalidate a cached snapshot only after the dataset changes.

The host owns rendering, exports, caching, and window state. Plugins do not render `egui` directly.

## Event History

`process_frame()` always receives `event_store: &EventStoreHandle<'_>`. Plugins that need only the current frame can ignore it. History-aware plugins can query:

- `frame_count()`
- `frame(index)`
- `frames()`
- `frame_range_for_timestamps(start_us, end_us)`
- `frames_in_range(start_us, end_us)`
- `collect_events_in_range(start_us, end_us, out)`
- `oldest_timestamp_us()`

## Migration From Older Plugin Code

When porting older code, replace:

- `AnalysisPlugin` with `Plugin`
- typed `PluginContext` exchange with `HostContext`
- direct `egui` UI code with declarative settings/status
- special-case host rendering hooks with `host_views()` and `host_view_dataset()`
- duplicated host-owned calibration values with `GlobalSettings`
- compile-time registration with `export_plugin!` plus a built `cdylib`
