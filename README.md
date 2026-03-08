<div align="center">

# augur-plugins

**Runtime-loaded analysis plugins for [AugurRS](https://github.com/muthmann/augur-rs).**

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
![Language](https://img.shields.io/badge/language-Rust-orange)

</div>

---

This repository is the home for AugurRS plugin crates, templates, and contribution docs.

Plugins are no longer compiled into `augur-gui`. A compatible plugin builds as a `cdylib`, ships a `plugin.toml`, and is loaded at runtime from `~/.augur/plugins/`.

## Runtime Model

Each installed plugin directory contains:

- `plugin.toml`
- one platform library (`.dylib`, `.so`, or `.dll`)

The GUI Plugin Manager can scan, enable, disable, and reload plugins without recompiling the host app.

## Compatibility Status

All maintained analysis plugins in this repository now target the runtime loader. The only exception is `roi-grid`, which intentionally stays built into `augur-gui` because the current runtime API does not expose camera-configuration mutation.

| Plugin | Status | Notes |
|---|---|---|
| `hotpixel` | Runtime-compatible | Migrated to `augur-plugin-api` |
| `localization` | Runtime-compatible | Migrated to `augur-plugin-api` |
| `focus-metrics` | Runtime-compatible | Consumes any upstream plugin that publishes `augur.localization.results` |
| `evesmlm-candidates` | Runtime-compatible | Publishes `augur.evesmlm.candidates` |
| `evesmlm-fitting` | Runtime-compatible | Publishes `augur.evesmlm.localization_results` and standard localization compatibility results |
| `evesmlm-postproc` | Runtime-compatible | Filters/drift-corrects EVE results and republishes standard localization compatibility results |
| `roi-grid` | Legacy / built-in | ROI Grid stays built into `augur-gui` for now |

Do not copy source trees directly into `~/.augur/plugins/`. Build the plugin first, then copy `plugin.toml` plus the generated `.dylib`, `.so`, or `.dll`.

## Quick Start

### Build a Runtime Plugin

```bash
cargo build -p augur-plugin-hotpixel --release
```

### Build All Runtime Plugins

```bash
./scripts/build-runtime-plugins.sh --profile release
```

This builds every runtime plugin crate in `plugins/` and skips non-runtime
entries such as `roi-grid`. Any arguments after `--` are forwarded to
`cargo build`.

### Install It

```bash
mkdir -p ~/.augur/plugins/hotpixel
cp plugins/hotpixel/plugin.toml ~/.augur/plugins/hotpixel/
cp target/release/libaugur_plugin_hotpixel.dylib ~/.augur/plugins/hotpixel/
```

Then open `augur-gui` and use **Plugins → Scan for New Plugins**.

### Install All Built Runtime Plugins

```bash
./scripts/install-built-plugins.sh --profile release
```

This scans `plugins/*/plugin.toml`, copies every runtime plugin that already has
its built library in `target/release/`, and skips unbuilt or non-runtime
plugins such as `roi-grid`.

### Build And Install Everything

```bash
./scripts/build-runtime-plugins.sh --profile release
./scripts/install-built-plugins.sh --profile release
```

### Build the eveSMLM Runtime Chain

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

Then follow [CONTRIBUTING.md](./CONTRIBUTING.md). The new workflow is:

1. implement `augur_plugin_api::Plugin`
2. export the vtable with `export_plugin!`
3. define settings via `SettingsSchema`
4. build a `cdylib`
5. copy the manifest plus built library into `~/.augur/plugins/<name>/`

## Repository Structure

```text
augur-plugins/
├── plugins/
├── plugin-template/
├── docs/
│   ├── installing-plugins.md
│   └── plugin-api.md
├── CONTRIBUTING.md
└── README.md
```

## Related

- [augur-rs](https://github.com/muthmann/augur-rs) — host application, runtime loader, and `augur-plugin-api`
- [Plugin Architecture](https://github.com/muthmann/augur-rs/blob/main/docs/features/analysis-plugins.md) — host-side architecture
- [Dynamic Plugin Loading](https://github.com/muthmann/augur-rs/blob/main/docs/features/dynamic-plugins.md) — install/reload model

## License

[MIT](./LICENSE)
