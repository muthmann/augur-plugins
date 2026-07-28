# Stage-A Photodiode

Live readout of the photodiode on **board SMA5 → Teensy pin 18 / analog input A4**, from the
free-running PDA1 `SamplesU16` stream the `stage-a-controller` firmware (0.4.0+) emits on its
**second** USB serial port (20 kSa/s default). The port carries no commands, so this plugin is
read-only by construction; the command port belongs to `stage-a-modulation`.

## Modes

- **RAW** — shows the ADC code and its voltage, `V = code · 3.3 / 4095`.
- **EXCITATION** — the photodiode sits in the excitation path behind the PBS and measures the
  light *removed* from the beam: `I_pd = I_tot − I_exc`. Given the user-set reference **I_tot**
  (in photodiode volts — the reading with the full beam on the diode), the plugin shows
  `I_exc = I_tot − I_pd`.

## Views

- a live rolling chart (window length settable, 1–120 s) of the value in the selected mode;
- a compact status table with the newest code/value, moving average, integrity,
  recording state, and connection state.

## Ports

**Use `auto` (default recommendation):** it listens briefly on every attached usbmodem/ttyACM
device and connects to the one actually streaming CRC-clean PDA1 sample frames — that is always
the Teensy stream port. `mock` generates a synthetic sine for hardware-free testing.

## Owner control service

This plugin is the sole owner of the Teensy photodiode stream port. Workflow
plugins control named recordings through the versioned
`stage_a.photodiode.control.v1` service and consume bounded
`stage_a.photodiode_summary.v1` snapshots. They never open the serial port or
receive raw sample arrays through the control plane; finalized PDQ files remain
the replay and analysis source of truth.

The snapshot's `stream.level` block carries the settled detector level over the
moving-average window in **raw** detector volts — the ADC map only, never the
RAW/EXCITATION display transform and never the optical geometry transform. It
also reports the window's peak-to-peak spread and the sample index it ends at,
so a consumer can prove a reading was taken *after* it changed something without
a shared clock. Unlike `optical_summary` it never refuses: it stays present
while the window clips (flagged), because the Pockels transfer sweep needs a
reading exactly where the reject-port detector is brightest.
