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

A1 asks the modulation owner for `mode=A1` when its protocol run takes the lease,
and only when the owner's published `controller_mode` is not already `A1`. `CONFIG`
is refused while the acquisition runs and `STOP` ends the photodiode stream, so a
mode change costs a stream restart — a run that needs no change must not pay for it.

`PrepareA1` carries nothing. `CONFIG` sets the acquisition rate, block size and
output flags along with the mode, and those belong to the controller's owner: it
reads them back from `STATUS`, restates them unchanged, and refuses the command
while it has not seen them. The rate in `CONFIG` is the controller's portable-sampler
rate, which is *not* the rate the photodiode streams at — the DMA path samples at its
own fixed rate and stamps that into the frames. A1 deriving the one from the other
refused every run on a 500 kSa/s bench.

The sequence is `STOP` → `CONFIG mode=A1 …` → `START` when the acquisition was
running, so the stream comes back the way it was found. A refusal ends the run:
without A1 mode there is nothing to measure against.

## Consequences

An A1 survey no longer inherits an A2 session's trigger policy, and a controller that
refuses the mode says so instead of producing points without `a`.

The A1 sidecar loses `center_dac` and `amplitude_dac`, and the modulation target
loses `a1_configuration`. All three read a payload that no run ever populated, so
they were absent from every artifact ever written.

A run that finds the controller in A2 restarts the photodiode stream once, before
its first recording. The photodiode treats that as a new segment, which it already
does after every rate change.

A2's own path is untouched and keeps two known divergences from the firmware:
`validate_a2_configuration` accepts sample rates up to 500 kSa/s where the controller
stops at 100 kSa/s and does not bound `block_samples` at all, and `a2_config_command`
sends a fixed `rate_hz=20000` rather than the configured rate. Aligning those changes
what A2 protocols are allowed to ask for and needs its own decision.
