# ADR 046: Confirm controller commands and preserve acquisition records

Date: 2026-09-07
Status: Accepted

## Problem

A1 preparation and drive updates shared a replaceable pending slot with UI slider
updates. A later update could remove an unsent mode change. Three A1 retarget
services returned Applied when queued, before the controller accepted them. A1
also treated an InProgress service reply as completion, and did not supply the
revision required by PrepareA1. After A2, the controller could remain stopped
although its mode was already A1 (the A2 drive-sync configuration uses A1 mode).

A missing live optical estimate prevented A1 from writing its config sidecar.
Conversely, a sidecar I/O failure could leave recording_completed_ok true.
Neither behavior describes acquisition correctly. Live analysis must not decide
whether acquisition metadata is retained.

## Decision

- Keep slider coalescing. Put automation operations in a separate bounded FIFO.
- Report InProgress until every command receives its controller response. Retain
  bounded terminal responses by requester and request ID so later requests do not
  overwrite a completion that its caller has not consumed.
- A1 polls the same request identity for completion and passes device refusals to
  its runner. It waits for preparation before issuing point commands, then waits
  for all point commands before settling and recording.
- PrepareA1 always sends STOP, CONFIG, START and STATUS. Its readback must
  confirm A1, RUNNING, J24_PHASE0 and comparator off. The reported mode alone is
  not sufficient: the A2 drive-synchronized capture configures A1 mode at its own
  sample rate, and the firmware releases an armed comparator only when a CONFIG
  leaves A2. Clear the previous A2 target metadata.
- A1 asks for preparation when the controller reports a different mode, a stopped
  acquisition, or an A2 target that the owner still carries. One preparation per
  protocol run, so a correct A1 acquisition keeps its stream.
- Explicit safe-off and lease expiry clear queued automation and do not re-arm
  the operator's previous output during lease cleanup.
- Write initial A1/A2 point metadata before camera start. Write final metadata
  even when the live optical estimate is absent. A1 records acquisition_complete,
  scientific_status=requires_offline_review and optical_unavailable. A missing
  estimate is not replaced by a commanded value in a measured-depth field.
- A1 records requested points, point outcomes, failures and final status in an
  append-only *_progress.jsonl file in the output folder. A write failure stops
  the protocol through normal cleanup instead of continuing without records.
- A fixed A1 protocol does not qualify its next frequency using the previous
  frequency's optical window. The sample-density check remains and waits briefly
  for readback after acquisition restart. Feedback depth locks retain their gates.
- Missing PDQ phase markers require controller/stream diagnosis. The camera
  trigger cable does not generate these internal markers. A short retained window
  needs more complete periods, not a lower modulation frequency.

This supersedes the sidecar-refusal behavior described in ADR 034 and ADR 045.
It does not weaken RAW/PDQ integrity checks or qualify any physical result.

## Validation and delivery

Regression tests cover command bursts, retained failures, delayed completion,
repeated A2 drive-sync/A1 preparation, metadata I/O failure and durable skipped
point records. The build workflow runs Stage-A tests on Windows as well as the
other build platforms before packaging plugins.

The laboratory target is Windows. A macOS release build is a source/build check,
not a Windows delivery or a bench pass. Install the matching Windows bundle only
after its tests and build pass. Close Augur fully before replacing all four
Stage-A plugin folders under `%USERPROFILE%\.augur\plugins`; retain BUILD-INFO.txt
with the session. The production Teensy image is `teensy41`, not the standalone
comparator bring-up image.

Before a long session, run short A1 -> A2 -> A1 captures on the actual Windows PC.
Check finalized RAW/PDQ and sidecars, PDQ sample and marker continuity, expected
camera trigger edges and the saved quality diagnostics. Repeat a few points to
exercise file finalization and reacquisition. No local mock test proves USB,
camera timing, signal quality or the installed firmware version.
