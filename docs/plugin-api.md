# Plugin API Reference

This document describes the public API that plugins interact with. The API is defined in the [augur-rs](https://github.com/muthmann/augur-rs) repository across two crates:

- **`augur-gui`** — owns the `AnalysisPlugin` trait, `PluginContext`, and `PluginInput`
- **`augur-core`** — owns the data types that plugins consume and produce

## `AnalysisPlugin` Trait

Every plugin implements this trait. The trait is defined in `augur-gui/src/plugin.rs`.

### Required Methods

| Method | Signature | Purpose |
|---|---|---|
| `name` | `fn name(&self) -> &str` | Display name in the Analysis panel |
| `enabled` | `fn enabled(&self) -> bool` | Whether the plugin is currently active |
| `set_enabled` | `fn set_enabled(&mut self, enabled: bool)` | Toggle the plugin on or off |
| `ui_settings` | `fn ui_settings(&mut self, ui: &mut egui::Ui, config: &mut CameraConfig) -> bool` | Draw settings widgets; return `true` if `CameraConfig` was mutated |
| `process_frame` | `fn process_frame(&mut self, frame: &PreviewFrame, output: &mut AnalysisOutput)` | Process a preview frame (simple path) |
| `reset` | `fn reset(&mut self)` | Clear all internal state |

### Optional Methods (with defaults)

| Method | Default | Purpose |
|---|---|---|
| `description` | `""` | Short text shown below the plugin name |
| `process_frame_with_context` | Delegates to `process_frame` | Context-aware processing; override when publishing or consuming shared data |
| `input_kind` | `PluginInput::FrameOnly` | Declares which execution phase the plugin belongs to |
| `dependencies` | `&[]` | Names of upstream plugins this one requires |

## `PluginInput` Enum

```rust
pub enum PluginInput {
    FrameOnly,    // Phase 1 — decoded preview frame only
    RawEvents,    // Phase 2 — raw CdEvent stream available
    DerivedData,  // Phase 3 — consumes results from Phase 1 or 2 plugins
}
```

The plugin host uses `input_kind()` to sort plugins into execution phases. Phases run in order: all `FrameOnly` plugins first, then `RawEvents`, then `DerivedData`.

Raw event transport is demand-driven: the pipeline only materializes `Vec<CdEvent>` when at least one enabled plugin declares `RawEvents`. This keeps the default preview path lightweight.

## `PluginContext` — Typed Data Bus

`PluginContext` is a per-frame key-value store indexed by Rust's `TypeId`. It enables zero-cost typed communication between plugins within a single frame.

| Method | Signature | Purpose |
|---|---|---|
| `publish` | `fn publish<T: Any + 'static>(&mut self, value: T)` | Store a value under its type |
| `get` | `fn get<T: Any + 'static>(&self) -> Option<&T>` | Retrieve a value by type |
| `clear` | `fn clear(&mut self)` | Clear all stored data (called between frames) |
| `raw_events` | `pub raw_events: Option<Vec<CdEvent>>` | Raw event stream for the current window |

### Publishing Pattern

```rust
fn process_frame_with_context(
    &mut self,
    frame: &PreviewFrame,
    output: &mut AnalysisOutput,
    ctx: &mut PluginContext,
) {
    let results = self.run_analysis(frame);
    ctx.publish(results);  // Downstream plugins can now access this
}
```

### Consuming Pattern

```rust
fn process_frame_with_context(
    &mut self,
    frame: &PreviewFrame,
    output: &mut AnalysisOutput,
    ctx: &mut PluginContext,
) {
    let Some(upstream) = ctx.get::<UpstreamResults>() else {
        // Upstream plugin not enabled or no data this frame
        return;
    };
    // Use upstream results
}
```

## Core Data Types

These types are defined in `augur-core` and used throughout the plugin API.

### `PreviewFrame`

```rust
pub struct PreviewFrame {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u16>,              // Decoded preview (event counts per pixel)
    pub events: Option<Vec<CdEvent>>,  // Raw events (only when requested)
    pub window_start_us: u64,          // Timestamp of window start
    pub window_end_us: u64,            // Timestamp of window end
}
```

### `CdEvent`

```rust
pub struct CdEvent {
    pub x: u16,
    pub y: u16,
    pub polarity: bool,
    pub timestamp: u64,
}
```

### `AnalysisOutput`

```rust
pub struct AnalysisOutput {
    pub overlays: Vec<Overlay>,
    pub warnings: Vec<AnalysisWarning>,
}
```

Plugins push visual overlays and status warnings into `AnalysisOutput`. The GUI renders overlays on the preview canvas and displays warnings in the status area.

### `CameraConfig`

The full camera configuration (biases, ROI, pixel mask, filters). Plugins receive a mutable reference in `ui_settings` — this allows plugins like ROI Grid to apply their results directly to the camera configuration.

## Shared Plugin Types

Domain-specific types shared between plugins live in `augur-gui/src/plugins/types.rs`. Currently:

- `Localization` — a single molecule localization result (x, y, sigma_x, sigma_y, amplitude, background, timestamp, fit error)
- `LocalizationResults` — a collection of localizations for a single frame window

These types are published by the Molecule Localization plugin and consumed by Focus Metrics. If you write a plugin that produces a new shared data type, define it in your plugin crate and document it in your README.
