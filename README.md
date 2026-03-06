<div align="center">

# augur-plugins

**The plugin marketplace for [AugurRS](https://github.com/muthmann/augur-rs) — community-maintained live analysis plugins for event cameras.**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
![Language](https://img.shields.io/badge/language-Rust-orange)

</div>

---

This repository is the central home for AugurRS analysis plugins. It serves as both a development workspace and a browsable catalog for community extensions.

AugurRS itself is a general-purpose event camera recorder and live preview tool. Plugins extend it with domain-specific live analysis: signal processing, detection, localization, metrics, custom overlays, or anything else that operates on the preview stream.

## How It Works

Plugins are Rust crates that implement the `AnalysisPlugin` trait defined in [augur-rs](https://github.com/muthmann/augur-rs). They are compiled into the `augur-gui` binary — adding or removing a plugin is a one-line Cargo dependency change plus a one-line registration call.

Each plugin runs live alongside the preview stream:

```
Preview frame → [Phase 1: FrameOnly] → [Phase 2: RawEvents] → [Phase 3: DerivedData]
                     │                       │                        │
               Hotpixel Detection      Localization            Focus Metrics
               ROI Grid                                        (reads localization results)
                                        EVE Candidate Finding   EVE Candidate Fitting
                                                                 EVE Post-Processing
```

Plugins share typed data through a per-frame **context bus** (`PluginContext`). An upstream plugin publishes results; a downstream plugin consumes them. The phase ordering guarantees that dependencies are satisfied within a single frame.

## Available Plugins

| Plugin | Phase | Domain | Description |
|---|---|---|---|
| **[Hotpixel Detection](plugins/hotpixel/)** | FrameOnly | General | Detects persistently noisy pixels and pushes them into the hardware DEM mask |
| **[ROI Grid](plugins/roi-grid/)** | FrameOnly | General | Partitions the sensor around masked hotpixels; finds the largest clean capture regions |
| **[Molecule Localization](plugins/localization/)** | RawEvents | SMLM | Wavelet denoising, center-of-mass seeding, sub-pixel elliptical Gaussian fitting |
| **[EVE Candidate Finding](plugins/evesmlm-candidates/)** | RawEvents | SMLM | Raw-event clustering for eveSMLM using DBSCAN, eigenfeatures, or a frame-based fallback |
| **[EVE Candidate Fitting](plugins/evesmlm-fitting/)** | DerivedData | SMLM | Multi-backend sub-pixel fitting of EVE candidate clusters with `LocalizationResults` compatibility |
| **[EVE Post-Processing](plugins/evesmlm-postproc/)** | DerivedData | SMLM | Filtering, drift correction, eNeNA precision tracking, PSF accumulation, and on-time summaries |
| **[Focus Metrics](plugins/focus-metrics/)** | DerivedData | Biophotonics | Mean PSF sigma, FFT sharpness, astigmatic ratio — live focus feedback |

The first two plugins are useful for any event camera workflow. The latter two are purpose-built for single-molecule localization microscopy (SMLM) and biophotonics, but they also serve as full-featured reference implementations for the plugin API.

## Quick Start

### Using Plugins

To include a plugin in your AugurRS build, add it as a dependency of `augur-gui` and register it in the plugin host. See the [Installation Guide](docs/installing-plugins.md) for step-by-step instructions.

### Writing a Plugin

The fastest way to start is to copy the [plugin template](plugin-template/):

```bash
cp -r plugin-template plugins/my-plugin
```

Then follow the [Plugin Development Guide](CONTRIBUTING.md#writing-a-plugin) to implement the `AnalysisPlugin` trait, choose an execution phase, and wire up the settings UI.

A minimal plugin looks like this:

```rust
use augur_core::{analysis::AnalysisOutput, config::CameraConfig, pipeline::PreviewFrame};

pub struct MyPlugin {
    enabled: bool,
}

impl AnalysisPlugin for MyPlugin {
    fn name(&self) -> &str { "My Plugin" }
    fn enabled(&self) -> bool { self.enabled }
    fn set_enabled(&mut self, enabled: bool) { self.enabled = enabled; }

    fn ui_settings(&mut self, ui: &mut egui::Ui, _config: &mut CameraConfig) -> bool {
        ui.label("Hello from my plugin!");
        false
    }

    fn process_frame(&mut self, frame: &PreviewFrame, output: &mut AnalysisOutput) {
        // Your analysis logic here
    }

    fn reset(&mut self) { /* reset state between sessions */ }
}
```

## Repository Structure

```
augur-plugins/
├── plugins/
│   ├── hotpixel/            # Each plugin is its own Cargo crate
│   │   ├── Cargo.toml
│   │   ├── plugin.toml      # Plugin manifest (metadata, phase, dependencies)
│   │   ├── README.md
│   │   └── src/lib.rs
│   ├── roi-grid/
│   ├── localization/
│   ├── evesmlm-candidates/
│   ├── evesmlm-fitting/
│   ├── evesmlm-postproc/
│   └── focus-metrics/
├── plugin-template/         # Copy this to start a new plugin
├── docs/
│   ├── features/            # Feature briefs for larger plugin suites
│   ├── adr/                 # Architecture decision records
│   ├── plugin-api.md        # API reference
│   ├── installing-plugins.md
│   └── architecture.md      # Design overview and comparisons
├── CONTRIBUTING.md           # How to write and submit plugins
└── README.md                 # This file
```

## Plugin Manifest

Every plugin includes a `plugin.toml` manifest that describes it for the catalog:

```toml
[plugin]
name = "My Plugin"
version = "0.1.0"
description = "One-line summary of what this plugin does."
authors = ["Your Name <you@example.com>"]
license = "MIT"
phase = "FrameOnly"            # FrameOnly | RawEvents | DerivedData
domain = "general"             # general | smlm | biophotonics | robotics | ...
dependencies = []              # Names of upstream plugins this one requires

[plugin.augur]
min-version = "0.1.0"         # Minimum augur-rs version this plugin supports
```

The manifest is not read at compile time — it exists for documentation, tooling, and future registry automation.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full guide, including:

- How to write a plugin from scratch
- How to submit a plugin to this repository
- Code style and documentation expectations
- The review process for new plugins

Plugin contributions are welcome from anyone. If you have a live analysis workflow for event cameras — whether in microscopy, robotics, computer vision, or any other domain — this is the place to share it.

## Related

- [augur-rs](https://github.com/muthmann/augur-rs) — the core camera SDK, streaming pipeline, and plugin runtime
- [Plugin Architecture](https://github.com/muthmann/augur-rs/blob/main/docs/features/analysis-plugins.md) — API reference in the core repository
- [eveSMLM Feature Brief](docs/features/evesmlm.md) — overview of the three-plugin EVE pipeline

## License

[MIT](./LICENSE)
