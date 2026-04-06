# Contributing to augur-plugins

This repository tracks the dynamic runtime plugin system used by AugurRS.

Use the upstream host documentation as the canonical contract:

- [`augur-rs/docs/features/plugin-authoring-guide.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md)
- [`augur-rs/docs/features/global-settings-menu.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/global-settings-menu.md)

This document only adds the repo-local workflow and conventions for plugin crates that live here.

## Repository Scope

- Each runtime plugin lives in its own crate under `plugins/`.
- `plugin-template/` is the starting point for new crates.
- `augur-rs` owns `augur-plugin-api`, the dynamic loader, the Plugin Manager, host views, and host-owned tools such as hotpixel detection.
- Shared data types that multiple plugins consume should live in a companion crate when they do not belong in `augur-plugin-api`.

## New Plugin Workflow

### 1. Start from the template

```bash
cp -r plugin-template plugins/my-plugin
```

Rename the crate, update `plugin.toml`, and add the crate to the workspace `members` list in the root `Cargo.toml`.

### 2. Implement `augur_plugin_api::Plugin`

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

Export the runtime vtable with:

```rust
use augur_plugin_api::export_plugin;
export_plugin!(MyPlugin);
```

### 3. Choose the execution phase and optional capabilities

`input_kind()` declares whether the plugin needs:

| Phase | Use |
|---|---|
| `FrameOnly` | Preview-only overlays or pixel-domain work |
| `RawEvents` | Current-frame raw `CdEvent` access |
| `DerivedData` | Data published by an upstream plugin |

Retained event history is a separate opt-in:

```rust
use augur_plugin_api::PluginCapabilities;

fn capabilities(&self) -> PluginCapabilities {
    PluginCapabilities {
        retained_event_history: true,
    }
}
```

Use `RawEvents` only when you need current-frame raw events. Use `retained_event_history` only when you need host-retained history.

### 4. Share data through `HostContext`

Dynamic plugins exchange JSON-serialized payloads under string keys:

```rust
context.publish("my.plugin.results", &results)?;
let upstream = context.get::<MyResults>("my.plugin.results")?;
```

Prefer standard shared payloads such as `CTX_LOCALIZATION_RESULTS` when they exist. Those shared payload types may live in companion crates such as `augur-plugin-types`. Declare `dependencies()` only when your plugin truly cannot operate without a specific upstream producer by name.

Persistent helpers still exist for plugin-owned caches, but they are not a substitute for host-owned experiment settings.

### 5. Read host-owned settings through `GlobalSettings`

AugurRS now publishes shared runtime settings on the normal context bus:

- key: `CTX_GLOBAL_SETTINGS`
- type: `GlobalSettings`

Example:

```rust
use augur_plugin_api::{GlobalSettings, CTX_GLOBAL_SETTINGS};

if let Some(globals) = context.get::<GlobalSettings>(CTX_GLOBAL_SETTINGS)? {
    let nm_per_pixel = globals.nm_per_pixel;
    let sensor_dims = (globals.sensor_width, globals.sensor_height);
    let acq_time_ms = globals.acq_time_ms;
    let event_store_budget = globals.event_store_budget_bytes;
    let _ = (nm_per_pixel, sensor_dims, acq_time_ms, event_store_budget);
}
```

Plugins should tolerate `None` when run against an older host build.

For new plugins, prefer `GlobalSettings` over duplicating host-owned values such as pixel scale or sensor geometry in plugin-local defaults.

### 6. Define declarative settings, status, and host views

Plugins do not render `egui` directly. Instead, expose:

- `settings_schema()`
- `get_setting()`
- `set_setting()`
- optional `status_entries()`
- optional `host_views()`
- optional `host_view_dataset()`
- optional `host_view_dataset_generation()`

The host owns rendering, export, caching, and window state for declared host views.

### 7. Write `plugin.toml`

Use the runtime format:

```toml
name = "My Plugin"
version = "0.2.0"
description = "One-line summary visible in the Plugin Manager."
domain = "general"
library = "augur_plugin_my_plugin"
```

`library` is the library base name without `lib` or the platform extension.

### 8. Build and install

```bash
cargo build -p augur-plugin-my-plugin --release
mkdir -p ~/.augur/plugins/my-plugin
cp plugins/my-plugin/plugin.toml ~/.augur/plugins/my-plugin/
cp target/release/libaugur_plugin_my_plugin.dylib ~/.augur/plugins/my-plugin/
```

Then open `augur-gui`, go to **Plugins**, click **Scan for New Plugins**, and enable the plugin.

## Migrating Older Plugins

If you are porting a crate that still assumes the pre-runtime model:

1. Replace `AnalysisPlugin` with `augur_plugin_api::Plugin`.
2. Replace typed `PluginContext` exchange with `HostContext` string keys.
3. Replace direct `egui` settings UI with `SettingsSchema` and `status_entries()`.
4. Replace special-case host rendering hooks with `host_views()` plus `host_view_dataset()`.
5. Export the runtime vtable with `export_plugin!(YourPlugin)`.
6. Build the crate as `crate-type = ["cdylib", "rlib"]`.
7. Install the compiled artifact into `~/.augur/plugins/<name>/`.

Moving a source folder into `~/.augur/plugins/` is never enough.

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
- host views appear when expected
- reload works after rebuilding

## Documentation Expectations

At minimum, each plugin crate should include:

- `plugin.toml`
- `README.md`
- `Cargo.toml`
- `src/lib.rs`

If the plugin publishes shared data, document the context key and payload type explicitly.

If you add or materially change a workflow or architecture pattern in this repository, also update:

- `docs/features/<feature>.md`
- `docs/features/README.md`
- `docs/adr/` when the architecture or public interface changes

## Local Development Note

This workspace points at the sibling `../augur-rs` checkout so plugin crates can track in-flight host/API branches during coordinated development.
