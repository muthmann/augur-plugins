# ADR 036 — Stage-A drive bounds and A1 measurement bounds are separate

- **Status:** Accepted
- **Date:** 2026-08-12
- **Relates to:** `stage-a-controller` ADR 004, Stage-A modulation, Stage-A
  photodiode, Stage-A A1

## Context

The Rust plugins repeated a 2 kHz literal in settings, service validation, and
protocol parsing. Raising one copy would make the UI promise a frequency that
another layer refused. It would also confuse two different limits: generating a
periodic drive and resolving that waveform with the photodiode.

The firmware is present in the sibling `stage-a-controller` repository. Its
`board_config.h` fixes the MOD range at 0.01 Hz to 2 kHz and the sine DAC update
ceiling at 40 kHz. At the maximum frequency the waveform has 20 DAC updates per
cycle. No local scope qualification supports a higher drive limit.

Firmware 0.5.0 separately streams the photodiode at 500 kSa/s by default, with a
1 MSa/s configured ceiling. This DMA path still has pending cadence, ENOB, and
analog-front-end bench acceptance. Older command acquisitions and mock data can
report 20 kSa/s.

## Decision

`stage-a-plugin-contract` owns the firmware-qualified Rust constants:

- `DRIVE_FREQUENCY_MIN_MILLIHZ = 10`;
- `DRIVE_FREQUENCY_MAX_MILLIHZ = 2_000_000`;
- `DRIVE_DAC_UPDATE_RATE_HZ = 40_000`.

The modulation settings, setting setter, apply path, service validation, A1
protocol validation, error text, and tests use these constants. The software
maximum remains **2 kHz**. A higher value needs a new firmware waveform design
and scope validation first.

A1 has an additional measurement gate. It reads the current photodiode sample
rate from the owner's fresh status and requires at least 16 samples per cycle.
The accepted A1 limit is therefore `sample_rate_hz / 16`: 1.25 kHz at 20 kSa/s
or 31.25 kHz at 500 kSa/s. This is stricter than Nyquist because A1 measures
waveform extrema and phase, not only signal presence. The drive limit still
wins at 2 kHz on current firmware.

Missing or stale sample-rate status refuses the recording. There is no silent
clamp and no artefact labelled with a frequency that was not applied or could
not be measured under the declared sampling rule.

## Consequences

Some current 20 kSa/s acquisition modes can output 2 kHz but A1 refuses to
record it above 1.25 kHz. The 500 kSa/s stream has enough digital sample density
for the full 2 kHz drive range, subject to the firmware ADR 004 bench acceptance
and the analog photodiode bandwidth.
