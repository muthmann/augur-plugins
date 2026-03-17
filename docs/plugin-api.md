# Runtime Plugin API

The plugin API now lives in the `augur-plugin-api` crate from `augur-rs`.

## Main Types

- `Plugin`: safe Rust trait implemented by plugin crates
- `PluginFrame`: borrowed access to preview pixels and optional raw events
- `HostOutput`: callbacks for overlays and warnings
- `HostContext`: string-keyed publish/get API for inter-plugin data
- `SettingsSchema`: declarative settings description
- `StatusEntry`: read-only status rows and sparklines
- `HostViewRegistry`: declarative dataset/view metadata for host-rendered analysis views
- `export_plugin!`: exports the C vtable expected by `augur-gui`

## Minimal Plugin

```rust
use augur_plugin_api::{export_plugin, HostContext, HostOutput, Plugin, PluginFrame};

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
    ) {
    }
}

export_plugin!(MyPlugin);
```

## Execution Phases

Override `input_kind()` to declare which phase the plugin runs in:

| Phase | When to use |
|---|---|
| `FrameOnly` | Overlays, pixel statistics, cheap preview-only analysis. No event materialization. |
| `RawEvents` | Requires the raw `CdEvent` stream. The pipeline only materialises events when at least one enabled plugin requests them. |
| `DerivedData` | Consumes results published by an upstream plugin via `HostContext`. Runs after all `FrameOnly` and `RawEvents` plugins. |

Use the earliest phase that satisfies your needs. The default is `FrameOnly`.

## Dependencies

Override `dependencies()` to declare which upstream plugins your plugin requires by name:

```rust
fn dependencies(&self) -> &[&'static str] {
    &["Molecule Localization"]
}
```

The Plugin Manager uses this list to show dependency relationships. Only declare a hard dependency when the plugin cannot run at all without the named producer. If the plugin degrades gracefully when the upstream payload is absent, prefer a runtime warning instead.

## Settings

Settings are described by schema, not direct UI code.

Supported item kinds:

| Kind | egui widget |
|---|---|
| `Bool` | checkbox |
| `F64Slider` | slider with float range |
| `I64Slider` | slider with integer range |
| `F64Drag` | drag value with float range and speed |
| `I64Drag` | drag value with integer range |
| `Enum` | row of radio buttons |

All kinds accept optional `suffix` (unit label) and `tooltip` strings.

When a setting changes, `augur-gui` calls `set_setting()` with a JSON value.

## Shared Data

Shared data crosses the plugin boundary as JSON bytes under string keys.

Example:

```rust
context.publish("my.plugin.results", &results)?;
let upstream = context.get::<MyResults>("my.plugin.results")?;
```

Custom shared types must derive `serde::Serialize` and `serde::Deserialize`.

Common built-in keys and types, such as localization results, live in `augur-plugin-api`.
If you want downstream plugins like Focus Metrics to consume your results, publish the standard `CTX_LOCALIZATION_RESULTS` payload in addition to any plugin-specific data.

## Status Entries

`status_entries()` returns a `Vec<StatusEntry>` rendered below the settings panel. Three variants are available:

- `StatusEntry::Text(String)` — plain label row
- `StatusEntry::LabeledValue { label, value, color: Option<[u8; 3]> }` — key-value row with optional RGB highlight color
- `StatusEntry::Sparkline { label, values: Vec<f64>, lower_is_better: bool }` — inline history plot; `lower_is_better` controls the coloring direction

## Host Views

Plugins can expose structured results to the host through host-rendered views. This replaces the old reconstruction-specific hook with a generic model that works for any analysis output.

Two `Plugin` trait methods drive the registry:

- `host_views()` returns dataset and view descriptors
- `host_view_dataset(dataset_id)` returns a serialized payload on demand

### Descriptors

A `HostViewRegistry` contains two lists:

- `HostDatasetDescriptor`: stable dataset id, title, kind (`HostDatasetKind::TableV1`), and empty-state message
- `HostViewDescriptor`: stable view id, dataset reference, placement, and host-rendered view kind

Available placements:

| Placement | Renders as |
|---|---|
| `HostViewPlacement::AnalysisPanel` | compact table in the analysis side panel |
| `HostViewPlacement::Window` | dedicated host window |

Available view kinds:

| Kind | Use |
|---|---|
| `HostViewKind::CompactTable` | 10-row preview in the analysis panel |
| `HostViewKind::TableWindow` | read-only table window with CSV export |
| `HostViewKind::Density2dFromTable { x_column, y_column }` | density heatmap derived from two numeric columns |

### Example

```rust
use augur_plugin_api::{
    HostDatasetDescriptor, HostDatasetKind, HostViewDescriptor, HostViewKind,
    HostViewPlacement, HostViewRegistry, TableColumn, TableColumnData,
    TableColumnValues, TableDatasetV1, TableSchema, TableValueType,
};

fn host_views(&self) -> HostViewRegistry {
    HostViewRegistry {
        datasets: vec![HostDatasetDescriptor {
            id: "molecules.table".into(),
            title: "Localized Molecules".into(),
            kind: HostDatasetKind::TableV1(TableSchema {
                columns: vec![
                    TableColumn {
                        id: "frame".into(),
                        title: "Frame".into(),
                        value_type: TableValueType::U64,
                    },
                    TableColumn {
                        id: "x_nm".into(),
                        title: "X [nm]".into(),
                        value_type: TableValueType::F64,
                    },
                    TableColumn {
                        id: "y_nm".into(),
                        title: "Y [nm]".into(),
                        value_type: TableValueType::F64,
                    },
                ],
                coordinate_space_2d: None,
            }),
            empty_message: "No localizations available yet.".into(),
        }],
        views: vec![
            HostViewDescriptor {
                id: "molecules.panel".into(),
                title: "Localization Preview".into(),
                dataset_id: "molecules.table".into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            },
            HostViewDescriptor {
                id: "molecules.window".into(),
                title: "Localization Density".into(),
                dataset_id: "molecules.table".into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::Density2dFromTable {
                    x_column: "x_nm".into(),
                    y_column: "y_nm".into(),
                },
            },
        ],
    }
}
```

### Dataset Payloads

`host_view_dataset()` returns `Option<Vec<u8>>` — JSON-serialized `TableDatasetV1` bytes for the requested dataset id. The host calls this lazily, only when a visible panel or open window needs the data.

```rust
fn host_view_dataset(&self, dataset_id: &str) -> Option<Vec<u8>> {
    if dataset_id != MY_DATASET_ID {
        return None;
    }
    serde_json::to_vec(&TableDatasetV1::new(vec![
        TableColumnData {
            column_id: "x_nm".into(),
            values: TableColumnValues::F64(self.x_values.clone()),
        },
    ]).unwrap()).ok()
}
```

### Shared Dataset IDs

When multiple plugins declare the same dataset and view ids, the host resolves duplicates in plugin execution order: later providers override earlier ones only when descriptor metadata matches exactly. Conflicting duplicate ids are ignored and logged.

This lets pipeline stages like EVE Candidate Fitting and EVE Post-Processing share a single compact table view — the later stage becomes the active provider when enabled.

See [augur-rs ADR 006](https://github.com/muthmann/augur-rs/blob/main/docs/adr/006-host-view-registry.md) for full design rationale.

## Accumulated Data (Deprecated)

> **Deprecated.** Prefer `host_views()` and `host_view_dataset()` for new plugins. The `accumulated_localizations()` hook is retained for one transition cycle.

Plugins that accumulate results across frames can expose them through `accumulated_localizations()`:

```rust
fn accumulated_localizations(&self) -> Option<Vec<u8>> {
    serde_json::to_vec(&my_table).ok()
}
```

The method returns serialized `LocalizationTable` bytes. The default implementation returns `None`.

`LocalizationTable` and `LocalizationRow` are defined in `augur-plugin-api` and map directly to the ThunderSTORM CSV format for cross-tool compatibility.

## Panic Safety

`export_plugin!` wraps every vtable call in `std::panic::catch_unwind`. A panic inside a plugin function is caught at the FFI boundary rather than unwinding into host code, which would be undefined behaviour. The host logs the panic and treats the current frame as a no-op for that plugin.

Do not rely on panics for control flow. Use `Result` and return errors through `set_setting()` or warnings through `HostOutput`.

## Migration from the Old API

Replace:

- `AnalysisPlugin` with `Plugin`
- direct `egui` calls with `SettingsSchema`
- typed `PluginContext` exchange with `HostContext`
- compile-time registration with `export_plugin!` plus a built `cdylib`
