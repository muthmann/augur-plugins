# ADR 007 — Persistent Stage-A owners with host-routed orchestration

- **Status:** Superseded in part (2026-07-20) — the persistent two-owner model and
  host-routed control plane still hold, but `stage-a-a1` no longer orchestrates the
  A1 acquisition. It was reduced to a read-only live-analysis plugin (two
  phase-folded quicklooks + the photodiode-measured `a`); leases, recordings,
  protocol/schedule freezing, references/epochs, and the minimum-depth logistic fit
  were removed. See [Stage-A A1 Analysis](../features/stage-a-a1.md).
- **Date:** 2026-07-20
- **Amends:** ADR 005 and ADR 006

## Context

The A1 workflow must coordinate laser modulation, high-rate photodiode capture,
and camera recording. The earlier handoff proposed that `stage-a-a1` open both
Teensy ports while armed. That would create a third hardware owner and duplicate
the control/readout logic already maintained by the two manual plugins.

The existing Augur frame context cannot solve this safely: it is available only
inside `process_frame`, while device control and reference acquisition must also
progress without camera frames. Augur also loads a GUI mirror and a live-worker
instance of each plugin, so an effectful setting copied between both instances
can make them compete for the same port.

## Decision

1. `stage-a-modulation` is permanently the sole command-port owner and source of
   truth for requested and controller-ACKed modulation state.
2. `stage-a-photodiode` is permanently the sole stream-port owner and source of
   truth for PDA1 ingestion, integrity accounting, and PDQ persistence.
3. `stage-a-a1` is an orchestrator and camera-analysis plugin. It never opens a
   Teensy port and never creates a `PdqWriter`.
4. Coordination uses Augur's frame-independent, worker-owned plugin service
   plane. Requests are atomic semantic operations with stable plugin IDs,
   request IDs, leases, run IDs, expected revisions, explicit success/rejection,
   and bounded versioned snapshots. The host routes messages but contains no
   Stage-A logic.
5. Manual controls and automation share the same owner-side validation. A held
   automation lease prevents competing manual mutations; a deliberate manual
   override revokes the lease, becomes a visible workflow fault, and commands
   output-off where safe.
6. Camera start/finalize uses the allow-listed plugin-to-host recording command
   contract. RAW and PDQ receipts are correlated by immutable run ID and actual
   finalized paths; the workflow never claims filesystem atomicity.
7. Raw photodiode arrays do not cross JSON. A1 consumes small live summaries and
   parses finalized PDQ data for replayable scientific results.

## Safety and synchronization

- Only the canonical live-worker instances may effect hardware. GUI mirrors,
  replay, and offline instances are fail-closed.
- Duplicate request IDs return the original terminal response without repeating
  an effect.
- Lease expiry, replay transition, plugin disable, worker shutdown, or hard fault
  revokes control and requests output-off/finalization.
- Current PDA1 frames do not carry a shared modulation/configuration revision.
  Ordered ACKs establish operational order, but scientific cross-port identity is
  reported as `UNSYNCED` until firmware supplies a common epoch or marker.

## Consequences

- A1/A2/A3 can reuse the same owner services without duplicating serial code.
- The manual plugins remain independently useful and testable.
- ADR 005's statement that each experiment plugin owns the serial port no longer
  applies to A1/A2/A3; exclusive ownership now belongs to the two device plugins.
- ADR 006's two-port/two-owner split becomes the stable architecture instead of a
  temporary commissioning simplification.

