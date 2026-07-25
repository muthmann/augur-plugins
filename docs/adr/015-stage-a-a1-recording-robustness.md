# ADR 015 — Stage-A A1 recording: one folder, full duration, named failures

- **Status:** Accepted
- **Date:** 2026-07-25
- **Relates to:** ADR 009 (A1 as a recording coordinator — revises decision 3 and
  its co-location consequence), ADR 005 (device ownership),
  ADR 006 (two-plugin split),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

On the bench, *Start recording* looked like it worked and then reported a
finished run almost immediately. What actually landed on disk was:

- an A1 `<stem>_config.toml` in the chosen output folder,
- a **truncated** camera `.raw` (plus the host's bias `.toml`) in an unrelated
  directory — the host process's working directory,
- **no `.pdq` and no photodiode sidecar at all**,
- and a status message that said only "was incomplete".

Three separate defects produced that outcome.

1. **A photodiode failure cut the camera recording short.** The coordinator
   starts the camera first, then connects, leases, and opens the PDQ. Every
   photodiode-leg failure — a rejected `Connect`, a refused lease, or a
   `BeginRecording` rejected because the photodiode's *Data directory* was unset
   — jumped straight to `stop_camera`. The host had been recording for a few
   hundred milliseconds, so the RAW was a stub that nevertheless carried a
   complete finalization receipt. `stop_requested` was also set on photodiode
   faults, conflating "the operator asked to stop" with "the photodiode broke".

2. **The failure reason was discarded.** Each failure wrote a specific message
   (`Photodiode start failed (invalid_path): set the data directory first`), and
   `finish_recording` then overwrote it with the generic "was incomplete".
   The one piece of information the operator needed was destroyed on the way out.

3. **One measurement scattered across up to three roots.** Per ADR 009 decision 3
   each recorder confines its own writes: the host resolves plugin recording paths
   below *its* output directory and **rejects absolute paths**; the photodiode
   resolves PDQ paths below *its* data directory; A1 writes its sidecar below
   *its* output folder. ADR 009 accepted this and called physical co-location a
   configuration convention. In practice the host's output path was relative, so
   its parent resolved to the process working directory, and the RAW landed in a
   source checkout — nowhere near the experiment folder.

## Decision

1. **The camera RAW always runs its full duration.** A photodiode failure while
   the camera is already recording no longer stops it. The run continues to the
   requested duration and closes normally, with the sidecar and message marking
   it camera-only. A complete camera-only recording is a usable measurement; a
   truncated file that reports itself as finalized is a trap. `stop_requested`
   now means only what its name says — an operator stop — and photodiode faults
   travel in `pd_rejected`.

2. **The photodiode is pre-flighted before the camera starts.** A recording is
   refused, with nothing recorded and an actionable message, when the photodiode
   is not reporting status, is not connected, has no data directory, or is leased
   by another client. These were exactly the conditions that used to surface as a
   PDQ rejection *after* the host was already recording. The same check feeds the
   A1 status view while idle, so the blocker is visible **before** the operator
   presses Record rather than after a wasted run.

   This needs the owner's data directory, so `PhotodiodeSummaryV1` gains an
   additive `data_dir: Option<String>` field (`#[serde(default)]`, absent from
   older owners, ignored by older consumers — the contract version is unchanged).

3. **The first failure is preserved and named.** `Recording::failure` keeps the
   first, most specific cause; later fallout cannot overwrite it. The closing
   message reads `Recording <id> incomplete: <cause> — metadata saved to <path>`.

4. **A1's output folder becomes authoritative for the whole measurement.** After
   both recorders report finalization, A1 moves the RAW, the host's bias sidecar,
   and the PDQ and its sidecar into `<output folder>/<id>/`, then writes the
   config sidecar with the final paths. This reverses ADR 009's "co-location is a
   configuration convention" without touching the host or photodiode path rules:
   both files are closed and hashed by the time their receipts arrive, so moving
   them afterwards is safe and stays inside each owner's contract.

   The move is a `rename` on one volume and a size-verified copy-then-delete
   across volumes. It never overwrites an existing destination and never removes
   a source it has not verified; if a move fails, the file stays put and the
   sidecar records where it actually is.

   PDQ receipts report the path **label** A1 requested — relative to the
   photodiode's data directory — not an absolute path, so A1 resolves it against
   the `data_dir` from decision 2 before locating the file. The sidecar records
   the resolved absolute path either way, which also fixes the previous ambiguity
   of storing a bare relative label under `[files]`.

5. **Self-inflicted pipeline restarts no longer wipe the row.** Starting and
   stopping the host recorder restarts the capture pipeline, which the host
   reports as `SourceChanged` — twice per recording, caused by A1 itself. That
   used to clear the pilot windows, the background floor, and every response
   point collected across a sweep. While a recording or sweep is in flight the
   boundary now resets only the event fold, whose timeline genuinely did restart.

## Consequences

- A recording can now end as *camera-only*: `recording_completed_ok` stays false,
  so an amplitude sweep still stops rather than silently collecting points with
  no measured `a`. The RAW is complete and reusable.
- A misconfigured bench refuses to record instead of producing a stub. This is a
  deliberate behaviour change: pressing Record with a disconnected photodiode
  now yields a message and no files, where it previously yielded a junk RAW.
- The measurement folder is the single place to look. Files are no longer where
  the host and photodiode settings happen to point, so operators do not have to
  keep three roots aligned by hand. Aligning them is still harmless — a file
  already in the destination is left alone.
- Moving a large RAW across volumes copies it. On one volume (the normal case)
  the move is a metadata operation regardless of file size.
- The contract addition is additive and backward compatible; no ABI change.
