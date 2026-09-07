# ADR 043 — An A1 protocol run stops on a failed recording, and refuses a survey it cannot write

- Date: 2026-09-07
- Status: Accepted in source; bench qualification pending

## Context

A 297-point survey recorded 38 points and skipped over 250. Every skip carried the
same host rejection: the camera could not be opened before a transport timeout.
The run kept going to the end of the file, so a bench hour produced almost no data
and the operator learned the outcome only from the closing summary.

A second run stopped at point 12 for an unrelated reason: the photodiode sampled a
direct path with no lamp-off dark reference, so A1 could not write a quantitative
sidecar. That gate is static — it depends on the placement and its dark provenance,
not on the drive — and it fails identically for every point in the file. The run
start checked the photodiode's connection, lease, freshness and sample rate, but
not this.

## Decision

Treat the two failure classes differently.

A rejection that proves no recording started — the host refused with
`recording_start_failed` while the camera was still being started and no RAW path
exists — is retried on the same point after 1, 2 and 4 s. Any other outcome is not
retried, because a partly written recording must not be repeated silently.

A recording that still does not complete ends the run at that point. Skipping keeps
a doomed survey running: the failures that cost points are rarely specific to one
point, and an unattended bench cannot judge that.

Check the placement's dark provenance at the button press. A photodiode on the
camera or emission path with no dark reference refuses the whole protocol, in the
photodiode owner's own words, and names the open-loop way out. The estimator's
other gates need the drive to be running and stay per-point.

A restoration that stays unconfirmed keeps the camera-configuration ownership and
the failure on screen, and Stop retries it. The drive lease is released while the
run waits, so a failed camera does not hold the modulation owner as well.

## Consequences

A single transient camera failure costs seconds, not a measurement point, and a
persistent one costs one run instead of a bench day. A survey that cannot produce a
quantitative sidecar never starts.

The cost is that a failure unrelated to the camera — an incomplete photodiode file,
a failed camera stop — also ends the run. Whether those deserve a consecutive-failure
budget instead is open, and needs bench evidence about how often they occur alone.

Failure wording no longer counts camera retries when there were none; the reason the
run stopped is the reason it reports.
