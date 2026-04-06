# Localization Reconstruction

Runtime plugin. Build `augur-plugin-reconstruction` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/reconstruction/`.

Accumulates standard `LocalizationResults` across frames so a compatible `augur-gui` host can render generic host-view windows from one shared accumulated dataset.

## Execution Phase

`DerivedData` — runs after any upstream plugin that publishes `augur.localization.results`.

## Configuration

| Setting | Default | Description |
|---|---|---|
| Max localizations | `1_000_000` | Safety cap for the accumulated localization table |

AugurRS now publishes host-owned calibration on `CTX_GLOBAL_SETTINGS` as `GlobalSettings`. This plugin uses the host `nm_per_pixel` value automatically when converting accumulated localizations into nanometer space, while retaining a hidden fallback for older hosts.

## Host Views

The plugin publishes one dataset, `augur.localization.accumulated`, and two host-rendered window views over that dataset:

- `Localization Table`
- `Reconstruction`

Both views read the same accumulated source of truth.

## Compatibility

Requires a matching `augur-gui` / `augur-plugin-api` build that supports the current runtime plugin ABI plus the generic host-view registry.
