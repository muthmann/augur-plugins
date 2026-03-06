# Installing Plugins

AugurRS uses compile-time plugin registration. Plugins are Rust crates that get compiled into the `augur-gui` binary. This guide walks through adding a plugin from this repository to your AugurRS build.

## Prerequisites

- A working [augur-rs](https://github.com/muthmann/augur-rs) checkout that builds successfully
- This repository cloned alongside it (or accessible via git URL)

Typical directory layout:

```
your-workspace/
├── augur-rs/           # The core camera SDK and GUI
└── augur-plugins/      # This repository
```

## Step 1: Add the Plugin Dependency

Open `augur-rs/augur-gui/Cargo.toml` and add the plugin crate as a dependency.

**From a local checkout:**

```toml
[dependencies]
augur-plugin-hotpixel = { path = "../../augur-plugins/plugins/hotpixel" }
```

**From the git repository:**

```toml
[dependencies]
augur-plugin-hotpixel = { git = "https://github.com/muthmann/augur-plugins.git" }
```

## Step 2: Register the Plugin

Open `augur-rs/augur-gui/src/plugins/mod.rs` and add the plugin to the `create_all_plugins()` function:

```rust
pub fn create_all_plugins() -> Vec<Box<dyn AnalysisPlugin>> {
    vec![
        Box::new(augur_plugin_hotpixel::HotpixelPlugin::default()),
        // ... other plugins ...
    ]
}
```

## Step 3: Build

```bash
cd augur-rs
cargo build --workspace
```

The plugin is now compiled in and will appear in the Analysis panel when you launch `augur-gui`.

## Removing a Plugin

Reverse the process: remove the registration line from `mod.rs` and the dependency from `Cargo.toml`. The application returns to a plain recording tool.

## Using All Plugins

To include the full plugin suite, add all four plugin crates as dependencies. See the individual plugin READMEs for any crate-specific dependency notes (e.g., `rustfft` for Focus Metrics).

## Troubleshooting

**Version mismatch:** If the plugin was built against a different version of `augur-core` than your checkout, Cargo will report type conflicts. Ensure both repositories are on compatible versions.

**Missing `AnalysisPlugin` trait:** The trait is defined in `augur-gui/src/plugin.rs`. Your plugin crate does not directly depend on `augur-gui` — the trait binding happens when `augur-gui` compiles with your plugin as a dependency and writes the `impl` block in its own `plugins/` module.
