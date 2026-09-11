# ADR 006 — Stage-A simplification: two plugins, one serial port each

- **Status:** Accepted
- **Date:** 2026-07-15
- **Amends:** ADR 005 (Stage-A device ownership)
- **Amended by:** ADR 007 (persistent owners with host-routed orchestration)

## Context

The three commissioning plugins (`stage-a-monitor`, `stage-a-funcgen`, `stage-a-a1`, ~3100 lines)
bundled experiment state machines, contrast estimation, and drive control into UIs that were too
complex and opaque for the current bench stage. What the bench actually needs now is:

1. direct, immediate control of the laser modulation output (capped power slider,
   constant/sine/square with frequency), and
2. a plain readout of the photodiode (raw, or inverted to excitation power).

Both need the same Teensy, but ADR 005 fixes one owner per serial port — and a context-bus
coupling (one plugin republishing data for the other) would make the readout depend on the
control plugin's connection.

## Decision

1. **The firmware enumerates two USB CDC ports** (`USB_DUAL_SERIAL`, `stage-a-controller`
   ADR 002): port 1 keeps the v1 command protocol; port 2 free-runs the PDA1 photodiode
   stream. ADR 005's rule is unchanged — one owner per port — there are simply two ports now.
2. **Two minimal plugins replace the three commissioning plugins** (deleted 2026-07-15, retained
   in git history):
   - `stage-a-modulation` owns the command port (`docs/features/stage-a-modulation.md`);
   - `stage-a-photodiode` owns the stream port (`docs/features/stage-a-photodiode.md`).
3. **`stage-a-io` stays** as the protocol library (wire format, client, worker, firmware-faithful
   mock — the mock now models firmware 0.3.0's `MOD` verb). Owner plugins use its transport/PDQ
   pieces. A1/A2/A3 do not open transports; they use the host-routed owner contract (ADR 007)
   and may use hardware-free parsing/analysis helpers.
4. **Immediate transfer replaces the Apply-action pattern**, and **all device control is
   settings-driven** (connect checkbox, slider changes sent as they happen). Host actions and
   the per-frame effects gate are unsuitable here: the host only runs `process_frame()` while
   camera frames flow, but the bench must work with no camera attached (amended 2026-07-16).
   Replay mode still disconnects the modulation plugin defensively. The firmware output is
   set-and-hold. ADR 008's 2026-07-23 amendment separates Manual/Calibrated drive method from
   waveform mode and makes `max_level` the universal DAC ceiling.

## Consequences

- Each owner plugin has a single hardware concern; the photodiode plugin uses
  `stage-a-io`'s PDA1 parser and PDQ persistence without taking command-port ownership.
- Both plugins work independently — either can connect, disconnect, or crash without affecting
  the other.
- Wire-protocol changes still land firmware-first (`stage-a-controller/include/wire_protocol.h`
  and command grammar), then in `stage-a-io`'s client/mock.
- The A1 min-depth workflow is rebuilt as an orchestrator on this stable two-owner
  stack; it never becomes a third Teensy owner.
