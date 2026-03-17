# Reconstruction Workflow

The reconstruction workflow adds a dedicated runtime plugin that accumulates standard localization results across frames so the host application can render a separate super-resolution view and export the results for downstream tools.

## Components

1. **Localization Reconstruction** (`DerivedData`) reads `LocalizationResults` from the context bus and stores a capped table of nanometer-space rows.
2. **Host-view registry** exposes the accumulated table through generic `host_views()` and `host_view_dataset()` methods, so the host renders it without reconstruction-specific GUI code.
3. **`augur-gui` host views** renders a table window (with CSV export) and a 2D density heatmap from the plugin's declared descriptors.

## Why The Split Exists

- The plugin owns scientific accumulation logic and stays reusable across hosts.
- The host owns rendering, file dialogs, image encoding, and viewport state.
- Dataset payloads are fetched lazily on demand, so the plugin does not re-serialize the full table every frame.

## Host View Descriptors

The reconstruction plugin declares:

- **Dataset** `augur.localization.accumulated` — a `TableV1` with 9 columns (id, frame, x_nm, y_nm, sigma_nm, intensity, offset, uncertainty_xy_nm, timestamp_us) and optional 2D coordinate space bounds
- **View** `augur.localization.accumulated.table` — `TableWindow` placement for full table inspection and CSV export
- **View** `augur.localization.accumulated.density` — `Density2dFromTable` placement for 2D super-resolution density rendering

## Data Flow

`LocalizationResults` -> `Localization Reconstruction` table -> host table window / density map / CSV / image export

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

Then run a matching `augur-gui` build that includes host-view registry support.
