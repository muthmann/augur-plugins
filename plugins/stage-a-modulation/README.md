# Stage-A Modulation

Controls the laser modulation input (Hermit J23, `DAC1.4`) through the Teensy **command port**
(the first of the two USB serial ports enumerated by `stage-a-controller` firmware 0.3.0+).

## What it does

- **Power slider** in DAC codes (0–4095). Its upper bound is the **max limit** setting — set that
  to the highest code the connected device tolerates and the slider physically cannot exceed it.
- **Mode**: `CONST` (hold the level), `SINE`, or `SQUARE` with a **frequency** (0.01–2000 Hz) and
  a **min threshold** — the periodic waveforms swing between the threshold and the slider value.
- Every accepted change is sent to the Teensy **immediately** (one `MOD` command); there is no
  Apply button.
- The panel shows the modulation and live DAC code the **board reports** (from the `MOD` reply and
  a 2 Hz `STATUS` poll), not just what was commanded.

## Connecting

- **Connect** is a checkbox in the plugin settings — it opens/closes the command port and works
  **without a running camera** (device I/O lives in a plugin-owned thread, independent of the
  host's frame-driven plugin passes). Connecting never changes the output; only changes made
  while connected are transferred.
- **Output off = power slider at 0.** The firmware output is **set-and-hold**: disconnecting,
  closing the GUI, or a crash leaves the last modulation running (`stage-a-controller` ADR 002).

## Ports

**Use `auto` (default recommendation):** it probes every attached usbmodem/ttyACM device and
connects to the one that answers `HELLO` — that is always the Teensy command port, never the
photodiode stream port. Explicit ports remain selectable; `mock` runs an in-process simulated
controller for hardware-free testing.

Replaying a recording disconnects the plugin defensively; live control itself needs no
capture session.
