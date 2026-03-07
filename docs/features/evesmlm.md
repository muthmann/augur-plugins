# eveSMLM Pipeline

The eveSMLM pipeline is implemented as three focused plugins so each stage can be enabled, configured, and debugged independently.

## Stages

1. **EVE Candidate Finding** (`RawEvents`) clusters raw `CdEvent` samples into emitter candidates and publishes `EveCandidates`.
2. **EVE Candidate Fitting** (`DerivedData`) converts each candidate into one or more sub-pixel localization estimates and republishes both `EveLocalizationResults` and `LocalizationResults`.
3. **EVE Post-Processing** (`DerivedData`) filters, drift-corrects, and evaluates the fitted localizations.

## Why Three Plugins

- Keeps raw-event grouping separate from numerical fitting, so candidate quality can be inspected directly.
- Lets researchers compare fitting methods on a fixed candidate set.
- Allows post-processing to be toggled or replaced without touching candidate generation.
- Preserves compatibility with existing downstream plugins through `LocalizationResults`.

## Data Flow

`CdEvent` stream -> `EveCandidates` -> `EveLocalizationResults` -> filtered / corrected `EveLocalizationResults`

## Installation

Build all three plugins and install them into `~/.augur/plugins/`:

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build \
  -p augur-plugin-evesmlm-candidates \
  -p augur-plugin-evesmlm-fitting \
  -p augur-plugin-evesmlm-postproc \
  --release

for name in evesmlm-candidates evesmlm-fitting evesmlm-postproc; do
  mkdir -p ~/.augur/plugins/$name
  cp plugins/$name/plugin.toml ~/.augur/plugins/$name/
  cp target/release/libaugur_plugin_${name//-/_}.dylib ~/.augur/plugins/$name/
done
```

Then open `augur-gui`, go to **Plugins → Scan for New Plugins**, and enable all three. The runtime loader discovers ordering from phase declarations (`RawEvents` then `DerivedData`), so no manual registration order is required.

## References

- Weber, L.M., Martens, K.J.A., Cabriel, C., Gates, J.J., Albecq, M., Vermeulen, F., Hein, K., Izeddin, I., & Endesfelder, U. (2024). "EVE is an open modular data analysis software for event-based localization microscopy." bioRxiv. https://doi.org/10.1101/2024.08.09.607224
