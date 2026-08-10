# ADR 029 — A leased run renews against the deadline the owner granted, not the one it asked for

- **Status:** Accepted
- **Date:** 2026-08-03
- **Relates to:** ADR 005 (device ownership), ADR 007 (owner orchestration),
  ADR 009 (recording coordinator), ADR 027 (declarative protocols),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

Both Stage-A device owners hand out an automation lease with a TTL, and both
**cap** the TTL they grant:

```rust
// modulation and photodiode, independently
const MAX_LEASE_TTL_MS: u64 = 60_000;
fn lease_deadline(ttl_ms: u64) -> u64 {
    now_unix_ms().saturating_add(ttl_ms.clamp(MIN_LEASE_TTL_MS, MAX_LEASE_TTL_MS))
}
```

The cap is a dead-man switch and it is right: an automation client that crashes
mid-run must not leave the laser driven indefinitely. A lease that lapses makes
the modulation owner queue `STOP` + `MOD wave=OFF`, and makes the photodiode
owner finalize its recording as `LeaseExpired`.

A1 asked for a TTL covering its whole run — a frequency ladder, an amplitude
sweep, or a protocol file's remaining points — and renewed **once per point**,
in the same tick that retargeted the drive. The clamp is silent: the request is
answered `Applied`, so A1 believed it held the drive for forty minutes when the
owner had granted sixty seconds.

That worked only while every point was shorter than the cap. It is not:

- the shipped example protocol has a `duration_s = 40, settle_s = 4` row, and
  every row also pays the camera start/stop and photodiode
  connect/lease/start/finalize handshakes;
- `acquire_photodiode` asks for `duration_s + 60 s`, so **any recording longer
  than the cap** outlived its own photodiode lease.

Past the granted deadline, one root cause surfaced as three unrelated-looking
failures in the same status line:

| symptom | actual cause |
| --- | --- |
| `the modulation owner requires an active automation lease` | the lease was reaped and the drive safe-offed |
| `cannot write a quantitative A1 sidecar without a fresh photodiode optical summary` | no drive → no modulated light, and the PDQ had been finalized as `LeaseExpired` |
| `Camera: … events, … no trigger signal` | no drive → the Teensy stopped emitting the phase-0 `EXT_TRIGGER` |

The third is the one that reads as a hardware fault. It sent the operator after
a trigger cable that was never disconnected.

## Decision

**The owner's cap stays. The client renews against the deadline the owner
publishes.**

Both owners already advertise the truth: `ModulationStateV1.lease` and
`PhotodiodeSummaryV1.lease` carry a `LeaseSnapshotV1 { lease_id, holder,
expires_at_unix_ms, .. }`. A1 never read it.

A1 gains one heartbeat, `drive_lease_heartbeat`, running on every control tick
ahead of the runners:

- it finds the modulation lease A1 currently holds — outermost runner first,
  since a nested run inherits the enclosing lease id — and the photodiode lease
  of a recording in flight;
- it renews only what the **owner's own snapshot** confirms A1 is holding, so a
  lease the owner has already dropped is not chased;
- it renews once less than `LEASE_RENEW_MARGIN_MS` (20 s) of the granted window
  is left, no more often than every `LEASE_RENEW_MIN_INTERVAL_MS` (2 s) — the
  control plane ticks at 20 Hz and the owner's snapshot lags a renewal by a tick
  or two.

The per-point renewals stay. They are correct and they cost nothing; the
heartbeat covers the interval between them.

The whole-run TTL helpers stay too, and keep asking for the run's real remaining
time. That is the honest statement of need, and it is the owner's job — not the
client's — to decide how much of it to grant.

## Consequences

- A point may now be arbitrarily long. The protocol's `duration_s` is bounded
  by the protocol schema (1..=3600 s), not by an owner's lease cap.
- The dead-man switch is intact: if A1 stops ticking, the heartbeat stops with
  it and the lease lapses within the cap, exactly as before.
- A lease A1 loses anyway (owner restart, an operator disconnect) is not
  papered over. The heartbeat goes quiet because the owner's snapshot no longer
  names A1 as the holder, and the runner's own retarget reports the real
  failure in its own words. See *Amendment* below for what "reports" turned out
  to have to mean for a protocol.
- Renewal replies are not routed to any runner. An unmatched `request_id`
  already falls through `on_service_reply` untouched, so a heartbeat cannot
  be mistaken for a point's retarget outcome.
- The owners were left alone. Raising `MAX_LEASE_TTL_MS` to survey length would
  have fixed the symptom by deleting the safety property that motivated it.

## Amendment (2026-08-10) — a protocol re-takes a lease it lost

"Report the real failure in its own words" was the right instinct and the wrong
end state for an unattended run. A rejected retarget is a *per-point* failure:
the point is skipped, the run steps to the next one, and that one is rejected
identically, because nothing about advancing the index gives the lease back. One
expiry mid-survey therefore cost every row after it — a forty-point protocol
reporting `3/40 recorded — 37 skipped (… the modulation owner requires an active
automation lease)` after an hour on the bench.

A protocol that is rejected with `LeaseRequired` / `LeaseExpired` /
`LeaseMismatch` now re-acquires **the same lease id** — so it is that lease
continuing rather than a second one — announces it on the status line, and
repeats the point from the top, restating all three axes. Bounded to one retry
per point: a lease the owner will not give back still ends as a named skip
rather than a spin, and a lease held by *somebody else* (`LeaseBusy`) is never
taken from them.

This does not weaken the dead-man switch, and it does not hide the fault: the
loss is stated when it happens, and the survey's own report still names it if
the retry fails. What changed is that a recoverable interruption no longer
costs the bench time of every point that follows it.

## Also fixed here

`on_discontinuity` asked `recording.is_active() || sweep.is_some()` to decide
whether a `SourceChanged` was self-inflicted. Starting and stopping the host
recorder raises it twice per recording, and between two points of a protocol or
a frequency ladder neither of those is true — so the run's own boundary was
treated as an idle-time reset and wiped the survey's pilot windows, background
floor and response curve mid-run. The question is now `automation_active()`:
the same set `request_stop` winds down.
