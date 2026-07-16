# Stage-A Photodiode

- **Crate:** `plugins/stage-a-photodiode` (`augur-plugin-stage-a-photodiode`)
- **Firmware:** `stage-a-controller` 0.4.0+ (`PDSTREAM_PDA1`), Teensy **stream port** (second CDC port)
- **Status:** Active (2026-07-16) — replaces the readout half of `stage-a-monitor`

## What it is

A live readout of the photodiode on **board SMA5 → Teensy pin 18 / A4**. Firmware 0.4.0 streams
PDA1 `SamplesU16` frames free-running at `pd_stream_rate_hz` (20 kSa/s default) on its second USB
serial port; a background thread parses them with `stage-a-io`'s `FrameParser` into a bounded raw
ring (up to 130 s / 4 M samples), and the plugin renders a rolling chart (10 ms – 120 s window)
plus the newest value. During a command-port acquisition the firmware mirrors the acquisition
blocks here — every rate change or sample-index jump restarts the ring as a new segment, so the
`index / rate` time base is always consistent.

Two modes:

- **RAW** — ADC code and volts (`V = code · 3.3 / 4095`).
- **EXCITATION** — the diode sits behind the PBS in the excitation path and measures the light
  removed from the beam (`I_pd = I_tot − I_exc`), so the plugin inverts against the user-set
  reference: `I_exc = I_tot − I_pd`, with `I_tot` given in photodiode volts.

## Chart

- The visible window is decimated into at most 1 000 buckets; when a bucket covers more than one
  sample the chart shows the bucket **mean** plus a **min/max envelope**, so narrow modulation
  peaks stay visible at any zoom. Windows short enough to fit raw samples render them directly.
- **Moving average** (for the low-voltage regime): a smoothed overlay line plus a numeric readout.
  The window is either a fixed sample count (`avg_samples`, default 4; 1 = off) or — the right
  tool for modulated signals — **one full period of a user-given frequency**
  (`avg_sync_freq_hz`, e.g. the MOD drive frequency): window = `rate / f` samples, which makes
  the mean independent of the modulation phase instead of riding the waveform.

## Contract

- Owns the Teensy **stream port** exclusively (ADR 006); the port carries no commands, so the
  plugin is read-only by construction. It reuses `stage-a-io` (`default-features = false`) only
  for the PDA1 wire parser — no client, worker, or transport.
- **Frame-independent**: connecting is a checkbox setting; the reader thread and all views
  work with no camera attached (the host only calls `process_frame()` while frames flow).
- Garbage on the port resynchronises at the next CRC-clean frame; skipped bytes and CRC failures
  are counted and shown in the status table's integrity column together with the firmware's
  cumulative drop counter and the segment-restart count.
- `mock` port synthesizes a noisy 5 Hz sine at 20 kSa/s in firmware-sized blocks for
  hardware-free testing.

## Verification

`cargo test -p augur-plugin-stage-a-photodiode` — frame ingestion incl. segment restarts on index
jumps and rate changes, duration-bounded ring with aligned indexes, moving-average window
derivation from the sync frequency, newest-window average, envelope decimation bounds and
min ≤ mean ≤ max, raw rendering for short windows, excitation inversion, mock reader, settings
round-trips.
