# ADR 025 — Drive settings clamp into the achievable range, they never refuse

- **Status:** Accepted
- **Date:** 2026-08-01
- **Relates to:** ADR 008 (optical waveform inversion), ADR 016 (lobe endpoints,
  not a distance), [Stage-A Modulation](../features/stage-a-modulation.md),
  [Stage-A Optical Waveform](../features/stage-a-optical-waveform.md)

## Context

The calibrated drive has two coupled controls — the cycle-mean lobe point `ū`
and the optical depth `a` — bounded by one shared constraint: the peak of the
swing must stay under the top of the Pockels lobe, and under the operator's DAC
max limit.

Every setting that feeds that constraint was validated transactionally:

```rust
let previous = self.mode;
self.mode = mode;
if let Err(error) = self.validate_drive() {
    self.mode = previous;          // snap back
    return Err(error);
}
```

Which produces this, from the bench:

> when using one mode, setting some of the I and a parameters it is blocking
> choosing other modes sometimes (which is a bug btw?) and then the user is
> asking himself why it isn't working

It is a bug, and the mechanism is exactly the revert. With an `a` left over
from a different lobe, selecting `OPTICAL_LOG_SINE` builds a warp table that
saturates, so the *mode* is rejected and the dropdown snaps back — reporting an
error about `a`, a control the operator was not touching. There is no
indication of which value is in the way or how far it would have to move, and
the two controls can each block the other, so the way out is guesswork.

## Decision

Nothing in the drive settings reverts. Two changes:

**1. One place that knows the constraint.** `waveform::PeakLaw` names how the
peak intensity follows from `ū` and `a`, one variant per calibrated mode:

| variant | peak | used by |
| --- | --- | --- |
| `Constant` | `ū` | `CONST` |
| `LogSwing` | `ū·e^{a/2}` | `DAC_SINE`, `SQUARE` |
| `LogSine` | `ū·e^{a/2}/I₀(a/2)` | `OPTICAL_LOG_SINE` |
| `LinearSine` | `ū·(1 + tanh(a/2))` | `OPTICAL_LINEAR_SINE` |

Solving each relation for one variable at a time gives `max_depth_for_mean` and
`max_mean_for_depth` — the achievable range. `LobeInversion::peak_intensity_ceiling`
turns the DAC max limit into the `u_max` they are solved against.

These are asserted to agree with `warp_table()` to within 1e-4: a range that
disagreed with what the drive builder accepts would either offer a refused
setting or hide a working one.

**2. Edits clamp, and only the edited control moves.** `reconcile_drive` takes
a `DriveKnob` naming what the operator just touched:

- `Depth` — dragging `a` up means "more depth", so `a` is what gets limited and
  `ū` stays put.
- `Mean` — and symmetrically.
- `Lobe` — a new calibration, a new ceiling or a new mode has no such
  preference, so brightness settles first and the depth that fits under it
  second.

Modes and drive methods are always accepted. The achievable range is on the
status line and in the two control labels, so the boundary is visible before
the drag reaches it rather than reported after.

An un-sendable drive is reported (`drive not sent: …`) instead of blocking the
edit, and the report is refreshed on every reconcile — a stale rejection from
an earlier combination no longer outlives the edit that fixed it.

## Consequences

- Every mode is selectable from every state. There is a test that walks all
  five modes from a deliberately unbuildable `(ū = 1.0, a = 6.0)` and asserts
  each one leaves a drive that builds.
- The labels carry the live bound: `Optical depth a (0..1.37 at ū=0.50)`.
- A clamp is a silent change to a value the operator asked for. That is right
  for a drag, and wrong for automation: `SetOperatingPoint`, `SetOpticalDepth`
  and `SetDriveFrequency` still refuse out-of-range requests rather than
  clamping, because a protocol asked for a specific point and quietly recording
  a different one would put the wrong parameters in every sidecar of a block.
- `validate_drive` is gone; `drive_command()` is consulted directly where its
  verdict is wanted.
