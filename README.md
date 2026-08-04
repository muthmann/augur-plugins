<div align="center">

<img src="assets/logo.png" alt="AugurRS" width="120" />

# augur-plugins

**Runtime-loaded analysis plugins, templates, and authoring docs for [AugurRS](https://github.com/muthmann/augur-rs).**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
![Language](https://img.shields.io/badge/language-Rust-orange)

</div>

---

This repository contains the runtime plugin crates that live alongside `augur-rs`.

The canonical host/runtime contract now lives upstream in:

- [`augur-rs/docs/features/plugin-authoring-guide.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md)
- [`augur-rs/docs/features/global-settings-menu.md`](https://github.com/muthmann/augur-rs/blob/main/docs/features/global-settings-menu.md)

Use this repository for the plugin implementations, template crate, and repo-local contributor docs.

## Runtime Model

- Each plugin ships as a `plugin.toml` manifest plus one platform library (`.dylib`, `.so`, or `.dll`).
- `augur-gui` discovers plugins from `~/.augur/plugins/`, loads the exported `augur_plugin_vtable`, and renders settings, status, and linked investigation datasets/views through the host.
- Host-owned built-in tools stay in `augur-gui`; they are not runtime plugins in this repository.
- Host-owned experiment settings such as pixel scale, sensor geometry, acquisition time, and EventStore budget are published to plugins as `GlobalSettings` on `augur.global_settings`.
- Standard shared scientific payloads can also live in companion crates such as `augur-plugin-types`.

## Investigation Workspace Contract

The host now owns a generic linked workspace across:

- 2D preview
- 3D inspection
- host-rendered tables

For plugins, that means:

- structured datasets are the primary linking mechanism
- stable row ids should be provided when possible
- 2D/3D coordinate metadata should be declared when the plugin has it
- layer/display metadata should describe visibility, color, marker shape, and size
- overlays are supplemental annotations, not the primary integration surface

## In-Tree Runtime Plugins (work in progress)

The plugin crates under `plugins/` are under active development and not yet ready for external use. The template crate and documentation are stable references for writing your own plugins.

| Plugin | Phase | Notes |
|---|---|---|
| `localization` | `RawEvents` | Wavelet/Gaussian SMLM localization and standard `LocalizationResults` output |
| `reconstruction` | `DerivedData` | Accumulated localization dataset with stable ids, time metadata, density rendering, and 3D inspection |
| `focus-metrics` | `DerivedData` | Focus metrics from localization results or FFT preview sharpness |
| `evesmlm-candidates` | `RawEvents` | Event-domain candidate clustering plus accepted/rejected raw-event investigation layers |
| `evesmlm-fitting` | `DerivedData` | Candidate fitting plus shared current-localization datasets, stable ids, and linked 3D inspection |
| `evesmlm-postproc` | `DerivedData` | Filtering, drift correction, evaluation, and the later shared EVE current-localization provider |
| `stage-a-modulation` | control service | Sole owner of the Stage-A Teensy command port and ACKed modulation state |
| `stage-a-photodiode` | control service | Sole owner of the Stage-A stream port, PDA1 ingestion, and PDQ persistence |
| `stage-a-a1` | `RawEvents` + orchestration | A1 protocol/schedule validation, raw phase quicklooks, analysis core, and a safety-gated commissioning run through the two owner services |

`plugin-template/` is the starting point for new plugin crates.

## Quick Start

### Download Prebuilt Plugins (no toolchain needed)

Every push to `main` publishes freshly built plugins for macOS (arm64 and x86_64),
Linux and Windows to the rolling
[`plugins-latest`](https://github.com/muthmann/augur-plugins/releases/tag/plugins-latest)
release. This is the recommended route for a measurement machine.

```bash
curl -LO https://github.com/muthmann/augur-plugins/releases/download/plugins-latest/augur-plugins-macos-arm64.zip
unzip augur-plugins-macos-arm64.zip -d augur-plugins-bundle
mkdir -p ~/.augur/plugins
cp -R augur-plugins-bundle/*/ ~/.augur/plugins/
```

Pick the archive matching the machine: `macos-arm64`, `macos-x86_64`,
`linux-x86_64`, or `windows-x86_64`. Then open `augur-gui`, go to **Plugins**, and
click **Scan for New Plugins**.

Each archive contains a `BUILD-INFO.txt` recording the `augur-rs` revision and the
`rustc` version the libraries were built against — quote it in any ABI-mismatch
report. Verify downloads against `SHA256SUMS.txt` from the same release.

Pull requests build the same bundles as workflow artifacts. See
[CI Prebuilt Plugin Bundles](./docs/features/ci-prebuilt-plugin-bundles.md).

### Build One Plugin

```bash
cargo build -p augur-plugin-localization --release
```

### Install One Plugin

```bash
mkdir -p ~/.augur/plugins/localization
cp plugins/localization/plugin.toml ~/.augur/plugins/localization/
cp target/release/libaugur_plugin_localization.dylib ~/.augur/plugins/localization/
```

On Linux, copy the `.so`. On Windows, copy the `.dll`.
On macOS, prefer `./scripts/install-built-plugins.sh --profile release`; it rewrites the copied
plugin dylib id so Plugin Manager reloads do not keep pointing at Cargo's build tree.

Then open `augur-gui`, go to **Plugins**, click **Scan for New Plugins**, and enable the plugin.

### Build All Runtime Plugins

```bash
./scripts/build-runtime-plugins.sh --profile release
```

### Install All Built Runtime Plugins

```bash
./scripts/install-built-plugins.sh --profile release
```

### Build the eveSMLM Chain

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build \
  -p augur-plugin-evesmlm-candidates \
  -p augur-plugin-evesmlm-fitting \
  -p augur-plugin-evesmlm-postproc \
  --release
```

## Writing a Plugin

Start from the template:

```bash
cp -r plugin-template plugins/my-plugin
```

The current authoring flow is:

1. implement `augur_plugin_api::Plugin`
2. export the vtable with `export_plugin!`
3. choose `input_kind()` and optional `PluginCapabilities`
4. use `HostContext` for shared payloads, companion crates such as `augur-plugin-types` for reusable payload types, and `CTX_GLOBAL_SETTINGS` for host-owned calibration/settings
5. declare host-rendered outputs with `host_views()` when needed and populate stable-id / coordinate / layer metadata when the dataset should participate in linked investigation
   - to expose interactive operations, append `HostActionDescriptor`s to `HostViewRegistry.actions` (scope `Dataset`/`Row`/`Cluster`, optional `param_schema`); consume requests from the persistent context key `CTX_INVESTIGATION_ACTION_REQUESTS`
6. build a `cdylib`
7. install `plugin.toml` plus the compiled library into `~/.augur/plugins/<name>/`

For the full contract, read the upstream authoring guide. For repo-local workflow and conventions, see [CONTRIBUTING.md](./CONTRIBUTING.md), [docs/plugin-api.md](./docs/plugin-api.md), and [docs/installing-plugins.md](./docs/installing-plugins.md).

## Repository Structure

```text
augur-plugins/
├── plugins/
├── plugin-template/
├── docs/
├── scripts/
├── CONTRIBUTING.md
└── README.md
```

## Related

- [Plugin API Notes](./docs/plugin-api.md) — repo-local summary of the current runtime contract
- [Installing Plugins](./docs/installing-plugins.md) — build, copy, reload, and troubleshoot installed plugins
- [CI Prebuilt Plugin Bundles](./docs/features/ci-prebuilt-plugin-bundles.md) — how the downloadable per-platform bundles are built and published
- [Architecture Notes](./docs/architecture.md) — repository role, execution model, host views, and shared settings
- [augur-rs Plugin Authoring Guide](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md) — canonical host/runtime authoring guide
- [augur-rs Global Settings Guide](https://github.com/muthmann/augur-rs/blob/main/docs/features/global-settings-menu.md) — host-owned settings published to plugins

## License

[MIT](./LICENSE)
