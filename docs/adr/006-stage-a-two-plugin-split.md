# ADR 006 — Stage-A simplification: two plugins, one serial port each

- **Status:** Accepted
- **Date:** 2026-07-15
- **Amends:** ADR 005 (Stage-A device ownership)

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
   ADR 002): port 1 keeps the v1 command protocol; port 2 free-runs a plain-ASCII photodiode
   stream. ADR 005's rule is unchanged — one owner per port — there are simply two ports now.
2. **Two minimal plugins replace the three commissioning plugins** (deleted 2026-07-15, retained
   in git history):
   - `stage-a-modulation` owns the command port (`docs/features/stage-a-modulation.md`);
   - `stage-a-photodiode` owns the stream port (`docs/features/stage-a-photodiode.md`).
3. **`stage-a-io` stays** as the protocol library (wire format, client, worker, firmware-faithful
   mock — the mock now models firmware 0.3.0's `MOD` verb). The A1/A2/A3 experiment plugins will
   build on it again when the bench reaches that stage; the estimator/pdq/sidecar modules remain
   for that purpose even though no current plugin uses them.
4. **Immediate transfer replaces the Apply-action pattern** in `stage-a-modulation`: setting
   changes are sent to the device as they happen (the operator's explicit request), still behind
   the fail-closed execution-context gate. The firmware output is set-and-hold; the explicit
   "Output OFF" action is the only stop.

## Consequences

- Each plugin is a few hundred transparent lines with a single concern; the photodiode plugin
  does not even depend on `stage-a-io`.
- Both plugins work independently — either can connect, disconnect, or crash without affecting
  the other.
- Wire-protocol changes still land firmware-first (`stage-a-controller/include/wire_protocol.h`
  and command grammar), then in `stage-a-io`'s client/mock.
- The A1 min-depth workflow is gone from the tree until it is rebuilt on the simplified stack;
  its last state is tagged by the deletion commit.
