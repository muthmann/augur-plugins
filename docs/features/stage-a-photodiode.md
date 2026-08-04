# Stage-A Photodiode

- **Crate:** `plugins/stage-a-photodiode` (`augur-plugin-stage-a-photodiode`)
- **Firmware:** `stage-a-controller` 0.4.0+ (`PDSTREAM_PDA1`), Teensy **stream port** (second CDC port)
- **Status:** Active (2026-07-16) — replaces the readout half of `stage-a-monitor`
- **Design:** [ADR 006](../adr/006-stage-a-two-plugin-split.md) (the split),
  [ADR 012](../adr/012-stage-a-contrast-geometry-is-bench-not-display.md) (the
  contrast geometry),
  [ADR 024](../adr/024-stage-a-photodiode-learns-its-own-anchor.md) (the
  learned total-power anchor; dark cancels),
  [ADR 017](../adr/017-stage-a-rail-detection-and-withheld-a-reasons.md)
  (span-relative rail detection; the published refusal reason),
  [ADR 019](../adr/019-stage-a-calibration-measures-its-own-window.md) (the
  published level owns its window)

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
- **EXCITATION** — the diode sits at the PBS reject port and measures the light
  removed from the sample beam (`I_pd = I_tot − I_exc`), so the plugin inverts against the learned
  total-power anchor: `I_exc = I_tot − I_pd`. Nothing to enter — see below.

## Optical log-contrast `a`

`measured_log_contrast` in the published `PhotodiodeOpticalSummaryV1` is **always** the excitation
contrast `a = ln(I_exc,max / I_exc,min)`, in **both** display modes. The detector sits behind the
PBS reject port and measures the complement — that is a property of the bench, not of the display —
so the estimator always runs the `RejectedComplement` geometry against the learned anchor. A1's
amplitude sweep settles on this value, so a display toggle must not be able to move it (ADR 012).

- **`I_tot` is learned, not entered** (ADR 024). The plugin latches the highest
  smoothed detector level it has seen since the port was opened. On the reject
  port the detector is brightest exactly where the excitation is extinguished,
  so that reading *is* `I_tot` — and the Pockels transfer sweep, which walks the
  DAC across the whole lobe, lands on the excitation null by construction. Run
  the sweep once and the anchor is right. The latch is over completed 64-sample
  summary-cell means, so one noise spike cannot pin it high, and it survives
  segment restarts (a rate change or an acquisition handover does not move the
  optics). Reconnecting the port relearns it. Provenance is published as
  `anchor_id = "observed-peak@<sample index>"`.
- **There is no dark level, and that is exact, not an approximation.** With a DC
  dark offset `D`, the excitation is `(I_tot,obs − D) − (v − D) = I_tot,obs − v`
  — the offset cancels, because both sides are readings from the same
  DC-coupled detector. `dark_volts` is fixed at 0 and `dark_id` reads
  `dark-cancels`. Two unit tests hold this down: one asserts that shifting the
  whole trace *and* the anchor leaves `a` unchanged to 1e-9, and a companion
  asserts that correcting only one side *does* move it, so the first cannot pass
  vacuously.
- The estimator uses only marker-bounded windows containing at least **two
  complete modulation cycles**, ending on phase 0. It no longer estimates
  extrema from an arbitrary trailing sample count; a low-frequency trace that
  does not fit the bounded window is withheld rather than phase biased.
- The estimator is **fail-closed**: it refuses when no anchor has been observed
  yet, on incomplete cycles, on ADC clipping, and when the excitation never dims
  below the brightest the detector has been — where there is no complement left
  to take a contrast of, and the fix is to run the transfer sweep. A refusal is shown as `a unavailable: <reason>`
  rather than a missing row — a wrong `a` is worse than no `a`.
- The refusal reason is also **published** on the contract as
  `PhotodiodeSummaryV1::optical_unavailable`, so a consumer that gates on `a`
  (A1's a₀ lock, amplitude sweep and frequency ladder) can name the gate rather
  than report absence. Set exactly when `optical_summary` is absent and a window
  existed to judge (ADR 017).
- Clip detection is **span-relative**: the near-rail margin is capped at 5 % of
  the window's own peak-to-peak code range. At this detector's 0.5–15 mV
  operating range the whole waveform sits inside the bottom ~20 of 4095 codes,
  where the former absolute 4-code margin classified 30 % of a clean sine as
  clipped and withheld `a` unconditionally. The rails themselves (code 0, full
  scale) stay guarded at every gain, so a waveform driven below zero is still
  refused (ADR 017).
- The same span-relative margin decides `PhotodiodeLevelV1::clipped`, so the
  Pockels sweep is not told that a detector running a few codes above zero is
  truncating.

## The published level owns its window

`PhotodiodeStreamV1.level` is the settled detector reading other plugins consume
— today, the modulation plugin's Pockels transfer sweep, which reads one per
commanded DAC code. It is averaged over a **fixed 20 ms**, set here and
independent of every display setting; `sample_count` reports what it was.

It used to be averaged over the chart's moving-average window below. That made a
display preference set the precision of a physical calibration: at the bench's
500 kSa/s the default of four samples published **8 µs** of signal per settled
code, and a clean Pockels curve came back reported as a 22 % residual with 26 %
"hysteresis" (ADR 019). 20 ms is one mains period, so the boxcar has a null at
50 Hz and every harmonic of it — and the chart's averaging is once again nothing
but a chart setting.

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

## Port discovery

`auto` listens briefly on every candidate port and keeps the one streaming CRC-clean PDA1 sample
frames — the probe, not the port name, is what identifies the stream port. Which ports are
candidates is platform-specific and shared with the modulation plugin through
`stage-a-io::transport::candidate_ports()`: `cu.usbmodem*` on macOS (the callout node only, since
every device is listed twice), `ttyACM*` on Linux, and every USB-classified `COMn` on Windows,
where the name carries no device information at all (ADR 032). The settings picker lists exactly
the same set, so a port offered in the dropdown is one `auto` would also have probed. When nothing
qualifies, the error names the ports the OS did enumerate.

## Contract

- Owns the Teensy **stream port** exclusively (ADR 006); the port carries no commands, so the
  plugin is read-only by construction. It uses `stage-a-io` for the PDA1 wire parser and for port
  discovery — no client, worker, or transport.
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
