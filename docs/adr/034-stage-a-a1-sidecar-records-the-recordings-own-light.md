# ADR 034 — The A1 sidecar records the recording's own light

- Status: accepted
- Date: 2026-08-07
- Related: [ADR 033](./033-stage-a-photodiode-ring-sizes-itself-to-the-drive.md),
  [ADR 017](./017-stage-a-rail-detection-and-withheld-a-reasons.md),
  [ADR 015](./015-stage-a-a1-recording-robustness.md)

## Context

A1 refuses to write a quantitative sidecar without a photodiode optical summary,
and the summary it used was read **live, at the moment the metadata was written**
— gated on the owner's `FreshnessV1`, a 2 s budget.

That moment is not adjacent to the recording. Between the last sample and
`write_sidecar` sit the photodiode finalize, the camera finalize, and
`gather_into_measurement_folder`, which moves the RAW, its bias sidecar and the
PDQ into the measurement folder — a `rename` within a volume, but a full **copy**
across one. All of it runs inside A1's own control tick, so no photodiode
snapshot can arrive while it happens. The freshness budget then expires against
wall-clock time the recording spent being written out, and the sidecar is
refused for a recording that is otherwise complete and correct.

The failure scales with the recording: the larger the RAW, the longer the
gather, the more certain the refusal. A run of
`a1_direct_sensor_647_gate.csv` on 2026-08-07 skipped its two 100 s rows and
recorded the 20 s row that followed them.

The refusal itself then named the `I_tot` anchor whatever the real gate had been,
so the operator was sent to re-confirm an anchor that was fine.

## Decision

**The sidecar's optical section is latched while the recording runs.** Every
control tick with an active recording copies the newest fresh
`PhotodiodeOpticalSummaryV1` into the recording state; `write_sidecar` reads that
latch, and only falls back to a live read for a sidecar written outside a
recording.

This is not only a robustness fix. The sidecar's job is to describe the light
**the recording was made under** — a summary observed after both finalizes is the
wrong number to record even when it is available. `depth_a` for a
photodiode-sourced run comes from the same latched window, so the recorded depth
and the optical section can never disagree.

**The refusal quotes the owner.** When there is no summary at all, the error
carries the photodiode's published `optical_unavailable` reason — the only side
that knows which estimator gate closed. A1's existing
`photodiode_a_blocker` gains a sibling that omits the "switch Depth `a` source"
escape, because the sidecar needs this summary whichever depth source is
selected: offering the escape there would name a way out that does not exist.

## Consequences

- A recording is no longer lost for having been large, and the sidecar carries
  the conditions of its own recording rather than of its file moves.
- A protocol point that is skipped now reports the gate that skipped it. For an
  unattended survey, that one sentence is the entire report.
- The latch holds the last summary seen *during* the recording, which for a long
  row is up to one control tick before the last sample — not the mean over the
  recording. The PDQ carries the full stream for anyone who needs more.
- Unchanged: a recording that never saw a fresh summary at all is still refused.
  Fail-closed was never the defect.
