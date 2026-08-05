# ADR 017 — Rail detection is span-relative, and a withheld `a` names its gate across the plugin boundary

- **Status:** Accepted
- **Date:** 2026-07-30
- **Relates to:** ADR 011 (Pockels transfer calibration), ADR 012 (contrast
  geometry is bench, not display), ADR 013 (event-count depth lock), ADR 014
  (frequency ladder),
  [Stage-A Photodiode](../features/stage-a-photodiode.md),
  [Stage-A A1 Analysis](../features/stage-a-a1.md),
  [Stage-A A1 Event-Count Depth](../features/stage-a-a1-event-count.md)

## Context

Two independent defects met on the bench and produced the same symptom: every
`a₀` action refused, and the panel could not say why.

### The clip guard was calibrated for a volt-scale detector

`estimate_contrast` is fail-closed on ADC clipping (ADR 012 §3): codes within
`CLIP_MARGIN_CODES = 4` of either rail counted as clipped, and more than 1 ‰ of
such samples refused the window. The margin was an **absolute** code count.

The Stage-A reject-port detector operates around **0.5–15 mV** — the range the
µV-granularity calibration inputs and the span-relative Pockels fit were
introduced for. At 3.3 V over 4095 codes (0.806 mV per code) that whole waveform
lives inside the bottom ~20 codes, so a 4-code margin covers 3.2 mV of a 14.5 mV
signal. A perfectly clean millivolt-scale sine put **30.7 %** of its samples
inside the "near the rail" band, 300× over the 1 ‰ limit, and was refused as
clipped. None of those codes was the rail; they were the signal.

The refusal was therefore unconditional at bench gain: `optical_summary` was
never published, and `a` was never available.

### A withheld `a` did not survive the plugin boundary

ADR 012 §3 established that a refusal is stated, not silent — but only in the
photodiode plugin's own status readout. `PhotodiodeSummaryV1` carried
`optical_summary: Option<…>` and nothing else, so a consumer saw absence with no
cause.

A1 gates the `a₀` lock, the amplitude sweep and the frequency ladder on that
value, and refused all three with one fixed sentence:

> No photodiode-measured a — connect the photodiode and anchor I_tot first

which named the two most common causes whatever the real one was. With a railed
window, too few trigger markers, or a stale snapshot, that message sent the
operator to re-check an anchor that was already correct. The resting status line
was no better: `a = — (photodiode: connected)`.

The same shape of problem sat on A1's event ingestion. With **Live analysis**
off, nothing is ingested at all, and the panel reported `0 events, …;
free-running (no EXT_TRIGGER)` — a description of a toggle, phrased as a
description of the bench. The frequency ladder refuses without phase-0 markers,
so an operator with Live analysis off was sent to check trigger wiring.

## Decision

### 1. The rail margin is capped against the window's own span

The near-rail margin exists to catch a waveform that is *about to* truncate,
which is only meaningful while the margin is small compared to the signal. It is
now `min(CLIP_MARGIN_CODES, floor(span · CLIP_MARGIN_SPAN_FRACTION))` with
`CLIP_MARGIN_SPAN_FRACTION = 0.05`, where `span` is the window's observed
peak-to-peak code range.

- Volt-scale windows (span ≥ 80 codes) keep the previous 4-code margin exactly.
- Millivolt-scale windows collapse the margin to 0, which leaves **precisely the
  rails** — code 0 and `full_scale_code` — classified as clipped.

Genuine saturation is still refused at every gain: a waveform driven below zero
pins samples *at* code 0, and the 1 ‰ limit still catches it. `MAX_CLIP_FRACTION`
is unchanged; this ADR narrows what counts as a rail, not how much clipping is
tolerated.

This follows the same reasoning as the span-relative `NoModulation` threshold in
the Pockels fit (ADR 011): a fixed absolute voltage cut cannot serve a detector
whose gain is a bench property.

### 2. The refusal reason is published on the contract

`PhotodiodeSummaryV1` gains `optical_unavailable: Option<String>` — additive in
V1, `#[serde(default)]`, skipped when absent, so older owners and consumers are
unaffected. It carries the owner's `EstimateError` rendering, set exactly when
`optical_summary` is `None` **and** a window existed to judge.

A1 consumes it through one `measured_a_blocker()` helper that returns the
operator action, checked in the order the data flows: no status snapshot → not
connected → stale snapshot → the owner's reason → no samples yet. Every gate that
needs `a` quotes it, and the resting status line renders it without the operator
pressing anything.

### 3. A1 distinguishes "Live analysis is off" from "no trigger"

The status line and the frequency-ladder refusal name whichever it is. Marker
count alone cannot tell them apart, and only one of them is fixed with a
screwdriver.

## Consequences

- Millivolt-scale windows publish `a`. They are **quantisation-limited**: with a
  ~19-code span the complement's excitation minimum is a fraction of one code,
  so `a` is sensitive to single-code noise. The estimator's 1st/99th-percentile
  extrema absorb spikes, but a bench wanting precise `a` at high contrast should
  still raise the detector gain. This ADR makes such windows *estimable*, not
  *precise* — the clip guard was never the right place to enforce resolution.
- The `Clipped` refusal now means the signal reached a rail, not that it sat near
  one. A window previously refused for being small is now accepted, so an
  operator who read that refusal as "gain too low" loses that (misleading) cue.
- Every A1 gate on `a` reports one of a bounded set of causes traceable to the
  owner. New `EstimateError` variants surface in A1 with no A1 change.
- `optical_unavailable` is a human-readable string, not a typed error. The
  contract crate is serde-only and does not depend on `stage-a-io`, and the
  consumer renders rather than branches on it. A consumer that needs to *act*
  per-variant will need the typed error on the contract instead.
