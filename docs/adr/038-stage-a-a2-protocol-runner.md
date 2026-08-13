# ADR 038 — A2 is a fail-closed protocol runner over existing owners

- **Status:** Accepted
- **Date:** 2026-08-13

## Decision

`stage-a-a2` uses the host service plane. It never opens a Teensy port. A TOML
protocol is the aggregate root: optical configuration, qualified hardware gates,
controller settings and ordered recording rows must be valid together before
any effect occurs.

A complete named camera profile is part of that root. The host applies and
confirms it before either hardware lease and restores the pre-run configuration
on every terminal path. A dark row and a stepped row are different acquisition
types; dark rows force modulation safe/off and have no trigger-count gate.

The modulation contract adds `PrepareA2`, which executes `STOP`, `CONFIG mode=A2`,
`CMP` and `MOD wave=LOG_SQUARE` as one acknowledged semantic operation. The owner
requires the firmware reply to confirm comparator trigger, armed comparator and
log-square drive. Camera RAW and photodiode PDQ are then started/stopped by their
owners and linked by one run ID. Camera configuration remains host-owned.

The plugin stores acquisition provenance and live integrity evidence only.
Scientific first-event fits remain offline.

## Consequences

An incomplete bring-up file is useful but not runnable: explicit TBD gates cause
preflight refusal. The current fluorescence template records emission-path 50:50
geometry. It must not fall back to rejected-port `I_tot` semantics.
