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

## Actions

- **Connect / Disconnect** — open/close the command port. Connecting never changes the output;
  only changes made while connected are transferred.
- **Output OFF** — sends `MOD wave=OFF` (DAC code 0). Needed because the firmware output is
  **set-and-hold**: disconnecting, closing the GUI, or a crash leaves the last modulation running
  (`stage-a-controller` ADR 002).

## Ports

Select the Teensy *command* port (binary protocol), not the photodiode stream port. `mock` runs an
in-process simulated controller for hardware-free testing; `auto` picks the first
usbmodem/ttyACM device. If you picked the wrong physical port, HELLO simply times out — pick the
other one.

Hardware commands only flow while the host execution context allows effects (live capture);
otherwise the connection is torn down and the panel shows the lock reason.
