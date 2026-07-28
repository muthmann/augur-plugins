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

## Phase-0 trigger overlay

The firmware stamps a device-clock **`Marker` frame** (wire type 4) on the stream at every
modulation phase-0, in step with the J24 camera trigger. Because the chart is on the device
(Teensy) sample clock — not the camera clock — this stream marker is the correctly-aligned phase-0
source (the camera `EXT_TRIGGER` belongs to A1's camera-clock analysis, not here).

- **Show phase-0 trigger markers** (opt-in) overlays them as one toggleable vertical curve
  ("phase-0 trigger") on the chart.
- The **modulation frequency is derived from the marker spacing** (`f = rate / mean marker gap`) and
  shown in the status; the mock emits synthetic markers so the overlay works without hardware.

## Modes

The mode is a **display** choice only. It selects what the chart and the sample readout show; it
never changes a published quantity (ADR 012).

- **RAW** — ADC code and volts (`V = code · 3.3 / 4095`).
- **EXCITATION** — the diode sits behind the PBS in the excitation path and measures the light
  removed from the beam (`I_pd = I_tot − I_exc`), so the plugin inverts against the user-set
  reference: `I_exc = I_tot − I_pd`, with `I_tot` given in photodiode volts.

## Optical log-contrast `a`

`measured_log_contrast` in the published `PhotodiodeOpticalSummaryV1` is **always** the excitation
contrast `a = ln(I_exc,max / I_exc,min)`, in **both** display modes. The detector sits behind the
PBS reject port and measures the complement — that is a property of the bench, not of the display —
so the estimator always runs the `RejectedComplement` geometry against `reference_volts`. A1's
amplitude sweep settles on this value, so a display toggle must not be able to move it (ADR 012).

- **Reference I_tot** (`reference_volts`) is the total-power anchor: the PD reading with the full
  beam diverted into the diode. Until it is set to a real measurement, `a` is withheld.
- **Dark level** (`dark_volts`) + the **Capture dark** button: block the beam and press; the mean of
  the current cache becomes the dark level. It is applied to the detector samples *and* to the
  `I_tot` anchor, so it cancels out of the complement rather than biasing `a` — its job is to keep
  the two sides consistent and to record the calibration the reading was taken under. `dark_id` in
  the sidecar reads `dark-measured` or `dark-none` accordingly.
- The estimator is **fail-closed**: it refuses on ADC clipping, on no headroom above dark, and when
  the anchor is not above the measured signal. A refusal is shown as `a unavailable: <reason>`
  rather than a missing row — a wrong `a` is worse than no `a`.

## Chart

- The visible window is decimated into at most 1 000 buckets; when a bucket covers more than one
  sample the chart shows the bucket **mean** plus a **min/max envelope**, so narrow modulation
  peaks stay visible at any zoom. Windows short enough to fit raw samples render them directly.
- **Moving average** (for the low-voltage regime): a smoothed overlay line plus a numeric readout.
  The window is either a fixed sample count (`avg_samples`, default 4; 1 = off) or — the right
  tool for modulated signals — **one full period of a user-given frequency**
  (`avg_sync_freq_hz`, e.g. the MOD drive frequency): window = `rate / f` samples, which makes
  the mean independent of the modulation phase instead of riding the waveform.

## Data (cache snapshot + disk recording)

- The monitor cache always holds the last *N* seconds (`cache_s`). **Save cache
  snapshot** writes it **once** as `pd_cache_<timestamp>.csv` + JSON sidecar.
- **Start recording** / **Stop recording** buttons tee every incoming sample
  frame to `pd_rec_<timestamp>.pdq`; stopping writes the JSON sidecar. Both
  buttons (and the snapshot) are disabled until a data directory is selected.
- All three are momentary buttons whose presses are forwarded from the UI
  mirror to the live worker as monotonic press counters (`PressLatch`, ADR 010)
  and act only on a press **edge**. The previous unguarded `save_snapshot`
  handler fired on every host settings sync — one unwanted CSV per settings
  change of *any* plugin — and the old `record` checkbox synced the mirror's
  always-false state to the worker, so it could never stay recording. The
  `record` boolean setting remains as a non-schema compatibility alias.

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
round-trips, the forwarded snapshot counter saving exactly once, and the record start/stop
buttons.
