# Localization Reconstruction

Runtime plugin. Build `augur-plugin-reconstruction` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/reconstruction/`.

Accumulates standard `LocalizationResults` across frames so a compatible `augur-gui` host can render a super-resolution reconstruction window and export ThunderSTORM-style CSV tables plus PNG or TIFF images.

## Execution Phase

`DerivedData` — runs after any upstream plugin that publishes `augur.localization.results`.

## Configuration

| Setting | Default | Description |
|---|---|---|
| Scale | `65.0` nm/px | Pixel size used to convert localization coordinates into exported nanometer units |
| Max localizations | `1_000_000` | Safety cap for the accumulated localization table |

## Published Data

None through `HostContext`. Instead, the plugin exposes its accumulated table through the runtime plugin API so the host can render and export it without re-publishing a full-frame copy each update.

## Compatibility

Requires a matching `augur-gui` / `augur-plugin-api` build that supports the `accumulated_localizations()` runtime hook.
