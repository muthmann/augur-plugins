# Runtime Plugin API

This repository follows the runtime-only plugin surface documented in `augur-rs`.

Use the upstream guide as the canonical contract:

- [`augur-rs/docs/features/plugin-authoring-guide.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md)
- [`augur-rs/docs/features/investigation-workspace.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/investigation-workspace.md)

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

Prefer standard shared payloads such as `CTX_LOCALIZATION_RESULTS` when they exist. If several plugins need the same domain-specific type, put that type in a companion crate instead of copying it into multiple plugin crates.

## Host-Owned Global Settings

The host publishes shared runtime settings on the normal context bus:

- key: `CTX_GLOBAL_SETTINGS`
- type: `GlobalSettings`

New plugins should prefer `GlobalSettings` over duplicating host-owned defaults such as pixel scale or sensor geometry.

## Linked Investigation Datasets

The host now treats structured datasets as the primary integration surface for linked 2D, 3D, and table workflows. Overlays are supplemental.

When a table dataset should participate in linked investigation, populate as many of these additive fields as the plugin can support:

- `TableSchema.coordinate_space_2d`
- `TableSchema.coordinate_space_3d`
- `TableSchema.row_id_column`
- `TableSchema.time_column`
- `TableSchema.layer_id`
- `TableSchema.semantic_label`
- `HostDatasetDescriptor.display`
  - `layer_title`
  - `default_visibility`
  - `default_color`
  - `default_marker_shape`
  - `default_size`

Guidelines:

- Use stable ids from the scientific data when possible.
- Fall back to deterministic plugin-generated ids when no natural id exists.
- Key reusable shared views by dataset id and keep descriptors byte-for-byte identical across providers that intentionally reuse the same ids.
- Prefer dataset/layer ids for styling and visibility instead of plugin-name-specific logic.

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
- `HostViewKind::Scatter3dFromTable`
- `HostViewKind::ImageWindow`
- `HostViewKind::LineSeriesWindow`

### Example

```rust
fn host_views(&self) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: "example.points".into(),
            title: "Example Points".into(),
            kind: HostDatasetKind::TableV1(TableSchema {
                columns: vec![
                    TableColumn {
                        id: "row_id".into(),
                        title: "ID".into(),
                        value_type: TableValueType::U64,
                    },
                    TableColumn {
                        id: "timestamp_us".into(),
                        title: "Timestamp (us)".into(),
                        value_type: TableValueType::U64,
                    },
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
                coordinate_space_2d: Some(TableCoordinateSpace2d {
                    x_column: "x".into(),
                    y_column: "y".into(),
                    x_min: 0.0,
                    x_max: 128.0,
                    y_min: 0.0,
                    y_max: 128.0,
                }),
                coordinate_space_3d: Some(TableCoordinateSpace3d {
                    x_column: "x".into(),
                    y_column: "y".into(),
                    z_column: "timestamp_us".into(),
                    x_min: 0.0,
                    x_max: 128.0,
                    y_min: 0.0,
                    y_max: 128.0,
                    z_min: 0.0,
                    z_max: 5_000.0,
                }),
                row_id_column: Some("row_id".into()),
                time_column: Some("timestamp_us".into()),
                layer_id: Some("example.layer.points".into()),
                semantic_label: Some("points".into()),
            }),
            empty_message: "No rows yet.".into(),
            display: Some(HostDatasetDisplayMetadata {
                layer_title: Some("Example points".into()),
                default_visibility: Some(true),
                default_color: Some([80, 200, 255, 255]),
                default_marker_shape: Some(HostMarkerShape::Point),
                default_size: Some(3.0),
            }),
        }],
        views: vec![HostViewDescriptor {
            id: "example.points.3d".into(),
            title: "Example 3D".into(),
            dataset_id: "example.points".into(),
            placement: HostViewPlacement::Window,
            kind: HostViewKind::Scatter3dFromTable {
                x_column: "x".into(),
                y_column: "y".into(),
                z_column: "timestamp_us".into(),
            },
        }],
    }
}
```

`host_view_dataset_generation()` is optional but recommended when the host should invalidate a cached snapshot only after the dataset changes.

The host owns rendering, exports, caching, and window state. Plugins do not render `egui` directly.

## Marker Overlays

Use structured datasets for the primary linked-workspace model. Use overlays when the plugin needs extra 2D annotations or hit-testing that supplements the dataset.

Current overlay helpers:

- `add_highlight_pixels(...)`
- `add_crosshair_markers(...)`
- `add_marker_overlay(...)`
- `add_warning(...)`

`add_marker_overlay(...)` supports:

- point, cross, box, ellipse, diamond, and filled-circle shapes
- per-item color and size
- optional timestamp
- optional stable id
- optional dataset id, layer id, and source label

That makes it the right choice when a 2D preview annotation should resolve back into the same host selection model.

## Event History

`process_frame()` always receives `event_store: &EventStoreHandle<'_>`. Plugins that need only the current frame can ignore it. History-aware plugins can query:

- `frame_count()`
- `frame(index)`
- `frames()`
- `frame_range_for_timestamps(start_us, end_us)`
- `frames_in_range(start_us, end_us)`
- `collect_events_in_range(start_us, end_us, out)`
- `oldest_timestamp_us()`

## Migration Notes

When porting older code:

- replace `AnalysisPlugin` with `Plugin`
- replace typed `PluginContext` exchange with `HostContext`
- replace direct `egui` UI code with declarative settings/status
- replace special-case host rendering hooks with `host_views()` / `host_view_dataset()`
- replace row-index-based linking assumptions with stable row ids where possible
- replace plugin-name-based styling assumptions with dataset/layer metadata
- keep overlays as supplemental annotations, not the primary data contract
