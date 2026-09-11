# ADR 026 — The applied Pockels lobe crosses the mirror/worker boundary

- **Status:** Accepted
- **Date:** 2026-08-01
- **Relates to:** ADR 010 (button presses cross mirror → worker), ADR 011
  (Pockels transfer calibration), ADR 016 (lobe endpoints),
  [Stage-A Pockels Calibration](../features/stage-a-pockels-calibration.md)

## Context

"Apply to V_null / V_peak" did nothing. Pressing it after a good sweep left the
two settings showing their old values and the drive on the old lobe.

The host runs **two instances** of every plugin: a UI mirror that renders the
settings panel, and a live worker that owns the device link. Settings travel in
one direction only. Every live-analysis pass calls
`collect_live_plugin_state_snapshot`, which reads `get_setting` from the
mirror, and `apply_live_plugin_snapshot`, which writes each value onto the
worker.

The measured fit lives on the worker — it is the instance with the photodiode
and the DAC. So the button failed twice over:

1. The mirror ran `apply_calibration_fit` with `self.fit == None` and reported
   "nothing to apply".
2. The worker applied the fit correctly, and the next settings sync overwrote
   `v_null_dac` / `v_peak_dac` with the mirror's stale pair — within one frame.

ADR 010 solved the *press* crossing this boundary (a monotonic counter through
`get_setting`). This is the opposite direction: a **result** produced on the
worker has to reach the mirror, and no channel carried one.

## Decision

Both instances live in the same process — the live worker is a thread, and the
plugin is one loaded `cdylib`. The applied lobe is published through a
process-global slot with a monotonic generation:

```rust
static APPLIED_LOBE: Mutex<Option<AppliedLobe>> = Mutex::new(None);
static APPLIED_LOBE_GENERATION: AtomicU64 = AtomicU64::new(0);
```

Scoped by runtime role, which is what makes it a channel rather than shared
mutable state:

- **only the live worker publishes** — it is the only instance that can have a
  fit;
- **only the UI mirror adopts** — so no instance ever reads back its own
  publication.

The mirror consults it in two places. `get_setting` and `settings_schema` read
through `effective_lobe()`, so a freshly applied lobe reaches the panel *and*
the outgoing snapshot on the next repaint. `set_setting` calls
`adopt_applied_lobe()` first, so an incoming echo of the old codes cannot land
on top of a newer applied one.

The generation makes adoption one-way and terminal: once the mirror is at
generation *n* it accepts ordinary edits again, so applying a calibration does
not freeze the two controls.

A fresh instance starts at generation 0, not at the current value. A mirror
built after a calibration — a plugin reload — has to pick the applied lobe up,
not assume it is already current.

## Consequences

- The button works, and there is a regression test: after a sweep and an apply
  on a worker, a newly constructed mirror's `get_setting("v_null_dac")` returns
  the applied code, and a subsequent edit on the mirror sticks.
- Applying no longer refuses on the *drive*. A lobe that cannot express the
  currently armed `a`/`ū` is still a valid measurement of the bench; the two
  controls clamp to the new lobe instead (ADR 025). Applying still refuses two
  codes that name no monotonic lobe at all.
- One global means one bench per process, which is what the hardware is. It
  does mean unit tests that fabricate several plugins share it — the role
  scoping keeps that harmless, since test plugins are live workers and never
  adopt.
- This is a general shape, not a one-off. Any worker-produced value that has to
  survive the settings snapshot needs the same treatment; the alternative —
  making the host merge worker state back into the mirror — is an `augur-rs`
  API change and is not warranted by one field.
