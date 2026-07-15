# Stage-A Bench Stack

- **Status:** Simplified two-plugin setup (2026-07-15, ADR 006)
- **Firmware:** `stage-a-controller` 0.3.0 (Teensy 4.1 on Hermit V2r1, `USB_DUAL_SERIAL`)

## Current shape

The Teensy enumerates as **two** USB serial ports, and each is owned by exactly one plugin:

| Port | Content | Owner |
|---|---|---|
| command port (first) | v1 ASCII commands + PDA1 binary frames | [`stage-a-modulation`](./stage-a-modulation.md) |
| stream port (second) | free-running `PD code=… n=… t_ms=…` lines, 50 Hz | [`stage-a-photodiode`](./stage-a-photodiode.md) |

- **`stage-a-modulation`** — capped power slider + constant/sine/square drive of the laser
  modulation input (J23), transferred to the Teensy immediately; shows the board-reported DAC
  code. Firmware output is set-and-hold; "Output OFF" is the explicit stop.
- **`stage-a-photodiode`** — live readout of SMA5/pin 18/A4, raw or inverted to excitation power
  `I_exc = I_tot − I_pd` against a user-set reference.
- **`stage-a-io`** (shared non-plugin library) — PDA1 wire format, typed client with idempotent
  retries, bounded I/O worker, and a firmware-faithful mock (including the 0.3.0 `MOD` verb).
  The estimator/pdq/sidecar modules are retained for the future A1–A3 experiment plugins.

## History

The earlier commissioning stack (`stage-a-monitor`, `stage-a-funcgen`, `stage-a-a1` — device
monitor with calibrated contrast, waveform familiarisation, and the A1 minimum-depth Bode sweep)
was removed on 2026-07-15 as too complex for the current bench stage (ADR 006). It remains in git
history; the experiment plugins will be rebuilt on the simplified stack when the bench needs
them. Device-ownership and safety rules: ADR 005 (one owner per port, fail-closed effects gate)
as amended by ADR 006.
