# Stage-A Monitor

Commissioning companion for the Stage-A camera-calibration bench: a live
view of the Teensy photodiode DAQ plus **gated** manual controller commands.

## What it shows

- **Photodiode waveform** — decimated calibrated trace (volts vs ms) from
  the `SamplesU16` stream.
- **Live optical contrast** — `a = ln(V_max/V_min)` from dark-corrected,
  clipping-guarded percentile extrema (see `stage-a-io::estimator`). An
  invalid window shows *why* (clipped / no headroom / too short) instead of
  a silently wrong number.
- **Stream integrity** — CRC failures, resync skips, frame-sequence gaps,
  and ADC overruns. Any nonzero counter means the current point is invalid.

## Controls (host actions on the status table)

`Connect`, `Disconnect`, `Start acquisition`, `Stop`, and an expert
`Apply drive` modal (integer DAC codes; the optical contrast is always
measured, never assumed from the drive). Commands are actions — not
settings — so a reloaded settings file can never arm hardware.

## Safety

The plugin fails closed: the serial port opens only when the host reports
`LiveCapture` with `effects_allowed` (plugin ABI v5 execution context).
Replay and offline analysis can never emit a serial byte, and an existing
connection is shut down the moment effects are revoked. The firmware-side
watchdog independently drops the controller to `SAFE_IDLE` if the host
disappears.

## Use it for (commissioning checklist)

1. Wiring / voltage-range check at both detector loads.
2. Dark-level measurement for the estimator calibration.
3. Coherent-crosstalk test (H14): drive on, light blocked — the waveform
   view and `a` readout must stay at the noise floor.
4. USB-throughput sanity (watch the integrity counters at full rate).
