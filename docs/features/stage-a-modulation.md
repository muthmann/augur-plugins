# Stage-A Modulation

- **Crate:** `plugins/stage-a-modulation` (`augur-plugin-stage-a-modulation`)
- **Firmware:** `stage-a-controller` 0.3.0+ (`MOD` capability), Teensy **command port**
- **Status:** Active (2026-07-15) — replaces `stage-a-funcgen` and the drive half of
  `stage-a-monitor`

## What it is

The simplest possible laser-modulation control for the Stage-A bench: one power slider in DAC
codes (J23 output, `DAC1.4`), a mode select (`CONST`/`SINE`/`SQUARE`) with frequency
(0.01–2000 Hz) and a min threshold for the periodic modes, and a user-set **max limit** that caps
the slider so a device with a lower tolerated input voltage can never be overdriven from the UI.

Every accepted setting change is transferred to the Teensy **immediately** as one `MOD` command —
no Apply button, no experiment state machine. The panel shows the modulation and live DAC code the
board *reports* (`MOD` reply + 2 Hz `STATUS` poll), not merely the commanded values.

## Contract

- Owns the Teensy **command port** exclusively (one owner per port, ADR 006). The photodiode
  stream port belongs to `stage-a-photodiode`.
- Uses `stage-a-io` (`StageAClient`, `IoWorker`, `Command`) for framing, idempotent retries, and
  the bounded background I/O thread; `process_frame()` never blocks on serial.
- Fail-closed effects gate: the connection only exists while the host execution context allows
  hardware effects.
- Firmware output is **set-and-hold** (`stage-a-controller` ADR 002): disconnecting does not stop
  the modulation. The explicit **Output OFF** action sends `MOD wave=OFF`.
- Safety invariants enforced plugin-side: `level ≤ max_level`, `min_level ≤ level`; the firmware
  waveform peaks at `level` by construction.
- `mock` port runs the firmware-faithful `MockController` in-process for hardware-free tests.

## Verification

`cargo test -p augur-plugin-stage-a-modulation` — mock round trips: immediate transfer on slider
change, board-code echo, max-cap clamping (including schema regeneration), square drive with min
threshold, Output OFF.
