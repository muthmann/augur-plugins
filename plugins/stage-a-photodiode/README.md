# Stage-A Photodiode

Live readout of the photodiode on **board SMA5 → Teensy pin 18 / analog input A4**, from the
free-running ASCII stream the `stage-a-controller` firmware (0.3.0+) emits on its **second** USB
serial port (`PD code=… n=… t_ms=…` at 50 Hz). The port carries no commands, so this plugin is
read-only by construction; the command port belongs to `stage-a-modulation`.

## Modes

- **RAW** — shows the ADC code and its voltage, `V = code · 3.3 / 4095`.
- **EXCITATION** — the photodiode sits in the excitation path behind the PBS and measures the
  light *removed* from the beam: `I_pd = I_tot − I_exc`. Given the user-set reference **I_tot**
  (in photodiode volts — the reading with the full beam on the diode), the plugin shows
  `I_exc = I_tot − I_pd`.

## Views

- a live rolling chart (window length settable, 1–120 s) of the value in the selected mode;
- a compact status table with the newest code/value and Connect/Disconnect actions.

## Ports

**Use `auto` (default recommendation):** it listens briefly on every attached usbmodem/ttyACM
device and connects to the one actually streaming `PD` lines — that is always the Teensy stream
port. Picking the command port manually by mistake is harmless: its binary frames parse to
nothing (no values appear). `mock` generates a synthetic slow sine for hardware-free testing.
