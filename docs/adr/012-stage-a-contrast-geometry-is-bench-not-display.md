# ADR 012 — The contrast geometry follows the bench, not the display mode

- **Status:** Accepted
- **Date:** 2026-07-27
- **Relates to:** ADR 006 (two-plugin split), ADR 008 (optical waveform
  inversion), ADR 010 (amplitude sweep), ADR 011 (Pockels transfer
  calibration),
  [Stage-A Photodiode](../features/stage-a-photodiode.md),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

The photodiode plugin has a display toggle: **RAW** plots the detector volts as
measured, **EXCITATION** plots `I_tot − I_pd`. `optical_summary` picked the
estimator's [`ContrastGeometry`] from that toggle — `Direct` under RAW,
`RejectedComplement` under EXCITATION — and published the result as
`PhotodiodeOpticalSummaryV1::measured_log_contrast`.

That made a *published scientific quantity* depend on what the operator
happened to be looking at. It is wrong on the physics and it breaks A1:

- On this bench the detector sits behind the PBS reject port and measures the
  complement `I_pd = I_tot − I_exc`. That is settled by construction
  (`knowledge base: setup/optical-path.md`), not a display choice. Under RAW the
  published value was `ln(I_pd,max / I_pd,min)` — the *detector* contrast, not
  the excitation contrast `a` that every A1 estimand is defined against.
- RAW is the default. A1's amplitude sweep settles `measured_a` against a target
  `a` (`drive_sweep`): with the display left on its default the sweep compares
  the wrong quantity, never settles, times out at 30 s per point, and writes a
  wrong `measured_a` into every sweep sidecar.

The same function also passed `dark_volts: 0.0` and a raw `reference_volts`
anchor, i.e. it dark-corrected one side of the complement and not the other.

## Decision

### 1. Geometry is a property of the optical configuration

`optical_summary` always uses `ContrastGeometry::RejectedComplement`, anchored on
`reference_volts`. `measured_log_contrast` is always the excitation contrast.
The display `Mode` is presentational and never reaches the estimator; the status
readout is labelled `a (excitation)` unconditionally.

If a future bench puts the detector in the excitation path, that is a new
optical configuration ID and a code change here — not a UI toggle.

### 2. The dark level is measured, and applied to both sides

`dark_volts` is a plugin setting with a **Capture dark** action (block the beam,
press; the mean of the current ring becomes the dark level, refused if it is not
below the `I_tot` reference). It is applied to the detector samples *and*
subtracted from the `reference_volts` anchor.

Applied consistently, the DC dark term **cancels** out of the complement — the
excitation is a difference of two readings from the same DC-coupled detector, so
a common offset drops out. Correcting only one side is what would bias `a`, and
that is what the code did. `dark_id` reports `dark-measured` or `dark-none` so a
consumer can tell a real dark measurement from the un-measured default.

### 3. A withheld `a` states its reason

The estimator is deliberately fail-closed (clipping, no headroom, anchor below
signal). Those refusals now surface in the status readout as
`a unavailable: <reason>` instead of the row silently disappearing. This matters
more under the new geometry: with an un-measured anchor left at ADC full scale,
`TotalPowerBelowSignal` is the expected first-run outcome, and the operator has
to be told to set `reference_volts`.

## Consequences

- `measured_log_contrast` is comparable across runs and independent of operator
  UI state; A1's sweep settles against the quantity it targets.
- Runs recorded before this change that were taken with the display on RAW
  carry a detector contrast in `measured_a`. They are distinguishable: their
  sidecar has `anchor_id: "detector-direct"`. Those points must not be mixed
  with `reference-volts` points.
- `excitation_headroom_volts` is, by construction, equal to
  `excitation_min_volts` (both geometries are dark-referenced). The field is
  kept because the contract publishes it, and is now documented as redundant
  rather than silently duplicated.
- First use on a fresh bench requires setting `reference_volts` before any `a`
  is published at all. This is intended: a wrong `a` is worse than no `a`.
