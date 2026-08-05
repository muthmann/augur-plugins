# ADR 034 — A run's time estimate is its plan corrected by the pace it is actually keeping

- **Status:** Accepted
- **Date:** 2026-08-05
- **Relates to:** ADR 010 (amplitude sweep), ADR 014 (frequency ladder),
  ADR 023 (nested depth × frequency sweep), ADR 027 (declarative protocols),
  ADR 029 (leases renewed against the granted deadline),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

Three A1 runs are long enough that the operator's next question after pressing
the button is *how long will this take*: the depth sweep, the frequency ladder
(either mode), and a protocol file. Only the protocol answered it, once, on the
button press — `about {minutes:.0} min of bench time` — from
`Protocol::total_seconds()`, the sum of each row's `duration_s + settle_s`.

That number is wrong in two different ways, and both matter at 23:00.

**It is too small.** The plan's seconds are the recording and the operator's
settle dwell. They are not what a point costs. Every point also pays:

- the camera start and stop, and the photodiode connect → lease → start →
  finalize handshake (the same handshakes ADR 029 had to widen the lease TTL
  for);
- the drive physically settling at a new depth, which the sweep waits for
  against the *measured* `a` with a 30 s cap;
- for a ladder rung, the trigger confirming the new frequency — four marker
  periods, so 40 s at 0.1 Hz — and, in `a₀` mode, a closed-loop lock of up to
  eight trials that appears in no setting at all.

**It goes stale.** It is printed once and then overwritten by the next
per-point message. A survey that is running at half the expected speed — a
drifting cell that settles slowly, a photodiode window that keeps being
rejected — says nothing until it is still going in the morning.

The obvious source for a better number is already in the file: the lease TTLs
(`sweep_lease_ttl_ms`, `freq_sweep_lease_ttl_ms`, `protocol_lease_ttl_ms`).
They must not be reused. A TTL is a deliberate **worst case** — it has to cover
the settle timeout of every remaining point or the owner reaps the lease
mid-run — and doubles its input on top. Quoting it would tell the operator that
a five-minute sweep needs half an hour, and an estimate nobody believes is worse
than none.

## Decision

**The estimate is the plan's own seconds, scaled by the pace the run has
actually kept, and it is shown for as long as the run lasts.**

`plugins/stage-a-a1/src/eta.rs` holds the whole mechanism — one `Eta` per run:

```rust
pub fn point_done(&mut self, now_ms: u64, planned_s: f64);   // at each point boundary
pub fn pace(&self) -> Option<f64>;                           // actual / planned, clamped
pub fn remaining_s(&self, now_ms: u64, planned_remaining_s: f64) -> f64;
```

- **Until the first point finishes** there is nothing to measure, so the plan
  stands alone plus a fixed `POINT_OVERHEAD_S = 5 s` allowance per point for the
  handshake. The line says so in words: *(from the plan until the first point
  finishes)*. It is a lower bound that improves, not a promise.
- **From then on**, every finished point re-scales what is left. The pace is
  clamped to `[0.5, 6.0]`, so one point that spent its timeouts cannot
  extrapolate a night onto the rest, and one that was refused before it began
  cannot predict the rest away.
- **Skipped points count.** What is being measured is how long this run takes to
  get through its list; a point that fails still spends its time.
- **The estimate counts down inside the point in flight**, so it moves between
  boundaries instead of standing still for a 40 s row.

What each run's plan *is* stays with the run, because only it knows its shape:
a protocol reads each row's own `duration_s + settle_s`; a depth sweep multiplies
the panel's duration and dwell by its point count; a ladder rung adds the
frequency retarget, the confirmation (`4 / f`, capped at the 20 s give-up), the
`a₀` lock where one runs at all — three nominal trials, not the cap of eight —
and then whatever the rung records.

**One estimate, for the outermost run.** A ladder's `Estimated time` line
already contains the inner depth sweep it handed the current rung to, and a
protocol contains both. Three lines would state one fact three times, with the
two inner ones — which end long before the run does — reading as contradictions
of the only one that matters.

**The finish clock is UTC**, and only appears once more than ten minutes are
left. UTC because every filename, sidecar and timestamp this plugin writes is
UTC, and two clocks in one panel is a bug waiting for a night shift.

## Consequences

- The panel gains one line, e.g.
  `Estimated time: ≈ 41 min left of ≈ 1 h 5 min — done by 03:41 UTC`, for a
  depth sweep, a frequency ladder, an `a₀` point or a protocol.
- The button press states the same number the panel then counts down, so the
  figure on the press and the figure a minute later are not two different
  answers. The protocol's `about N min` wording is gone.
- The nominal constants (`POINT_OVERHEAD_S`, `A0_LOCK_NOMINAL_TRIALS = 3`,
  `A0_LOCK_NOMINAL_TRIAL_S`, `FREQ_RETARGET_NOMINAL_S`) are bench guesses and
  are only ever the *first* estimate of a run. They are deliberately not tuned
  against any one bench: the measured pace replaces them within one point.
- Nothing here steers a run. No point is skipped, shortened or refused because
  of an estimate; it exists so the operator can decide whether to wait.
- `Protocol::total_seconds()` is now `remaining_seconds(0)`, and
  `protocol_lease_ttl_ms` reuses the same sum instead of restating it.
