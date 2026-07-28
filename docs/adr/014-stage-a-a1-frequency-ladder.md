# ADR 014 — Stage-A A1 unattended frequency ladder

- **Status:** accepted (2026-07-27)
- **Relates to:** ADR 009 (recording coordinator), ADR 010 (amplitude sweep via
  leased `SetOpticalDepth`), ADR 012 (the contrast geometry the measured `a` is
  defined in), ADR 013 (the per-frequency `a₀` lock),
  [Stage-A A1 Exact Event Count](../features/stage-a-a1-event-count.md)

## Context

ADR 013 gave the operator two buttons per frequency — *Find a₀* and *Record a₀
point* — and deliberately left the ladder manual, because "the protocol's
ordering and randomisation decisions are scientific, not mechanical".

In practice an A1 event-count block is 7–12 frequencies over two or three
decades, each one a lock plus a recording, repeated over three independent
blocks. That is an hour of pressing two buttons in the right order while
watching a status line — and every gap between the two presses is a gap in which
a modulation settings sync can re-apply the operator's own `depth a` on top of
the found one (ADR 013 §3 exists precisely because of this hazard, and only
closes it *within* one point).

The ordering decisions are scientific, but they are also **expressible**: a
seeded schedule and an interleaved reference cadence are exactly what the A1
checklist asks to be frozen in the session plan before the block starts. Freezing
them as settings and recording them per point is stronger than leaving them to
be executed by hand and written down afterwards.

Three things blocked automation:

1. **A1 could not change the frequency.** The contract exposed `SetOpticalDepth`
   but no frequency equivalent, and A1's scoped reach (ADR 007/010) was "the
   armed drive's depth while leased".
2. **Nothing could confirm a frequency had arrived.** The firmware ACKs a table
   it accepted; the light modulating at that rate is a different claim.
3. **The measured `a` was not trustworthy at the bottom of a ladder.** The
   photodiode estimated the peak-to-peak contrast over a fixed 0.82 s window,
   under one cycle for every `f < 1.2 Hz` — and the `a₀` lock divides by that
   value, so a truncated estimate drives the depth up until it rails.

## Decision

**1. `ModulationCommandV1::SetDriveFrequency { frequency_millihz }`** (contract
addition, additive to V1) — the frequency counterpart of `SetOpticalDepth`, with
the same scoping: leased only, re-derived through the same `drive_command()`
builder, rejected when the link is closed or the armed drive has no frequency to
retarget. The owner **parks the operator's armed frequency** on the first
retarget and restores it in `end_lease`, exactly as it already does for the
depth, so a finished ladder does not leave the bench on its last point.

**2. The ladder is a supervisor, not a third state machine.** `FreqSweep` runs
`AcquiringLease → (per point) SettingFrequency → ConfirmingFrequency → Locking →
Recording → …release`, where *Locking* and *Recording* are the **unchanged**
ADR 013 lock and ADR 010/013 point. Both gained an inherited-lease mode
(`owns_lease: false`): when the ladder starts them they neither acquire nor
release, they run on its lease.

That is the substantive guarantee: **one lease spans the whole ladder**, so the
operator's drive settings are locked out from the first frequency to the last,
and the "the amplitude cannot change during the recorded interval" property
ADR 013 established for one point now holds across the gap between a lock and
the point that replays it.

**3. The trigger confirms the frequency.** A point does not start until enough
phase-0 markers *at the new period* agree with the commanded frequency. On every
frequency change the retained markers and events are dropped: the measured
period is their mean spacing, so keeping them would confirm the new frequency
against a mixture of the old drive and the new one.

**Pilot windows are dropped with them.** Windows frozen at one period are a
phase interval of *that* period; carrying them into another frequency would
score the point in the wrong window — silently, because a fold always produces
something. Re-freezing a pilot per frequency stays the operator's call; the
ladder only guarantees it never reuses a stale one.

**4. The schedule is data.** Log spacing (a Bode ladder is read per decade),
four orders — ascending, descending, alternating, seeded random — and an
optional low-frequency reference interleaved every N points. The executed
position, the order and the seed go into every point's sidecar
(`[frequency_sweep]`), so a block is interpretable from its files rather than
from a notebook.

**5. A bad point is skipped, not fatal.** An unreachable `a₀`, an unconfirmed
frequency, or a failed recording skips that frequency and names it in the final
summary; the lock table keeps the failed attempt. The remaining decades are
worth more than a clean abort. Only losing the lease ends the ladder.

**6. The plan is validated before the drive moves.** The photodiode estimates
`a` over one window for the whole ladder, so its *lowest* frequency decides
whether the ladder is measurable. That, the drivability of `a₀`, the presence of
a trigger, and the destination are all checked at the button press.

## Consequences

- A1's scoped hardware reach widens by one parameter: it may retarget the armed
  drive's **frequency** as well as its depth, still only while leased, still
  through the owner's own builder and validation. Everything else about the
  drive remains the operator's.
- The lease is now held for the length of a whole block rather than a point, so
  its TTL is sized from the ladder (renewed per point). A lost lease ends the
  run — which is the correct failure: without it the drive is no longer
  provably A1's.
- Pilot-frozen windows no longer survive a frequency change. A workflow that
  relied on freezing one pilot and recording several frequencies against it was
  producing wrongly-scored `q_p`; it now falls back to per-fold auto-windows and
  says so.
- `a₀` is still not frozen numerically here, the refractory bound is still not
  checked, and references are still the operator's. The ladder automates the
  mechanical repetition, not the scientific choices.
