# ADR 009 — Stage-A A1 as a focused recording coordinator

- **Status:** Accepted
- **Date:** 2026-07-23
- **Relates to:** ADR 005 (device ownership), ADR 006 (two-plugin split),
  ADR 007 (owner orchestration — the earlier, broader orchestrator),
  [Stage-A A1 Analysis](../features/stage-a-a1.md),
  [Stage-A A1 Automation](../features/stage-a-a1-automation.md)

## Context

The A1 measurement records, for one illumination `I_k` and frequency `f`, several
runs while sweeping the modulation depth `a`. Each run must persist the camera
**RAW** stream, the photodiode **PDQ** stream, and enough configuration to
reproduce and analyse it offline — named consistently so repeats of an `(I_k, f)`
pair stay grouped.

The previous A1 plugin (ADR 007, then the live-analysis MVP that superseded it)
was a *read-only* surface: it folded events into quicklooks and offered a manual
response-curve, but **recorded nothing**. Operators had to start/stop the camera
and photodiode recordings separately, with no shared naming and no single place
capturing the modulation settings and measured `a`. Its controls had also drifted
away from the real workflow: a "Capture camera events" toggle that recorded
nothing, an obsolete fallback frequency and phase-bin width, and interim
phase-anchoring knobs (event latency, self-align) that the now-reliable
`EXT_TRIGGER` makes unnecessary.

## Decision

1. **A1 becomes a focused recording coordinator.** One *Start recording* button,
   a chosen **folder**, a per-`(I_k, f)` **measurement id** (auto-default,
   regenerate, or edit), and a **duration** drive a small ordered state machine:
   start and acknowledge the host camera recorder; connect and lease the
   photodiode; open and acknowledge the PDQ; run for the requested duration;
   atomically finalize the PDQ and release its lease; stop and acknowledge the
   camera; then write an A1 config sidecar. This order keeps PDQ cleanup inside
   the live-effects window and starts the timer only after both streams exist.
   It deliberately **re-adds** recording orchestration that the
   live-analysis MVP had dropped — in a narrow form: only camera + photodiode
   recording, no drive/lease of the modulation device.

2. **A1 never drives the Teensy.** The optical drive is armed in the modulation
   plugin. A1 only *reads* the published `ModulationStateV1` snapshot into the
   sidecar. Reintroducing the modulation drive (settle detection, amplitude
   sweep) stays on the [automation roadmap](../features/stage-a-a1-automation.md).

3. **Consistent naming, recorder-owned directories.** Files share an
   `<id>_<timestamp>` stem under an `<id>/` subfolder. The camera RAW path is
   relative to the **host output root** and the PDQ path relative to the
   **photodiode data root** — each recorder confines its own writes, so A1 cannot
   force a single absolute directory. The A1 config sidecar is written under
   `<chosen folder>/<id>/` and records the *resolved* paths of both files, so the
   set is linked regardless; pointing all roots at the same experiment directory
   co-locates everything physically.

4. **Camera biases stay owned by the host recorder.** The host writes a companion
   `<stem>.toml` next to the RAW containing the camera config (biases, ROI). A1
   cannot read biases itself; its sidecar cross-references that file and also
   passes the key parameters as recording metadata, which the host and photodiode
   embed in their own sidecars.

5. **Two live quicklooks, clearly scoped.** Keep the **rolling half-period
   response** `S_p(t)` (live sanity: are events appearing, is ON/OFF timing sane?)
   and the **response probability** `q_p` (binary pixel-cycle statistic vs measured
   `a`). Drop the phase-bin rate plot. The authoritative `q_p(a, f)` fit is an
   **offline** computation over the recordings; the live `q_p` is a quicklook.

   **`q_p` windows: auto by default, pilot-frozen per row.** Because the
   `EXT_TRIGGER` fixes the phase, ON and OFF fall in opposite half-cycles, so the
   windows are found directly from the current fold — each anchored on its
   histogram peak and grown outward until it drops below a floor (default 10 % of
   the peak) or the opposite polarity dominates. This replaces the old
   manual-pilot *button* and its window-threshold / self-align knobs.

   The window phase depends on the event latency, which is a *phase* shift `τ·f`
   (negligible at low `f`, up to a full cycle at high `f`) and drifts with `I_k`,
   so windows must be fixed **per `(I_k, f)` row** and held across the `a`-sweep.
   A **Record pilot** action therefore freezes the auto-windows for the row and
   writes them into the pilot recording's sidecar; **Record background** captures
   the floor `q0`. Both are keyed to the measurement id (one id = one row) and are
   auto-reloaded by scanning the measurement folder, so returning to a row reuses
   its frozen windows. The live `q_p` remains a quicklook — the authoritative fit
   still freezes windows offline from the brightest run.

6. **Lean the trigger surface.** With `EXT_TRIGGER` now reliable, remove the
   fallback frequency, the phase-bin width, the event-latency shift, and the
   response-curve self-align/threshold knobs. The trigger marker spacing *defines*
   `T`; the modulation acknowledged waveform is the only fallback.

## Consequences

- A1 now declares `host_commands = ["start_recording", "stop_recording"]` in its
  manifest and holds a photodiode lease while recording (the photodiode's manual
  recording UI is locked during that window). The first host-command use triggers
  a one-time GUI consent prompt.
- A1 writes one file itself (the `.toml` sidecar) via `std::fs` — a small, bounded
  write, not a PDQ/serial writer; hardware ownership is unchanged.
- A recording reports success only after complete host finalization and a valid
  typed PDQ finalization receipt. The UI keeps one concise phase/result message,
  not a rolling internal log.
- The host returns to Preview before delivering its final receipt, which keeps
  repeated recordings and automated sweeps live without an extra operator step.
- True single-directory co-location is a **configuration** convention (align the
  recorder roots), not something A1 enforces. Enforcing it would require host and
  photodiode path changes and is out of scope.
- The contract and ABI are unchanged: every message used already exists
  (`HostCommand`, `PhotodiodeCommandV1` lease/begin/finalize).
