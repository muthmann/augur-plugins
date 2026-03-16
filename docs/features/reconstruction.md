# Reconstruction Workflow

The reconstruction workflow adds a dedicated runtime plugin that accumulates standard localization results across frames so the host application can render a separate super-resolution view and export the results for downstream tools.

## Components

1. **Localization Reconstruction** (`DerivedData`) reads `LocalizationResults` from the typed plugin context and stores a capped table of nanometer-space rows.
2. **`augur-plugin-api` accumulation hook** lets runtime plugins expose that table without re-publishing it into the per-frame context map.
3. **`augur-gui` reconstruction window** renders the accumulated table into a histogram image and exports ThunderSTORM-style CSV plus PNG/TIFF snapshots.

## Why The Split Exists

- The plugin owns scientific accumulation logic and stays reusable across hosts.
- The host owns rendering, file dialogs, image encoding, and viewport state.
- The per-frame `HostContext` stays lightweight instead of re-serializing the full reconstruction table every frame.

## Data Flow

`LocalizationResults` -> `Localization Reconstruction` table -> host reconstruction image / CSV / image export

## Installation

Build the plugin:

```bash
cargo build -p augur-plugin-reconstruction --release
```

Install it into the runtime plugin directory:

```bash
mkdir -p ~/.augur/plugins/reconstruction
cp plugins/reconstruction/plugin.toml ~/.augur/plugins/reconstruction/
cp target/release/libaugur_plugin_reconstruction.dylib ~/.augur/plugins/reconstruction/
```

Then run a matching `augur-gui` build that includes the reconstruction window support.
