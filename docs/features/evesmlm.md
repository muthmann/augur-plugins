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

## Registration

Register the plugins in `augur-gui` in this order:

1. `EveSmlmCandidatePlugin`
2. `EveSmlmFittingPlugin`
3. `EveSmlmPostProcPlugin`
