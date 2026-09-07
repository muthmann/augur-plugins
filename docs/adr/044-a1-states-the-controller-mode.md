# ADR 044 — An A1 run states the controller's experiment mode

- Date: 2026-09-07
- Status: Accepted in source; bench qualification pending

## Context

The photodiode measures `a` from marker-bounded windows. The markers are `Marker`
frames with `source=1` that the controller stamps on the photodiode stream at every
modulation phase 0 — and it stamps them only in `mode=A1`. In `mode=A2` the optical
comparator drives the camera trigger instead and writes `source=2` markers.

`PrepareA1` existed in the contract and in the modulation owner, but no plugin ever
sent it. A1 therefore ran in whatever mode the controller happened to be in. After
any A2 work the bench answered an A1 survey with many camera triggers (comparator
edges) and no phase-0 markers at all, so every point refused its quantitative
sidecar. A live `STATUS` on 2026-09-07 read `mode=A2 trigger_source=PD_COMPARATOR
cmp_armed=1 stream_marker_drops=9306`, which is exactly that state.

The command was also unsendable. `CONFIG` accepts `mode`, `rate_hz`,
`block_samples`, `raw` and `summary`, and answers anything else with
`SYNTAX unknown_config_field`; `a1_config_command` added `wave`, `freq_mhz`,
`center_dac` and `amplitude_dac`.

## Decision

A1 sends `PrepareA1` once per protocol run, when the modulation lease is granted and
before the first point's drive commands. The owner applies its queue in order, so the
run does not wait for the acknowledgement — but a refusal ends the run, because
without A1 mode there is nothing to measure against.

`A1AcquisitionConfigV1` carries only what `CONFIG` accepts: sample rate, block size
and the two output flags. The drive stays with `MOD`, commanded per point. The
controller's own bounds (`100..=100_000` Sa/s, `1..=256` samples per block) live in
the contract crate and are checked by the owner before anything reaches the wire.

The rate A1 states is the one the photodiode already streams at. `CONFIG` carries the
rate together with the mode, so any other value would reconfigure the photodiode
owner's sampler behind its back; a rate outside the controller's window refuses the
run instead.

## Consequences

An A1 survey no longer inherits an A2 session's trigger policy, and a controller that
refuses the mode says so instead of producing points without `a`.

The A1 sidecar loses `center_dac` and `amplitude_dac`. Both read the echoed
`a1_configuration`, which no run ever populated, so both were absent from every
artifact ever written.

A2's own path is untouched and keeps two known divergences from the firmware:
`validate_a2_configuration` accepts sample rates up to 500 kSa/s where the controller
stops at 100 kSa/s and does not bound `block_samples` at all, and `a2_config_command`
sends a fixed `rate_hz=20000` rather than the configured rate. Aligning those changes
what A2 protocols are allowed to ask for and needs its own decision.
