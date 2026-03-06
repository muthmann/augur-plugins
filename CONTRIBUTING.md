# Contributing to augur-plugins

Contributions are welcome — whether you are adding a new plugin, improving an existing one, or fixing a bug.

This guide covers everything you need to write, test, and submit a plugin.

## Writing a Plugin

### 1. Start From the Template

Copy the [plugin template](plugin-template/) into the `plugins/` directory:

```bash
cp -r plugin-template plugins/my-plugin
```

Rename the crate in `plugins/my-plugin/Cargo.toml`, update `plugin.toml` with your metadata, and add the crate to the workspace `members` list in the root `Cargo.toml`.

### 2. Implement the `AnalysisPlugin` Trait

Every plugin implements a single trait defined in `augur-gui`. The trait covers the full lifecycle:

```rust
pub trait AnalysisPlugin {
    // --- Identity ---
    fn name(&self) -> &str;
    fn description(&self) -> &str { "" }

    // --- Enable / disable ---
    fn enabled(&self) -> bool;
    fn set_enabled(&mut self, enabled: bool);

    // --- Settings UI ---
    // Rendered in the Analysis panel. Return true if CameraConfig was mutated.
    fn ui_settings(&mut self, ui: &mut egui::Ui, config: &mut CameraConfig) -> bool;

    // --- Per-frame processing ---
    fn process_frame(&mut self, frame: &PreviewFrame, output: &mut AnalysisOutput);

    // Context-aware variant (override this when you publish or consume shared data):
    fn process_frame_with_context(
        &mut self,
        frame: &PreviewFrame,
        output: &mut AnalysisOutput,
        ctx: &mut PluginContext,
    ) {
        self.process_frame(frame, output);
    }

    // --- Execution metadata ---
    fn input_kind(&self) -> PluginInput { PluginInput::FrameOnly }
    fn dependencies(&self) -> &[&str] { &[] }

    // --- Lifecycle ---
    fn reset(&mut self);
}
```

### 3. Choose an Execution Phase

Plugins run in three ordered passes per preview frame:

| Phase | `PluginInput` | When it runs | Use when you need |
|---|---|---|---|
| Phase 1 | `FrameOnly` | First | Only the decoded preview frame (pixel counts) |
| Phase 2 | `RawEvents` | Second | The raw `CdEvent` stream for the current window |
| Phase 3 | `DerivedData` | Third | Results published by upstream plugins |

Choose the earliest phase that satisfies your requirements. `FrameOnly` plugins are the cheapest — the pipeline skips raw event materialization entirely when no plugin declares `RawEvents`.

### 4. Share Data Through the Context Bus

If your plugin produces results that downstream plugins should consume, publish them into `PluginContext`:

```rust
// In your process_frame_with_context:
let results = MyResults { /* ... */ };
ctx.publish(results);
```

Downstream plugins retrieve your results by type:

```rust
if let Some(upstream) = ctx.get::<MyResults>() {
    // Use upstream results
}
```

When consuming upstream data, declare the dependency so the plugin host can validate the execution order:

```rust
fn dependencies(&self) -> &[&str] {
    &["Upstream Plugin Name"]
}
```

### 5. Add a Settings UI

The `ui_settings` method receives an `egui::Ui` reference. Use standard egui widgets — sliders, checkboxes, labels, collapsing headers. Follow the pattern used by existing plugins:

```rust
fn ui_settings(&mut self, ui: &mut egui::Ui, _config: &mut CameraConfig) -> bool {
    let mut changed = false;

    egui::CollapsingHeader::new(self.name())
        .default_open(true)
        .show(ui, |ui| {
            ui.weak(self.description());
            changed |= ui
                .add(egui::Slider::new(&mut self.threshold, 0.1..=10.0).text("Threshold"))
                .changed();
        });

    // Return true only if you mutated CameraConfig (e.g. changed the ROI).
    // Most plugins return false here.
    false
}
```

### 6. Write the Plugin Manifest

Create a `plugin.toml` in your plugin directory:

```toml
[plugin]
name = "My Plugin"
version = "0.1.0"
description = "One-line summary visible in the catalog."
authors = ["Your Name <you@example.com>"]
license = "MIT"
phase = "FrameOnly"
domain = "general"
dependencies = []

[plugin.augur]
min-version = "0.1.0"
```

Valid `phase` values: `FrameOnly`, `RawEvents`, `DerivedData`.
Valid `domain` values: `general`, `smlm`, `biophotonics`, `robotics`, `computer-vision`, or propose a new one in your PR.

### 7. Write a README

Each plugin directory should have a `README.md` that explains:

- What the plugin does (one paragraph)
- How to enable and configure it
- What data it publishes (if any)
- What dependencies it requires (if any)
- Any relevant references (papers, algorithms)

### 8. Register the Plugin

To compile your plugin into `augur-gui`, two changes are needed in the [augur-rs](https://github.com/muthmann/augur-rs) repository:

**`augur-gui/Cargo.toml`** — add the dependency:

```toml
[dependencies]
my-plugin = { path = "../../augur-plugins/plugins/my-plugin" }
# or for published plugins:
# my-plugin = { git = "https://github.com/muthmann/augur-plugins.git" }
```

**`augur-gui/src/plugins/mod.rs`** — add the registration:

```rust
pub fn create_all_plugins() -> Vec<Box<dyn AnalysisPlugin>> {
    vec![
        // ... existing plugins ...
        Box::new(my_plugin::MyPlugin::default()),
    ]
}
```

That is it. One dependency line, one registration line.

## Testing

### Build Check

```bash
# From the augur-plugins root:
cargo build --workspace

# Clippy and formatting:
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

### Unit Tests

Add tests in your plugin crate:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn my_analysis_produces_expected_output() {
        // Create a synthetic PreviewFrame, run your analysis, assert results.
    }
}
```

Run with:

```bash
cargo test --workspace
```

### Integration Testing

For end-to-end testing, build `augur-gui` with your plugin registered and verify it works with a live camera or a recorded `.raw` file replay.

## Submitting a Plugin

### New Plugin

1. Fork this repository
2. Create your plugin in `plugins/your-plugin-name/`
3. Include: `Cargo.toml`, `plugin.toml`, `README.md`, `src/lib.rs`
4. Add your crate to the workspace `members` in the root `Cargo.toml`
5. Ensure `cargo build --workspace` and `cargo test --workspace` pass
6. Open a pull request

### Bug Fix or Improvement

1. Open an issue describing the problem (optional but appreciated for larger changes)
2. Submit a pull request with the fix
3. Include what changed and how you verified it

### Pull Request Checklist

- [ ] Plugin builds without warnings (`cargo clippy`)
- [ ] Code is formatted (`cargo fmt`)
- [ ] Tests pass (`cargo test`)
- [ ] `plugin.toml` is complete and accurate
- [ ] `README.md` documents the plugin
- [ ] Workspace `Cargo.toml` includes the new crate (if adding a plugin)

## Code Style

- Follow standard Rust conventions (`cargo fmt`, `cargo clippy`)
- Use clear, descriptive names — avoid abbreviations except for well-known terms (FFT, PSF, ROI, etc.)
- Keep plugin files focused — one plugin per crate
- Prefer explicit over clever; another researcher should be able to read your analysis logic

## Documentation Expectations

Good plugin documentation helps other researchers understand, use, and extend your work. At minimum:

- `plugin.toml` with accurate metadata
- `README.md` with a description, configuration reference, and any relevant citations
- Inline doc comments on public types and methods

If your plugin implements a published algorithm, cite the paper and note any differences from the reference implementation.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Be respectful, be constructive, assume good intent.

## Questions

Open an issue or start a discussion on the [augur-rs](https://github.com/muthmann/augur-rs) repository. Plugin API questions, design suggestions, and feature requests are all welcome.
