# Stage-A Photodiode

- **Crate:** `plugins/stage-a-photodiode` (`augur-plugin-stage-a-photodiode`)
- **Firmware:** `stage-a-controller` 0.3.0+ (`PDSTREAM`), Teensy **stream port** (second CDC port)
- **Status:** Active (2026-07-15) — replaces the readout half of `stage-a-monitor`

## What it is

A minimal live readout of the photodiode on **board SMA5 → Teensy pin 18 / A4**. The firmware
streams `PD code=<mean> n=<reads> t_ms=<millis>` lines at 50 Hz on its second USB serial port; a
background thread parses them into a bounded ring, and the plugin shows the newest value plus a
rolling chart (1–120 s window).

Two modes:

- **RAW** — ADC code and volts (`V = code · 3.3 / 4095`).
- **EXCITATION** — the diode sits behind the PBS in the excitation path and measures the light
  removed from the beam (`I_pd = I_tot − I_exc`), so the plugin inverts against the user-set
  reference: `I_exc = I_tot − I_pd`, with `I_tot` given in photodiode volts.

## Contract

- Owns the Teensy **stream port** exclusively (ADR 006); the port carries no commands, so the
  plugin is read-only by construction and needs no protocol library — it depends only on
  `serialport` and parses one line format.
- Same fail-closed effects gate as the other stage-a plugins for consistent device handling.
- Garbage on the port (e.g. the binary command port picked by mistake) parses to nothing and is
  bounded — it can neither grow memory nor produce fake values.
- `mock` port synthesizes a slow sine for hardware-free testing.

## Verification

`cargo test -p augur-plugin-stage-a-photodiode` — line parsing (including clamping and rejection),
excitation inversion against the reference, mock reader filling ring/series, ring bound.
