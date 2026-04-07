# Reconstruction Workflow

The reconstruction workflow publishes one accumulated host-view dataset instead of a reconstruction-specific UI hook. That keeps the accumulation logic in the plugin while letting the host render multiple windows from the same source of truth.

## Components

1. **Localization Reconstruction** (`DerivedData`) reads `LocalizationResults` from `HostContext` and stores a capped nanometer-space accumulation table.
2. **`host_views()`** declares one dataset, `augur.localization.accumulated`, plus two host-rendered window views:
   - `Localization Table`
   - `Reconstruction`
3. **`host_view_dataset()`** serves one columnar `TableV1` snapshot that both windows consume.

## Source Of Truth

- the reconstruction plugin owns the only accumulated localization state
- the full table window and density reconstruction window read the same dataset id

## Resource Use

- the plugin keeps only a capped FIFO of localization rows in memory
- cap enforcement avoids shifting the full accumulation buffer on overflow

## Calibration Note

AugurRS now publishes host-owned calibration on `CTX_GLOBAL_SETTINGS` as `GlobalSettings`.

`Localization Reconstruction` now uses that host `nm_per_pixel` value automatically when it is available, while retaining a hidden fallback for older hosts that do not publish `GlobalSettings` yet.

## Data Flow

`LocalizationResults` -> `augur.localization.accumulated` -> host table window / host density window

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

Then run a matching `augur-gui` / `augur-plugin-api` build that includes the generic host-view registry support.
