# ADR 021 — There is nothing to search for in a depth you are commanding

- **Status:** Accepted
- **Date:** 2026-07-31
- **Relates to:** ADR 013 (event-count depth lock — the search this scopes),
  ADR 014 (frequency ladder), ADR 020 (depth source),
  [Stage-A A1 Exact Event Count](../features/stage-a-a1-event-count.md),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

ADR 013 built a closed-loop search, `Find a₀`, for one reason. The Pockels
transfer curve is measured once, so the inversion A1 commands through is
**static**, while the depth the cell actually delivers **rolls off with
frequency**. Holding one *measured* `a₀` across a frequency ladder therefore
means re-finding, per frequency, the commanded depth that produces it:

```
a_cmd ← a_cmd · a₀ / a_measured        (≤ 8 trials, 3 readings each)
```

The result is stored per frequency in `a0_locks.json`, and every downstream
action — `Record a₀ point`, each rung of the ladder — replays a stored,
converged row. That is real work and it is the scientific core of the
exact-event-count workflow.

ADR 020 then added a second depth source: when the photodiode cannot publish an
`a` at all (no phase-0 marker stream), A1 can take `a` from the modulation
owner's *commanded* calibrated drive. That made the workflow reachable again —
and immediately made the search meaningless, because in that mode the quantity
the loop measures *is* the quantity it commands:

| step | measured source | commanded source |
| --- | --- | --- |
| command `a₀` | drive moves | drive moves |
| read back | photodiode reports what the light did | the owner reports the number just sent |
| correct | `a_cmd · a₀/a_measured`, repeat | ratio is exactly 1 — no-op |
| result | a per-frequency commanded depth | `commanded_a = a₀`, at every frequency |

Eight ladder points produced eight identical rows carrying no information, each
behind a lease acquisition, a settle dwell and three "readings". The operator
was required to press `Find a₀` before `Record a₀ point` would arm, for a search
whose answer was already on screen.

It was also actively harmful. `begin_a0_lock` warm-starts from any stored row at
the current frequency, so a row left over from a *measured* session (say
`commanded_a = 0.83` for `a₀ = 0.5`) would seed an open-loop run with a
closed-loop number, fail to converge on trial 1, and spend a second trial
correcting itself back to `0.5`. A no-op that can still be wrong is worse than
no operation at all.

## Decision

Whether the search is needed is a property of the depth source, expressed once
as `DepthSource::needs_a0_lock()`, and three things follow from it.

**1. The lock table stops being the way to ask "what depth is armed here?"**
That question moves to `armed_a0()`, which returns a lock row from the table
under a measured source and *synthesises* one under a commanded source
(`commanded_a = target_a = a₀` at the current frequency, `trials: 0` recording
honestly that no search happened). `begin_a0_point` and the ladder both ask
this, and neither knows which regime it is in. Nothing is written to
`a0_locks.json` open loop, because nothing was found.

**2. The ladder skips the `Locking` phase entirely.** Per rung it becomes
lease → set frequency → confirm → record. The `FreqSweepPhase::Locking` arm and
the direct path share one `start_freq_sweep_recording`, so there is a single
place where a point becomes a recording.

**3. `Find a₀` refuses instead of pretending.** It states that `a₀` is commanded
directly and points at `Record a₀ point` / `Record all frequencies`. The button
is disabled rather than hidden — it is what the whole a₀ workflow is documented
around, so it has to stay visible and explain itself.

### Frequency confirmation follows the same logic

The ladder confirmed each commanded frequency against the **camera's** phase-0
markers, on the correct principle that an ACK says the table was accepted, not
that the light is modulating at that rate. But a bench with no marker stream —
the exact bench the commanded source exists for — can never satisfy it, so
removing the search alone would have left the ladder refusing at the next gate.

Under a commanded source the ladder therefore confirms against the modulation
owner's **acknowledged waveform**. This is not a new trust relationship: it is
the same owner, and the same acknowledged state, that mode already trusts to
state `a`. The cost is explicit — the live `q_p` fold goes free-running without
markers — and it is confined to the live quicklook. The recorded RAW and PDQ,
which are what the offline fit actually reads, are unaffected.

Under a measured source nothing changes: markers confirm the frequency, and
`begin_freq_sweep` still refuses up front when they are absent.

## Consequences

- The commanded ladder runs on a bench with **no photodiode `a` and no camera
  trigger**, with `Live analysis` off, and records every planned point.
- Open loop, the panel stops reporting a saved-depth count that is structurally
  always zero, and says what will happen instead: *"the drive is commanded to
  a = 0.500 at 1.000 kHz — no search needed"*.
- The a₀ section's description text switches with the source, and states the
  trade in the operator's own terms: nothing verifies the light reached `a₀`,
  and the static inversion does deliver less depth as `f` rises.
- **The measured workflow is untouched.** `Find a₀`, the per-frequency trim, the
  lock table and its disk mirror all behave exactly as ADR 013 specifies the
  moment the depth source is the photodiode again.

## Alternatives considered

**Delete the lock outright.** Simplest possible plugin, and wrong: it would
permanently give up ADR 013's guarantee that the same *measured* depth was held
across frequencies. The roll-off it corrects is real; only its applicability to
an open-loop depth is not.

**Keep the search but make it one trial.** Still a lease, a dwell and a table
row per frequency, to reproduce a number the operator typed. The ceremony was
the complaint, not its duration.

**Write the synthesised rows to `a0_locks.json` anyway**, for uniformity. They
would be eight identical restatements of the `a₀` setting, and a reader of that
file could no longer tell a found depth from an assumed one. Provenance already
lives in the sidecar (`depth_a_source`, `[a0_lock].depth_source`).
