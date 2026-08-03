# ADR 020 — A1 chooses where `a` comes from, and records the choice

- **Status:** Accepted
- **Date:** 2026-07-31
- **Relates to:** ADR 010 (amplitude sweep), ADR 011 (Pockels transfer
  calibration), ADR 012 (contrast geometry is bench, not display), ADR 013
  (event-count depth lock), ADR 014 (frequency ladder), ADR 017 (rail detection
  and withheld-`a` reasons), ADR 018 (A1 gates on what it needs),
  [Stage-A A1 Analysis](../features/stage-a-a1.md),
  [Stage-A A1 Event-Count Depth](../features/stage-a-a1-event-count.md),
  [Stage-A Pockels Calibration](../features/stage-a-pockels-calibration.md)

## Context

`a = ln(I_exc,max / I_exc,min)` is a property of the excitation *light*. ADR 011
and the estimator's module docs are emphatic about it: the Pockels V→T response
is non-linear, so the commanded DAC excursion is not a modulation depth and the
photodiode trace is the only valid source of `a`. Every A1 path that needs a
depth — the amplitude sweep, the `a₀` lock, the frequency ladder, the live
response curve — therefore read exactly one number: the photodiode owner's
`measured_log_contrast`.

That number is fail-closed by design. The photodiode refuses to publish `a`
unless it can prove the window it estimated over covers whole modulation cycles,
which it does by bounding the window between firmware **phase-0 marker frames**
on its own stream port. With fewer than three retained markers it returns
`IncompleteModulationCycles` and publishes no `a` at all.

On the bench this turned out to be reachable with the markers simply *absent*:

```
Measured depth a: not available. No stretch of samples covers two whole
modulation cycles between triggers (0 trigger(s) in the last 3446784 samples)
— lower the frequency, or raise the photodiode cache length
```

Three and a half million samples and zero markers is not a window that is too
short. It is a marker stream that is not arriving — no `MARKER` frames on the
stream port at all. The advice the refusal gives (lower the frequency, raise the
cache) cannot fix that, and no combination of settings can: without markers the
photodiode can never publish an `a`, so `Find a₀` refuses, the amplitude sweep
refuses, and the frequency ladder refuses. The entire A1 workflow is unreachable
on a bench whose Pockels cell is calibrated and whose drive is running
correctly.

The workflow does not actually need a *measured* `a` to run. It needs **a
depth it can name, aim at, and record**. The modulation owner already has one:
it inverts the measured `V_null` / `Vπ` transfer curve to command a depth, and
publishes it as `OpticalDriveStateV1::depth_a_milli`. That is a calibrated
number — it comes from the same measurement ADR 011 exists to make — it is
simply not verified against the light afterwards.

## Decision

A1 gets one operator setting, **`depth_source`**, naming where its depth `a`
comes from:

- `DepthSource::Photodiode` (**default**) — the photodiode's measured
  excitation log-contrast. Unchanged behaviour, and the source of record.
- `DepthSource::Commanded` — the depth the modulation owner's calibrated
  optical drive is commanding, read back from its published
  `optical_drive.depth_a_milli`.

One accessor, `depth_a()`, resolves the setting, and *every* consumer reads it:
the sweep's settle check, the `a₀` lock's readings, all three refusal gates, the
status panel, the live response curve, and the sidecar. The source is chosen in
exactly one place, so no path can be left reading the wrong one.

`DepthSource::Commanded` is admissible only under the same conditions that make
a commanded depth mean anything at all. `optical_drive` is published solely for
`OPTICAL_LOG_SINE` / `OPTICAL_LINEAR_SINE` under an identified transfer
calibration, so a manual DAC band or a constant level yields no depth and the
gates refuse with that reason. A DAC number is never dressed up as an `a`.

### Consequences for the closed loop

The `a₀` lock is a feedback loop: command a depth, measure what the light did,
correct. Open loop the measurement *is* the command, so the loop converges on
trial 1 and the correction is a no-op. This is the honest degenerate case, not a
bug — there is nothing on the bench that could contradict the command — and it
is what makes the downstream machinery (the armed lock row, the event-count
point, the ladder) work unchanged. Two rules that exist for the estimator are
therefore scoped to the photodiode source:

- the **stale-window rule** (only count summaries published after the depth was
  commanded) — a commanded depth is not read out of a window, so enforcing it
  would only couple the trial to the modulation owner's device-poll cadence;
- the **window-covers-a-cycle** and **clipping** checks — both are statements
  about a detector window, and neither bounds a commanded depth.

The operator's settle dwell still applies in both modes, so the drive gets its
physical time to move either way.

### Provenance is not optional

Everything that records an `a` records which source produced it:

| artefact | field |
| --- | --- |
| A1 config sidecar (`.toml`) | `depth_a_source`, `depth_a` |
| A1 sidecar, `[a0_lock]` | `depth_source` |
| camera / PDQ recorder metadata | `depth_a_source`, `depth_a`, `a0_lock_depth_source` |
| `a0_locks.json` | `depth_source` (defaults to photodiode on older tables) |
| a₀ lock host view | `a from` column |

`measured_a` keeps its historical meaning — a number the photodiode actually
measured — so an open-loop run simply carries no `measured_a`, rather than
carrying a commanded value under that name. A `q_p(a, f)` fit that pools the two
sources without looking at `depth_a_source` would be pooling two different error
budgets; the field is there so that cannot happen silently.

The panel follows the same rule in prose: it says *"Commanded depth a (open
loop, not measured)"*, and an open-loop lock reports that the drive *"is
commanded as"* a value rather than that it *"measures"* one.

## Alternatives considered

**Fall back automatically when the photodiode withholds `a`.** Rejected. The
difference between a measured and a commanded depth is the difference between
two error budgets, and a silent switch would put both into one dataset with no
way to separate them afterwards. It also hides a real bench fault (a missing
trigger cable) behind a workflow that keeps running.

**Loosen the photodiode's marker requirement instead** — estimate over a fixed
window when no markers exist. Rejected: a sub-cycle window *under*-reports `a`,
and the `a₀` lock divides by it, so it would drive the depth up until it rails.
ADR 017's fail-closed refusal is right; what was missing was a way past it that
does not lie about what was measured.

**Enter `a` by hand.** Rejected — that is the datasheet number ADR 011 exists to
eliminate. The commanded depth comes from a measurement of *this* cell.

## Status on the bench

The commanded source is a way to keep working while the phase-0 marker stream is
diagnosed, not a replacement for measuring the light. A run recorded this way
carries the Pockels calibration's error plus any drift since it was taken, and
nothing checks it. Runs that go into the final `q_p(a, f)` fit should be
photodiode-measured.
