# Runtime Plugin API

The plugin API now lives in the `augur-plugin-api` crate from `augur-rs`.

## Main Types

- `Plugin`: safe Rust trait implemented by plugin crates
- `PluginFrame`: borrowed access to preview pixels and optional raw events
- `HostOutput`: callbacks for overlays and warnings
- `HostContext`: string-keyed publish/get API for inter-plugin data
- `SettingsSchema`: declarative settings description
- `StatusEntry`: read-only status rows and sparklines
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

Plugins can declare host-rendered datasets and views through `host_views()` and serve snapshots through `host_view_dataset()`:

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
        views: vec![
            HostViewDescriptor {
                id: "example.table.compact".into(),
                title: "Current Rows".into(),
                dataset_id: "example.table".into(),
                placement: HostViewPlacement::AnalysisPanel,
                kind: HostViewKind::CompactTable,
            },
            HostViewDescriptor {
                id: "example.table.window".into(),
                title: "Example Table".into(),
                dataset_id: "example.table".into(),
                placement: HostViewPlacement::Window,
                kind: HostViewKind::TableWindow,
            },
        ],
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
```

This keeps plugin-owned scientific state on the plugin side while letting the host render panel sections, read-only windows, CSV export, and density views generically.

## Compatibility Hook

`accumulated_localizations()` remains available as a deprecated compatibility hook for one transition cycle. New in-tree plugins should use `host_views()` instead.

`LocalizationTable` and `LocalizationRow` are still defined in `augur-plugin-api` and map directly to the ThunderSTORM CSV format for cross-tool compatibility with older hosts.

## Panic Safety

`export_plugin!` wraps every vtable call in `std::panic::catch_unwind`. A panic inside a plugin function is caught at the FFI boundary rather than unwinding into host code, which would be undefined behaviour. The host logs the panic and treats the current frame as a no-op for that plugin.

Do not rely on panics for control flow. Use `Result` and return errors through `set_setting()` or warnings through `HostOutput`.

## Migration from the Old API

Replace:

- `AnalysisPlugin` with `Plugin`
- direct `egui` calls with `SettingsSchema`
- typed `PluginContext` exchange with `HostContext`
- compile-time registration with `export_plugin!` plus a built `cdylib`
