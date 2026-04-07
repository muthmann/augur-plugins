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
- `augur-gui` discovers plugins from `~/.augur/plugins/`, loads the exported `augur_plugin_vtable`, and renders settings, status, and host views through the host.
- Host-owned built-in tools stay in `augur-gui`; they are not runtime plugins in this repository.
- Host-owned experiment settings such as pixel scale, sensor geometry, acquisition time, and EventStore budget are published to plugins as `GlobalSettings` on `augur.global_settings`.
- Standard shared scientific payloads can also live in companion crates such as `augur-plugin-types`.

## In-Tree Runtime Plugins (work in progress)

The plugin crates under `plugins/` are under active development and not yet ready for external use. The template crate and documentation are stable references for writing your own plugins.

| Plugin | Phase | Notes |
|---|---|---|
| `localization` | `RawEvents` | Wavelet/Gaussian SMLM localization and standard `LocalizationResults` output |
| `reconstruction` | `DerivedData` | Accumulated localization table plus host-rendered reconstruction windows |
| `focus-metrics` | `DerivedData` | Focus metrics from localization results or FFT preview sharpness |
| `evesmlm-candidates` | `RawEvents` | Event-domain candidate clustering for eveSMLM |
| `evesmlm-fitting` | `DerivedData` | Candidate fitting plus EVE and compatibility localization outputs |
| `evesmlm-postproc` | `DerivedData` | Filtering, drift correction, evaluation, and the later EVE compact view provider |

`plugin-template/` is the starting point for new plugin crates.

## Quick Start

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
5. declare host-rendered outputs with `host_views()` when needed
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
- [Architecture Notes](./docs/architecture.md) — repository role, execution model, host views, and shared settings
- [augur-rs Plugin Authoring Guide](https://github.com/muthmann/augur-rs/blob/main/docs/features/plugin-authoring-guide.md) — canonical host/runtime authoring guide
- [augur-rs Global Settings Guide](https://github.com/muthmann/augur-rs/blob/main/docs/features/global-settings-menu.md) — host-owned settings published to plugins

## License

[MIT](./LICENSE)
