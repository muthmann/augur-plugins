# ADR 024 — The photodiode learns its own total-power anchor, and dark cancels

- **Status:** Accepted
- **Date:** 2026-08-01
- **Relates to:** ADR 006 (two-plugin split), ADR 011 (Pockels transfer
  calibration), ADR 012 (contrast geometry is bench, not display),
  ADR 019 (calibration measures its own window),
  [Stage-A Photodiode](../features/stage-a-photodiode.md)

## Context

The Stage-A detector sits behind the PBS reject port and reads the complement
of the excitation, `I_pd = I_tot − I_exc`. Recovering the excitation contrast
`a = ln(I_exc,max / I_exc,min)` therefore needs `I_tot`, and the plugin asked
the operator for it through four settings:

| setting | what it wanted |
| --- | --- |
| `reference_volts` | the detector reading with the whole beam sent into it |
| `reference_anchor_id` | a name for that reading, for provenance |
| `reference_confirmed` | a tick-box asserting it was measured *for this setup* |
| `dark_volts` + `capture_dark` | the reading with the beam blocked |

Nothing downstream would publish `a` until the tick-box was ticked, and editing
either the value or the id un-ticked it. That gate is why the A1 plugin's
photodiode depth source appeared not to work at all: the default configuration
withholds `a` permanently, and the reason surfaces only as one line of status
text on a different plugin.

Three things were wrong with this:

1. **`I_tot` is measurable, not typeable.** Getting it by hand means blocking
   the sample arm, reading a number off the chart, and typing it back — a
   procedure that is re-done, or silently not re-done, every time the optics
   are touched. The tick-box exists precisely because nobody can tell from the
   number whether it is current.

2. **The bench already measures it.** The Pockels transfer sweep (ADR 011)
   walks the DAC across the whole lobe, which drives the excitation through its
   null by construction. At the null all the light goes to the reject port, so
   the detector reading *there* is `I_tot`. The calibration archive has
   recorded `detector_volts_at_null` all along, described as "a lower bound on
   the total-power anchor".

3. **The dark level cancels.** With a DC dark offset `D`, the corrected
   excitation is `(I_tot,obs − D) − (v − D) = I_tot,obs − v`. The `D` terms
   cancel *exactly*, because both sides are readings from the same DC-coupled
   detector. A dark setting can therefore only do harm: entered on one side
   only, it biases `a`; entered on both, it does nothing.

## Decision

The photodiode learns `I_tot` from its own stream and the four settings are
removed.

`SharedState` latches `observed_peak_code`: the maximum of the completed
64-sample summary-cell means since the port was opened. On the reject port the
detector is brightest exactly where the excitation is extinguished, so this is
`I_tot` by construction. The latch is over cell *means*, not raw samples, so a
single noise spike cannot pin the anchor high for every later `a`.

Nothing has to be entered, and nothing has to be confirmed: the transfer sweep
the operator already runs before any measurement lands on the excitation null
and teaches the anchor as a side effect. The latch survives segment restarts —
a rate change, a dropped sample or an acquisition handover does not move the
optics, and the sweep that teaches the anchor is followed by exactly such a
handover.

Dark correction is removed entirely. `AdcCalibration::dark_volts` is fixed at
zero and the published `dark_id` says `dark-cancels` rather than implying an
unmeasured zero.

`PhotodiodeCalibrationV1` keeps its shape; `anchor_id` becomes
`observed-peak@<sample index>` — provenance for a number nobody typed.

The removed setting keys are still accepted by `set_setting` and ignored, so a
configuration written before this ADR still loads.

## Consequences

- The photodiode depth source works out of the box. `a` is withheld only for
  reasons that are actually about the measurement — too few cycles in the
  window, clipping, or an excitation that never dims below the brightest the
  detector has been.
- That last case is a new refusal, and an honest one: if the modulation has not
  yet been anywhere dimmer than the running peak, there is no complement to
  take a contrast of. The message says to run the transfer sweep.
- `a` is now invariant to any DC offset in the front end, provably — there is a
  unit test asserting that shifting the whole detector trace and the anchor
  together leaves `a` unchanged to 1e-9, and a companion test asserting that
  correcting one side alone *does* change it, so the first test cannot pass
  vacuously.
- **Before any sweep has run, the estimator fails closed by construction.** The
  obvious worry is that with only a modulated trace observed, the "total power"
  sits barely above the signal and `a` explodes. It cannot: the anchor is the
  maximum of 64-sample cell *means*, which for a modulated trace is always below
  the robust high percentile the estimator compares against, so
  `TotalPowerBelowSignal` fires instead. There is a test asserting exactly that
  refusal — by name, not merely "some error" — at two very different
  cycle-to-cell ratios, because a refusal for want of whole cycles would
  otherwise make it pass without exercising the anchor at all.
- The anchor is a running maximum, so it never decreases within a session.
  Reducing the laser power mid-session leaves it too high until the port is
  reopened. This is the conservative direction — `a` comes out low rather than
  high — and reconnecting resets it (`connect()` clears the ring).
- **The trustworthy path is still the transfer sweep.** The anchor is only as
  good as the dimmest excitation the detector has seen; a bench that has never
  been driven through the null has no anchor worth the name, and the estimator
  says so. If that ever needs to be stronger, the modulation owner's fit already
  holds a settled `detector_volts_at_null` and could push it over as a scoped
  command — the same direction A1 already commands the photodiode in, so no
  dependency cycle.
- The modulation plugin's calibration folder gains a stated purpose: its
  archived `detector_volts_at_null` is the anchor a past run's `a` was measured
  against.

## Alternatives considered

- **Publish the anchor from the modulation plugin's fit.** It has the number
  already. Rejected: the photodiode is the upstream owner in the existing
  dependency direction, and pushing the anchor back down it creates a cycle
  between the two device owners for a value the detector can observe itself.
- **Keep `I_tot` as an optional override.** Rejected on the operator's own
  reading of it: an escape hatch that is almost never the right path is still a
  setting to understand, and its presence is what made the happy path feel
  conditional.
