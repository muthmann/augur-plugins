# ADR 045 — A refused `a` names which gate holds it

- Date: 2026-09-07
- Status: Accepted in source; bench qualification pending

## Context

The photodiode publishes `a` only from a window bounded by phase-0 markers that
covers two whole modulation cycles (ADR 033). Everything that leaves it without
such a window produced one sentence:

```
no stretch of samples covers two whole modulation cycles between triggers
(0 trigger(s) in the last 8894464 samples) — lower the frequency, or raise the
photodiode cache length
```

Four different benches reach it, and the advice fits one of them:

- the controller is not in `mode=A1`, so no phase-0 marker is stamped at all
  (ADR 044) — no cache length helps;
- markers arrive stamped on a sample index the ring never holds, i.e. the marker
  clock and the sample clock disagree — no cache length helps;
- the stream keeps restarting, which clears the samples and their markers
  together, so the window never grows — the device is dropping samples;
- the window really is shorter than two cycles of a slow drive — raise the cache
  or lower `f`.

An A1 survey loses *every* point to whichever one it is, one full-length
recording at a time, and the sidecar refusal is the whole report an unattended
run leaves behind. Naming the wrong lever costs a survey.

## Decision

`EstimateError::IncompleteModulationCycles` carries what tells the four apart:
the marker count, the retained window in seconds, how long ago the stream
restarted when that restart is recent enough to be why the window is short, and
how many markers were stamped outside the window. The `Display` renders one
sentence per bench, and only the fourth mentions the cache length.

The counters are the photodiode owner's: it counts markers dropped for landing
before the ring, and remembers when the ring last restarted. The estimator
renders; it does not measure.

A1 latches the refusal reason on the same tick and for the same reason it
latches the optical summary (ADR 034): read after both finalizes, the reason
describes the bench after the recording, and a finalize that restarts the stream
reports a 0.2 s window whatever the real gate was.

A1 renders `Controller: mode=…` whenever the modulation owner publishes a mode
other than `A1`, and stays silent otherwise. The refusal tells the operator to
check the mode, and until now the contract carried `controller_mode` without any
panel rendering it.

## Consequences

Seconds replace sample counts in the refusal: `8894464 samples` was a number an
operator had to divide by a stream rate that is not shown anywhere.

A retained window that is short because the ring is still filling after a
restart is no longer reported as a frequency or cache problem. The restart
branch wins over the others, because a restart also explains a zero marker
count.

Marker frames stamped before the ring were dropped silently. They are counted
now, which is the only evidence that separates a trigger that is absent from one
whose clock disagrees.
