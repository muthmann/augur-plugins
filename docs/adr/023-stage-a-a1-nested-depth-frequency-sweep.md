# ADR 023 — The frequency ladder is an outer loop, not one experiment

- **Status:** Accepted
- **Date:** 2026-07-31
- **Relates to:** ADR 010 (amplitude sweep — the inner run), ADR 013 (`a₀`
  lock), ADR 014 (frequency ladder), ADR 021 (no search for a commanded depth),
  [Stage-A A1 Analysis](../features/stage-a-a1.md),
  [Stage-A A1 Exact Event Count](../features/stage-a-a1-event-count.md)

## Context

A1 had two multi-recording runs, and they were built as if they were unrelated:

- **The amplitude sweep** (ADR 010) — leases the drive, walks `[min_a, max_a]`,
  records one measurement per depth. One `q_p(a)` curve, at whatever frequency
  the operator happened to have armed.
- **The frequency ladder** (ADR 014) — leases the drive, walks a log-spaced set
  of frequencies, and records *one* point at each.

The experiment the bench actually exists to produce is `q_p(a, f)` — a response
curve per frequency, from which `a50(f)` is fitted offline. Getting it meant
driving the amplitude sweep by hand once per frequency: set `f` in the
modulation plugin, press *Record depth sweep*, wait, come back, repeat. Seven
frequencies of that is seven manual interventions, seven opportunities for the
drive to be left somewhere unintended between blocks, and — because each sweep
takes and releases its own lease — seven windows in which the operator's own
settings are re-applied on top of the run.

Meanwhile the ladder already had every part of that missing outer loop: log
spacing, visit order (ascending / descending / alternating / seeded random),
interleaved low-frequency reference repeats, per-frequency confirmation, one
lease held across the whole block, skip-and-report on a frequency it cannot
reach, and a summary. All of it was hard-wired to record exactly one thing at
each rung.

## Decision

The ladder becomes an outer loop with a **mode** naming what each rung records:

```rust
enum FreqSweepMode {
    A0Point,     // one event-count point at the frozen depth a₀ (ADR 013/014)
    DepthSweep,  // the whole [min_a, max_a] sweep — the q_p(a, f) surface
}
```

`DepthSweep` reaches the inner run through `begin_leased_sweep` with
`SweepKind::Amplitude` and the ladder's own `lease_id` — the *unchanged*
amplitude sweep, inheriting the lease rather than taking one. So the operator's
drive settings stay locked out from the first frequency to the last, not merely
between the points of one curve, and are handed back once at the end.

Everything else about the ladder is shared as it stands: ordering, the reference
repeats, the frequency confirmation of ADR 021, skip-and-report, the summary.
Adding the second experiment added one enum, one dispatch and one button.

### A depth sweep never needs a search

`FreqSweepMode::needs_armed_depth()` is false for `DepthSweep`, so the
`Locking` phase is skipped **in both depth sources** — not only the commanded
one of ADR 021. The reasoning is different from ADR 021's and worth stating: an
`a₀` rung replays a single depth that something must have chosen, whereas a
depth sweep commands every `a` in its range itself and settles on each against
the measured value. There is nothing for a lock to contribute at any frequency.
A measured-source nested sweep is therefore still fully closed-loop — each point
waits for the photodiode to reach its own target — it simply has no `a₀`.

### Two things that had to change underneath

**The ladder needed the inner run's verdict, not the last recording's.** It
advanced on `recording_completed_ok`, which describes one recording. A depth
sweep that gives up on point 4 of 5 leaves that flag `true` from point 3, and
the rung would have counted as finished with a half-recorded curve. `Sweep` now
carries `completed_ok`, set only on the branch that runs out of points with
every one recorded, and `finish_sweep` publishes it as
`last_sweep_completed_ok`. The `a₀` path reads the same flag — it is also a
`Sweep` — so this replaced the weaker check rather than adding a second one.

**File names had to carry both axes.** Amplitude-sweep points are tagged `_pNN`,
which repeats at every rung and would collide inside one measurement id. A
nested point is now `…_f<f>Hz_pNN`, so the surface sorts by frequency and then
by depth.

**The lease TTL is sized per mode.** A depth-sweep rung costs a whole inner
sweep; a TTL computed for one `a₀` point would expire mid-curve and hand the
drive back to the operator's settings while the block was still running.

## Consequences

- One button — *Record depth sweep at every frequency* — produces the whole
  `q_p(a, f)` block: `frequency points × depth points` recordings on a single
  lease, unattended.
- It introduces **no new settings**. The depth axis is the existing Recording
  section (`min_a`, `max_a`, `sweep_count`, settle, duration); the frequency
  axis is the existing ladder (`min_f`, `max_f`, `freq_count`, order, seed,
  reference repeats). Its own section says so explicitly, because those two
  groups live under headings named after other experiments.
- The run is *large* by construction. The button's tooltip states the
  multiplication rather than discovering it at runtime, and the start message
  reports `N × M recordings`.
- `a0_locks.json`, `Find a₀` and the `a₀` ladder are untouched.

## Alternatives considered

**A separate second ladder.** A copy of the outer loop specialised to depth
sweeps. Rejected: ordering, reference repeats, confirmation, skip-and-report and
the lease discipline would then exist twice and drift apart — ADR 014's
machinery is the valuable part, and it is entirely mode-agnostic.

**Make the inner run a list of `(f, a)` pairs in one flat sweep.** Simpler
state, but it loses the ladder's per-frequency semantics: the reference repeats
are defined per frequency, the frequency confirmation happens per frequency, and
a failure has to skip a *frequency* rather than a point. Flattening would have
made the skip granularity wrong.

**Reuse `Record all frequencies` with a mode setting instead of a second
button.** Rejected: a control whose meaning depends on a nearby dropdown is how
an operator records the wrong experiment overnight. Two buttons, two names.
