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

## Settings

Settings are described by schema, not direct UI code.

Supported item kinds:

- `Bool`
- `F64Slider`
- `I64Slider`
- `F64Drag`
- `I64Drag`
- `Enum`

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

## Migration from the Old API

Replace:

- `AnalysisPlugin` with `Plugin`
- direct `egui` calls with `SettingsSchema`
- typed `PluginContext` exchange with `HostContext`
- compile-time registration with `export_plugin!` plus a built `cdylib`
