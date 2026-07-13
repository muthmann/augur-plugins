# ADR 005 — Stage-A device ownership and the `stage-a-io` boundary

- **Status:** Accepted
- **Date:** 2026-07-13

## Context

The Stage-A camera calibrations (A1–A3) drive a Teensy stimulus/DAQ
controller over USB serial while recording the event camera. Someone has
to own the serial port, the experiment state machines, and the safety
rules. The knowledge-base control-software spec fixes the boundary:
AugurRs stays a generic camera recorder and plugin host and must not gain
laboratory-instrument abstractions.

## Decision

1. **Device control lives in removable protocol plugins** (`stage-a-monitor`,
   `stage-a-a1`, later `-a2`/`-a3`), one experiment concern per plugin.
   Exactly one enabled, armed plugin owns the serial port; opening a busy
   device is a visible error.
2. **A shared plain-Rust library `stage-a-io`** (this repo, not a plugin)
   owns everything protocol-shaped: PDA1 framing + CRC resync, the ASCII
   command grammar with idempotent sequence retries, the bounded I/O
   worker, `.pdq` persistence, the run sidecar, and the calibrated optical
   contrast estimator. It contains **no experiment policy** (sweeps,
   bisection, fits stay in the plugins) and **no augur types** (testable
   without a host).
3. **Effects are gated by the host's execution context** (plugin ABI v5):
   plugins fail closed unless `LiveCapture && effects_allowed`. Hardware
   commands are host actions, never persistent settings.
4. **Wire compatibility is anchored to the firmware header**
   (`stage-a-controller/include/wire_protocol.h`); `stage-a-io` mirrors it
   with layout tests, and the mock controller implements the same
   idempotency contract the firmware promises.

## Consequences

- A2/A3 plugins reuse `stage-a-io` unchanged; only their state machines
  and views are new code.
- The GUI knows nothing about Teensys; removing the three plugins removes
  every trace of lab hardware from the product.
- Protocol changes must land in the firmware header first, then in
  `stage-a-io`, keeping a single source of truth for the wire format.
- Plugins depend on `stage-a-io` by path; it is versioned with the
  workspace and its API may still move until A2/A3 land.
