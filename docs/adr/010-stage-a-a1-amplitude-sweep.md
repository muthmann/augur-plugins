# ADR 010 — Stage-A A1 amplitude sweep via leased optical-depth retargeting

- **Status:** accepted (2026-07-23)
- **Relates to:** ADR 007 (owner orchestration), ADR 009 (recording
  coordinator), [Stage-A A1 Automation](../features/stage-a-a1-automation.md)

## Context

The A1 workflow records a response curve `q_p(a, f)`: several recordings at
different modulation depths `a` for one `(I_k, f)` row. With the manual
coordinator (ADR 009) the operator had to retarget the drive in the modulation
plugin and press *Start recording* once per amplitude. The automation roadmap
(§1–§4 of the automation brief) calls for a scoped control path: sweep only
`a`, never the rest of the drive.

Two structural gaps blocked this:

1. **No semantic "set depth" command.** The modulation service only exposed
   `SetWaveform` (raw DAC band) and `PrepareA1`. Sweeping `a` through raw DAC
   values would duplicate the optical-inversion math (ADR 008) and the
   calibration state (`V_null`, `Vπ`, `u_k`) outside their owner.
2. **Momentary buttons never reached the live worker.** The host runs a UI
   mirror and a live worker per plugin; button presses land on the mirror via
   `set_setting(key, true)`, while the worker only receives the settings
   snapshot built from `get_setting`. Buttons that returned `false` lost every
   press (the root cause of the dead record buttons).

## Decision

**1. `ModulationCommandV1::SetOpticalDepth { depth_a_milli }`** (contract
addition, additive to V1). Under an automation lease the modulation owner
re-derives its armed drive with the new depth through the same
`drive_command()` builder the operator path uses; everything else (waveform
shape, frequency, `u_k`, calibration, power cap) stays as armed. The owner
rejects the command when no device link is open, when the armed drive cannot
express a depth (manual DAC method, constant mode), or when the derived drive
violates its own safety validation. The command is applied immediately
(`Applied`), not revision-tracked: the sweep's ground truth for "the drive is
really there" is the photodiode-measured `a`, not a firmware ACK.

**2. The sweep lives in A1** as a small state machine layered *on top of* the
ADR 009 coordinator: `AcquiringLease → (per point) SettingDepth → Settling →
Recording → …release`. Per point it renews the modulation lease, retargets the
depth, waits until the photodiode-measured `a` holds the target tolerance
(±10 %, at least ±0.05) for the configured dwell (30 s cap, then it records
anyway — the sidecar stores the measured `a`), and hands off to the unchanged
recording coordinator (`…_pNN` stem tag, `sweep.requested_a` / `point_index` /
`point_total` in the sidecar). Any rejection, timeout, or failed point aborts
the sweep and releases the lease (`safe_off = false` — the drive holds; safety
remains the owner's lease-expiry job).

**3. Press counters for momentary buttons.** Every A1 button exports a
monotonic press counter from `get_setting`; `set_setting` interprets `true` as
a local click and a counter advance as one forwarded press edge, adopting the
first-seen value silently (reloads must not replay presses). The plugin-API
`Button` doc now records this idiom, and `SettingKind::Button` gained an
`enabled` flag (serde-default `true`, backward compatible in both directions)
so prerequisite-less presses can be prevented in the UI instead of rejected
after the fact.

## Consequences

- A1 now drives exactly one modulation parameter, under a lease, through the
  contract — the "A1 owns no hardware" boundary narrows to "A1 may retarget
  the armed drive's depth while leased" (the focused re-introduction ADR 007
  anticipated).
- Manual modulation settings stay locked during a sweep (lease lock), and the
  operator's own `depth a` re-applies on the next modulation settings sync
  after release.
- The press-counter idiom is the sanctioned pattern for momentary controls in
  dual-instance plugins; requested-state booleans (`connect`, `record`,
  `protocol_run`) remain correct as-is.
