# Contributing to augur-plugins

This repository now targets AugurRS runtime-loaded plugins.

You do not add crates to `augur-gui` anymore. A compatible plugin builds as a `cdylib`, ships a `plugin.toml`, and is loaded by `augur-gui` from `~/.augur/plugins/`.

## What Counts as Compatible

A runtime-compatible plugin:

- depends on `augur-plugin-api`
- implements `augur_plugin_api::Plugin`
- exports `augur_plugin_vtable` via `export_plugin!(MyPlugin)`
- builds with `crate-type = ["cdylib", "rlib"]`
- exposes settings through `SettingsSchema` instead of direct `egui`
- derives `Serialize` and `Deserialize` for any custom payloads published through `HostContext`

If a plugin still implements the old `AnalysisPlugin` trait or expects to be compiled into `augur-gui`, moving its source folder into `~/.augur/plugins/` will not work.

## Writing a New Plugin

### 1. Start From the Template

```bash
cp -r plugin-template plugins/my-plugin
```

Rename the crate, update `plugin.toml`, and add the crate to the workspace `members` list in the root `Cargo.toml`.

### 2. Implement `augur_plugin_api::Plugin`

The plugin trait is now the safe Rust layer over the FFI boundary:

```rust
use augur_plugin_api::{EventStoreHandle, HostContext, HostOutput, Plugin, PluginFrame};

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
        frame: &PluginFrame<'_>,
        output: &mut HostOutput<'_>,
        context: &mut HostContext<'_>,
        event_store: &EventStoreHandle<'_>,
    ) {
        let _ = (frame, output, context, event_store);
    }
}
```

Export it with:

```rust
use augur_plugin_api::export_plugin;
export_plugin!(MyPlugin);
```

`export_plugin!` wraps every vtable call in `std::panic::catch_unwind`, so a panic inside plugin code is caught at the FFI boundary instead of unwinding into `augur-gui`. Use `Result` and warnings for expected error paths; do not rely on panics for control flow.

### 3. Choose the Execution Phase

Return the right `PluginInput` from `input_kind()`:

| Phase | Use |
|---|---|
| `FrameOnly` | Overlays, pixel statistics, cheap preview-only work. No event materialization cost. |
| `RawEvents` | Requires the raw `CdEvent` stream (e.g. event-domain reconstruction). |
| `DerivedData` | Reads results published by an upstream plugin via `HostContext`. |

Use the earliest phase that satisfies your needs. The default is `FrameOnly`.

### 4. Share Data Through `HostContext`

Dynamic plugins exchange JSON-serialized data under string keys:

```rust
use augur_plugin_api::CTX_LOCALIZATION_RESULTS;

context.publish(CTX_LOCALIZATION_RESULTS, &results)?;
let upstream = context.get::<MyResults>("my.plugin.results")?;
```

Persistent cross-frame state uses the companion helpers:

```rust
context.publish_persistent("my.plugin.state", &state)?;
let state = context.get_persistent::<MyState>("my.plugin.state")?;
```

Declare downstream requirements with `dependencies()` when a downstream plugin truly requires a specific upstream producer. If your plugin can consume any producer of a standard payload such as `CTX_LOCALIZATION_RESULTS`, prefer a runtime warning over a hard name-based dependency.

### 5. Define Declarative Settings

Plugins no longer render `egui` directly. Instead, expose:

- `settings_schema() -> SettingsSchema`
- `get_setting()`
- `set_setting()`
- optional `status_entries()`

See `plugins/hotpixel`, `plugins/localization`, `plugins/focus-metrics`, and the `plugins/evesmlm-*` chain for working examples.

### 6. Write `plugin.toml`

Use the runtime format:

```toml
name = "My Plugin"
version = "0.2.0"
description = "One-line summary visible in the Plugin Manager."
domain = "general"
library = "augur_plugin_my_plugin"
```

`library` is the library base name without `lib` or the platform extension.

### 7. Build the Plugin

```bash
cargo build -p augur-plugin-my-plugin --release
```

That should produce:

- macOS: `target/release/libaugur_plugin_my_plugin.dylib`
- Linux: `target/release/libaugur_plugin_my_plugin.so`
- Windows: `target/release/augur_plugin_my_plugin.dll`

### 8. Install It Locally

```bash
mkdir -p ~/.augur/plugins/my-plugin
cp plugins/my-plugin/plugin.toml ~/.augur/plugins/my-plugin/
cp target/release/libaugur_plugin_my_plugin.dylib ~/.augur/plugins/my-plugin/
```

Then launch `augur-gui` and use **Plugins → Scan for New Plugins**.

## Migrating a Legacy Plugin

If your plugin still uses the old compile-time model, port it with this checklist:

1. Replace the old `AnalysisPlugin` implementation with `augur_plugin_api::Plugin`.
2. Remove direct `egui` UI code and replace it with `SettingsSchema` plus `status_entries`.
3. Replace typed `PluginContext` calls with `HostContext` and string keys.
4. Add `export_plugin!(YourPlugin)`.
5. Change the crate to `crate-type = ["cdylib", "rlib"]`.
6. Build the release library and install the compiled artifact into `~/.augur/plugins/<name>/`.

Moving a legacy source folder into `~/.augur/plugins/` is not enough.

## Testing

Recommended checks:

```bash
cargo fmt --all -- --check
cargo build -p augur-plugin-my-plugin --release
cargo test -p augur-plugin-my-plugin
```

Then verify in `augur-gui`:

- the plugin appears in Plugin Manager
- enable/disable works
- settings render correctly
- reload works after rebuilding

## Documentation Expectations

At minimum:

- `plugin.toml`
- `README.md`
- `Cargo.toml`
- `src/lib.rs`

If the plugin publishes shared data, document the context key and payload type explicitly.

## Host-Side Documentation

The host application, runtime loader, and `augur-plugin-api` crate live in [augur-rs](https://github.com/muthmann/augur-rs). Useful references:

- [Plugin Architecture](https://github.com/muthmann/augur-rs/blob/main/docs/features/analysis-plugins.md) — execution model, context bus, FFI API surface
- [Dynamic Plugin Loading](https://github.com/muthmann/augur-rs/blob/main/docs/features/dynamic-plugins.md) — manifest format, install layout, troubleshooting
- [Plugin API Reference](./docs/plugin-api.md) — trait methods, phases, settings, status entries

## Notes for Local Development

This workspace currently points at the sibling `../augur-rs` checkout so the plugin crates can
track in-flight host/API branches during coordinated development.

If you need to switch back to the published Git source later, update the workspace dependencies in
the root `Cargo.toml` or use a local `[patch]` override during migration.
