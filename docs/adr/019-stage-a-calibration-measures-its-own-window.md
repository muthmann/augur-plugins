# ADR 019 — The calibration owns its measurement window, and judges itself against its own noise

- **Status:** Accepted
- **Date:** 2026-07-30
- **Relates to:** ADR 011 (measured Pockels transfer calibration), ADR 016 (the
  lobe is two observed codes), ADR 017 (rail detection and withheld `a`),
  [Stage-A Pockels Transfer Calibration](../features/stage-a-pockels-calibration.md),
  [Stage-A Photodiode](../features/stage-a-photodiode.md)

## Context

On 2026-07-30 the operator ran a transfer-curve sweep on the orange bench. The
curve on the plot was clean and unmistakably a Pockels lobe. The plugin reported
it as bad on three counts at once:

- residual **22.3 %** of the detector span,
- hysteresis **25.7 %** — "the cell is drifting or the settle time is too short",
- **34 of 98** points "clipped the ADC … add attenuation and re-measure".

The record is kept verbatim at
`plugins/stage-a-modulation/testdata/pockels-20260730-083123.json`. All three
numbers were artifacts, and the fit underneath them was exactly right.

### Every point was four ADC samples

Each archived `volts` is an exact multiple of a quarter code —
`0.002619 V = 13/4`, `0.04412 V = 219/4`, `0.05581 V = 277/4`. The stream ran at
500 kSa/s, so each "settled" point was **8 µs** of signal, captured *after* the
sweep had already waited 4 ms for the cell to arrive.

`PhotodiodeLevelV1` was computed over `avg_window_samples` — the photodiode
plugin's **chart smoothing** setting, default four samples. A display preference
was setting the precision of a physical calibration, and nothing named that
coupling anywhere. At 20 kSa/s it had been 200 µs and merely mediocre; the move
to 500 kSa/s made it 8 µs without changing a line of code.

The consequences were all downstream of that one number. Per-point scatter came
out at σ = 11.3 mV against a 50.8 mV lobe, which *is* the reported 22.3 %
residual.

### The hysteresis was the same noise, counted twice

Both passes measure one curve, so at a matched code they differ by two
independent errors of scale σ, and `E|Δ| = σ√2·√(2/π) = 1.128 σ`. For this sweep
that predicts 12.8 mV; the measured mean |up − down| was 13.0 mV. The metric was
reporting its own point noise as cell drift, and a fixed 5 % threshold cannot
tell the two apart on any bench whose points are not far quieter than that.

### The clipping flag was ADR 017's bug, one layer up

`current_level` still marked a window clipped when its minimum fell within a
fixed 4 codes of the rail. The reject-port detector's dark end genuinely sits at
~3 codes (2.6 mV), so a third of every sweep was flagged. The span-relative
margin that ADR 017 introduced in the contrast estimator had never been carried
into the published level.

### And a gate that would have got worse

`fit_transfer` refused a sweep whose between-code span did not exceed the median
`peak_to_peak_volts` of the settled windows. That compares a span of *means*
against a *raw within-window excursion* — wrong by √N, and wrong in a way that
tightens as the averaging window grows. This sweep cleared it by a factor of 1.9.
Lengthening the window without touching this gate would have refused the very
sweeps the longer window was meant to rescue.

## Decision

### 1. The published level is a measurement, not a view of the chart

`PhotodiodeStreamV1.level` is averaged over a **fixed duration owned by the
photodiode plugin** (`LEVEL_WINDOW_SECONDS = 20 ms`), independent of
`avg_samples` and `avg_sync_freq_hz`. `sample_count` reports what it actually
was. The chart's own averaging is untouched — it remains an operator preference,
and it no longer reaches anything downstream.

A duration rather than a sample count, because what averages noise down is
time × bandwidth, not samples. 20 ms specifically because a boxcar of exactly one
mains period has a null at 50 Hz and every harmonic of it. At 500 kSa/s that is
10 000 samples in place of 4.

### 2. Rail detection is shared, not re-derived

`stage_a_io::near_rail_margin` is the single span-relative margin, used by both
`estimate_contrast` and the published level. A detector running a few codes above
zero is not truncating; the rails themselves stay guarded at every gain.

### 3. Nothing judges the sweep by `peak_to_peak_volts`

Both surviving gates use the fit's own RMS residual, which is the scatter of the
*averaged* points about the curve — the same quantity the lobe amplitude is
measured in, so the comparison is dimensionally honest and independent of
whatever window the owner publishes.

**Is a lobe resolved?** Refuse when `rms ≥ 0.5·|span|`. The threshold needs
margin on both sides because a free period search over pure noise does not return
zero amplitude: with `n` points the quadrature pair has scale `σ√(2/n)`, and the
best of a 600-step scan inflates it by about `√(2 ln 600)`. Measured, that puts
noise-only quality at 0.7–1.0 (0.97 in the regression fixture) while the noisiest
real record on file reads 0.22. Half-way between is a plain statement — the lobe
must be at least twice its own scatter — with better than 2× margin either way.

**Is the up/down difference drift?** Compare `hysteresis` against
`1.128 · rms / |span|`, the value it takes under noise alone. The ratio has two
derivable endpoints: **1.0** for pure noise, and **1.77** for pure drift, because
a systematic offset inflates the residual too (the fit splits the difference
between the passes, carrying `√(Δ²/4 + σ²)` while the metric carries
`√(Δ² + (1.128σ)²)`). The range is narrow and it is not optional to know that: a
generous multiple of the floor — 2×, the obvious first guess — sits above *both*
endpoints and never fires at all. The cut is at **1.33**, which detects a
systematic offset around 1.5× the point noise. Both endpoints are asserted in
`the_hysteresis_ratio_sits_between_its_two_derived_endpoints`.

### 4. Settling is a duration

`SETTLE_SECONDS = 0.1`, converted through the photodiode's published sample rate,
replacing a bare `SETTLE_SAMPLES = 2_000` that was written for 20 kSa/s and had
silently become 4 ms. `SETTLE_SAMPLES` remains only as the fallback for a stream
that has not published a rate. Settling is a property of the HV amplifier and the
crystal; nothing about it follows the acquisition rate.

### 5. Clipping says what it costs

Rail-touching points truncate the reported detector extrema, and with them the
`I_tot` lower bound. They do **not** move `V_null` or `Vπ`, which come from the
shape. The warning says so, and no longer advises attenuation — for a
reject-port detector it is the *dark* end that reaches the bottom rail, so the
fix is more gain, not less light.

## Consequences

- The chart's averaging setting no longer has any downstream effect. This is a
  behaviour change for anyone who had turned it up expecting quieter sweep
  points; they now get a quiet sweep without asking.
- A sweep costs ~120 ms per point (100 ms settle + 20 ms window), so ~12 s for
  the full 98-point pass — still inside the documented ~20 s and far inside
  `POINT_TIMEOUT`.
- The real bench record is a regression fixture. Synthetic sweeps could not have
  caught any of this: they carry uniform noise, while a real detector's noise is
  signal-proportional, and the metrics that broke were all compared against zero.
- The synthetic fixture's own "noise" was itself wrong for this question — a
  wobble alternating with the point index is perfectly anti-correlated between
  the two passes, i.e. systematic. `calibration::scatter` replaces it with
  deterministic per-`(code, direction)` scatter.
- Nothing in `augur-rs` changes; this is entirely inside the Stage-A plugins and
  their shared I/O crate.
