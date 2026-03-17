# Localization Reconstruction

Runtime plugin. Build `augur-plugin-reconstruction` as a `cdylib`, then copy `plugin.toml` plus the generated library into `~/.augur/plugins/reconstruction/`.

Accumulates standard `LocalizationResults` across frames so a compatible `augur-gui` host can render generic host-view windows from one shared accumulated dataset.

## Execution Phase

`DerivedData` — runs after any upstream plugin that publishes `augur.localization.results`.

## Configuration

| Setting | Default | Description |
|---|---|---|
| Scale | `65.0` nm/px | Pixel size used to convert localization coordinates into exported nanometer units |
| Max localizations | `1_000_000` | Safety cap for the accumulated localization table |

## Host Views

The plugin publishes one dataset, `augur.localization.accumulated`, and two host-rendered window views over that dataset:

- `Localization Table`
- `Reconstruction`

Both views read the same accumulated source of truth. `accumulated_localizations()` remains available only as a compatibility hook for older hosts during the transition cycle.

## Compatibility

Requires a matching `augur-gui` / `augur-plugin-api` build that supports the generic host-view registry.
