# ADR 018 — A1 gates on what it needs, not on what it would like to know

- **Status:** Accepted
- **Date:** 2026-07-30
- **Relates to:** ADR 010 (amplitude sweep), ADR 013 (event-count depth lock),
  ADR 014 (frequency ladder), ADR 015 (recording robustness), ADR 017 (rail
  detection and withheld-`a` reasons),
  [Stage-A A1 Analysis](../features/stage-a-a1.md),
  [Stage-A A1 Event-Count Depth](../features/stage-a-a1-event-count.md)

## Context

Every A1 recording path was unusable on the bench, and the panel described the
symptom rather than the cause.

### Provenance metadata was enforced as a precondition

`begin_recording` refused unless three fields were non-empty: the output folder,
the measurement id, and the physical `I_k` flux point id. Only the first is
something the plugin actually needs — it is where the files go. The measurement
id names a folder and a file stem, and the plugin has always shipped a generated
default for it. The flux point id is pure provenance: it records *which*
illumination calibration point a row belongs to, and nothing in the recording,
the sweep or the lock reads it.

Enforcing them anyway meant a blank field could not be distinguished from a
misconfigured bench, and the refusals were spread unevenly across the entry
points:

| entry point | folder | measurement id | flux point id |
| --- | --- | --- | --- |
| `begin_recording` | refused | refused | **refused** |
| `begin_sweep` | refused | refused | **refused** |
| `begin_leased_sweep` | refused | refused | — |
| `begin_freq_sweep` | refused | refused | **not checked** |

The last row is the defect. The frequency ladder validated its whole plan up
front — deliberately, so that "a plan that cannot work should say so in a
message, not two hours into a block" — but did not ask the question its own
recordings would ask. It therefore took the modulation lease, retargeted the
drive, confirmed the frequency against the trigger, and ran a closed-loop `a₀`
lock, and only then handed off to `begin_recording`, which refused on the blank
flux point id. The recording coordinator stayed idle, the sweep saw
`point_started == false`, and the point was skipped — for every point. The panel
read `Frequency sweep 1/7 … — recording` next to `Recording: idle`, which is an
accurate description of two components and an explanation of neither.

The same held for the amplitude sweep and, transitively, for the single
event-count point.

### The test suite could not see any of it

Every fixture in `runtime.rs` was built from `plugin_with_markers()`, which sets
`flux_point_id: "flux-test"`, or set the field explicitly. Fifty-one tests
passed, including one that drove the entire frequency ladder to completion,
because none of them ever exercised the state an operator actually starts in: a
fresh panel with nothing typed in.

### A converged lock disarmed itself

`armed_lock()` required `(lock.target_a - a0_target).abs() <= 1e-6`. `a0_target`
is an `F64Drag` with a 0.01 step that round-trips through JSON on every settings
sync. One stray pixel of drag after a successful `Find a₀` silently disarmed the
lock, and `begin_a0_point` then reported

> No converged a₀ lock for 10.000 Hz — press Find a₀ at this frequency first

which is the one instruction that does not help, addressed to an operator who
had just done it. That sentence also covered two other causes — no lock at this
frequency at all, and a lock that ran out of trials — without distinguishing
them.

### The panel was written for the person who wrote it

Section descriptions ran to full paragraphs of bench physics
(`q_p`, `S_p(t)`, `I_exc = I_tot − I_pd`, "marker-bounded window",
"refractory condition 2·f·a₀/C ≪ 1/τ_refr"). The prose was accurate and
unreadable, and it competed for attention with the one line that mattered — the
status message saying why the button had just refused.

## Decision

### 1. A gate exists only for an input the action cannot proceed without

The output folder stays required: there is no defensible default destination for
measurement data, and the buttons are already disabled without one, which is
discoverable before the click rather than after.

The measurement id is filled in on use. `ensure_measurement_id()` generates one
when the field is blank and **writes it back to the field**, so the run is filed
under a name the operator can see. It is tested on the raw field, not on
`sanitize_stem`'s output — the sanitizer substitutes `A1` for anything that
reduces to nothing, so asking it whether the id was blank always answers no, and
every unnamed run would have quietly shared one folder called `A1`.

The flux point id is recorded, never enforced. A blank one is written as the
explicit sentinel `unspecified`, which keeps "not stated" distinguishable from a
real id downstream. A missing provenance field makes a recording *less
traceable*; it never makes it *wrong*, and that is not a reason to withhold the
operator's data.

### 2. Every gate a run will eventually hit is asked before the drive moves

`begin_sweep` and `begin_freq_sweep` now call `photodiode_blocker()` up front,
alongside the checks they already made. The principle ADR 015 established for
the recording coordinator — check before the camera starts, not after — extends
to the supervisors: a ladder must not take a lease and move the drive to
discover, at point 1, something it could have known at point 0.

`begin_sweep` also drops its own hand-written photodiode sentence in favour of
`measured_a_blocker()` (ADR 017 §2). It was the last gate on `a` still naming
the anchor and the cable whatever the real cause was.

### 3. A lock arms within the operator's own tolerance

`armed_lock()` compares `lock.target_a` to `a0_target` against
`a0_tolerance`, not against `1e-6`. The tolerance is already the operator's
statement of how close to `a₀` counts as `a₀`; applying a stricter rule to the
*same* quantity one line later was never coherent.

`armed_lock_blocker()` returns the actual cause — no lock at this frequency, a
lock that stopped short (and where), or a lock aimed at a different `a₀` (naming
both values) — and both `begin_a0_point` and the resting status line render it.
This is ADR 017 §2's shape applied to the second gate.

### 4. The panel speaks to the operator

Operator-visible strings — section descriptions, tooltips, status entries,
refusal messages, and the `EstimateError` renderings A1 quotes across the plugin
boundary — state what to do, in the words of someone standing at the bench.
Quantities keep their symbols (`a`, `a₀`, `f`) because those are on the
whiteboard too; the machinery behind them (fold windows, marker-bounded
estimation, reject-port complement algebra) belongs in these documents, which is
where a reader who wants it will look.

Refusals name an action. `"ADC clipping: 307‰ low / 0‰ high"` became
`"the signal is hitting the ends of the detector's range (307‰ at the bottom,
0‰ at the top) — lower the drive amplitude or the detector gain"`.

### 5. A missing frequency is not a disconnected plugin, and is stated once

A frequency reaches A1 from two independent places: the phase-0 trigger markers,
or the drive the modulation plugin has *acknowledged*. Neither is the connection
state, which `modulation_connected()` checks separately. The resting line
nonetheless read

> Frequency: unknown — connect the modulation plugin, or the trigger cable

on a bench whose modulation plugin was connected. The usual cause is simply that
no periodic drive has been applied yet, and telling an operator to plug in
hardware that is already plugged in is worse than saying nothing.

`frequency_blocker()` distinguishes the cases in the order the data flows — no
owner snapshot, not connected, connected with nothing applied, a waveform that is
not periodic, a periodic waveform at 0 Hz — and `begin_a0_lock`,
`armed_lock_blocker()` and the status line all render it. This is the third
application of the `measured_a_blocker()` shape from ADR 017 §2; the pattern is
now the house style for any gate an operator can see.

Each fact also appears on exactly one line. A missing frequency previously
occupied three — the transient message, the `Frequency:` line, and the a₀
readiness line, each with its own phrasing of the same cause — which reads as
three problems. The a₀ line now defers to the frequency line rather than
restating it, and the response-curve line is omitted entirely when it has neither
points nor windows to report, instead of stating the absence of the two facts
above it.

## Consequences

- The three recording workflows run with an output folder and nothing else
  typed in. Regression tests cover exactly that state, and the frequency-ladder
  test now exists in both variants — ids set and ids blank.
- Sidecars from unnamed rows carry `flux_point_id = "unspecified"`. Offline
  analysis that joins on the flux point must treat that value as absent; it is a
  sentinel, not an id. Analysis written against the old contract never saw a
  blank field, because a blank field never produced a recording.
- Generated measurement ids are timestamp-derived (`A1-<date>-<ms>`), so two
  unnamed runs started in the same millisecond would collide. They cannot be:
  the id is generated inside `begin_recording`, which refuses re-entry while a
  recording is active.
- A lock now survives an `a₀` nudge inside the tolerance. Widening
  `a0_tolerance` therefore also widens what counts as "the same target", which
  is the intended reading — but an operator who widens it to 0.5 to force a
  stubborn lock through will find older locks arming for targets they did not
  mean. The status line always names the lock's own target.
- The panel no longer states the estimator's gates in the estimator's terms.
  Someone debugging the fold or the contrast geometry reads ADR 011, 012 and 017
  rather than a tooltip.
- The resting panel is shorter, and lines disappear when they have nothing to
  say. An operator scanning for "did it change?" now has fewer stable lines to
  scan, but cannot rely on a fixed line count or line order — anything parsing
  `status_entries()` positionally would break. Nothing does; the host renders
  them as a list.
- `frequency_blocker()` reports on the *acknowledged* drive, so a frequency the
  operator has typed into the modulation plugin but not applied still reads as
  "not applied yet". That is the intended reading — A1 measures against what the
  bench is doing, never against what a field says.
