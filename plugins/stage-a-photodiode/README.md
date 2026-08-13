# Stage-A Photodiode

Live readout of the photodiode on **board SMA5 → Teensy pin 18 / analog input A4**, from the
free-running PDA1 `SamplesU16` stream the `stage-a-controller` firmware (0.4.0+) emits on its
**second** USB serial port (20 kSa/s default). The port carries no commands, so this plugin is
read-only by construction; the command port belongs to `stage-a-modulation`.

## Detector placement and modes

Set **Detector placement** to the physical geometry before recording:

- **PBS rejected port** — complementary excitation. This is the legacy mode and
  uses the learned `I_tot` anchor described below.
- **camera path (direct)** — direct sample of the path sent to the camera.
- **emission path (direct fluorescence)** — direct fluorescence after the
  emission filter. Set **Fraction sent to PD** to the beamsplitter fraction
  (`0.5` for 50:50), block the beam and press **Capture lamp-off dark**.

The two direct modes compute `a = ln((V_max-D)/(V_min-D))`. They never use
`I_tot`. The splitter fraction is written as provenance and is not used to
rescale log contrast. Direct-path `a` is withheld until a lamp-off dark has
been explicitly captured. The PD artifacts record its value, ID, source,
capture time, and age. The numeric dark field is only a draft; pressing **Use
manual dark** activates it with source `manual`. This explicit step prevents UI
settings replay from replacing a captured lamp-off reference.

- **RAW** — shows the ADC code and its voltage, `V = code · 3.3 / 4095`.
- **EXCITATION** — in rejected-port placement, the photodiode sits behind the PBS and measures the
  light *removed* from the beam: `I_pd = I_tot − I_exc`, so the plugin shows `I_exc = I_tot − I_pd`.
  `I_tot` is **learned, not entered**: it is the brightest smoothed reading the detector has taken
  since the port opened, which on the reject port is where the excitation is extinguished. The
  Pockels transfer sweep drives through that null by construction, so running it once teaches the
  anchor. There is no dark level either — a DC offset cancels exactly out of the complement.
  See [ADR 024](../../docs/adr/024-stage-a-photodiode-learns-its-own-anchor.md).

RAW/EXCITATION is a display choice. Detector placement is the scientific
geometry and controls the estimator independently of the chart mode.

## Views

- a live rolling chart (window length settable, 1–120 s) of the value in the selected mode;
- a compact status table with the newest code/value, moving average, integrity,
  recording state, and connection state.

## Ports

**Use `auto` (default recommendation):** it listens briefly on every attached USB serial port and
connects to the one actually streaming CRC-clean PDA1 sample frames — that is always the Teensy
stream port. `mock` generates a synthetic sine for hardware-free testing.

Which ports get listened to is platform-specific: `cu.usbmodem*` on macOS, `ttyACM*` on Linux,
and every USB-classified `COMn` on Windows (ADR 032).

## Owner control service

This plugin is the sole owner of the Teensy photodiode stream port. Workflow
plugins control named recordings through the versioned
`stage_a.photodiode.control.v1` service and consume bounded
`stage_a.photodiode_summary.v1` snapshots. They never open the serial port or
receive raw sample arrays through the control plane; finalized PDQ files remain
the replay and analysis source of truth.

The snapshot's `stream.level` block carries the settled detector level in **raw**
detector volts — the ADC map only, never the RAW/EXCITATION display transform and
never the optical geometry transform. It is averaged over a fixed **20 ms**
owned here and independent of the chart's moving-average setting, because that
setting is a display preference and this is a measurement: deriving one from the
other let a default of four samples publish 8 µs per point at 500 kSa/s and
report a clean Pockels calibration as a 22 % residual
([ADR 019](../../docs/adr/019-stage-a-calibration-measures-its-own-window.md)).
It
also reports the window's peak-to-peak spread and the sample index it ends at,
so a consumer can prove a reading was taken *after* it changed something without
a shared clock. Unlike `optical_summary` it never refuses: it stays present
while the window clips (flagged), because the Pockels transfer sweep needs a
reading exactly where the reject-port detector is brightest.

When `optical_summary` *is* refused, `optical_unavailable` on the same snapshot
carries the reason, so a consumer that gates on `a` can name the gate instead of
reporting absence. Clip detection is span-relative — the near-rail margin is
capped at 5 % of the window's own peak-to-peak span, so this detector's 0.5–15 mV
operating range is not mistaken for a waveform truncating at code 0. See
[ADR 017](../../docs/adr/017-stage-a-rail-detection-and-withheld-a-reasons.md).
