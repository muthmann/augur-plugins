# Stage-A Plugin Contract

This crate is the serde-only control-plane contract between the Stage-A workflow plugins and the
two plugins that permanently own the Teensy ports:

- `stage-a-modulation` owns and controls the command port;
- `stage-a-photodiode` owns and reads the PDA1 stream port;
- experiment plugins such as `stage-a-a1` orchestrate those owners without opening either port.

The crate deliberately has no Augur, serial, filesystem, or thread dependency. Its payloads can be
serialized through the host's persistent plugin context. Every context key and payload is
explicitly versioned. A request carries a unique request ID, the target owner instance, an
optional lease and run ID, and an optional requested semantic revision. Responses echo those
identities and report the ACKed revision.

## Mailboxes

| Direction | Context key |
|---|---|
| orchestrator → modulation owner | `stage_a.modulation_request.v1` |
| modulation owner → orchestrator | `stage_a.modulation_response.v1` |
| modulation owner snapshot | `stage_a.modulation_state.v1` |
| orchestrator → photodiode owner | `stage_a.photodiode_request.v1` |
| photodiode owner → orchestrator | `stage_a.photodiode_response.v1` |
| photodiode owner snapshot | `stage_a.photodiode_summary.v1` |

Persistent context is a last-writer-wins mailbox, not a queue. An orchestrator must keep at most
one outstanding request per owner, retain it until its request ID is acknowledged, and never
reuse a request ID. Owners must make duplicate delivery idempotent by returning the original
result without repeating the effect.

## Safety and data boundaries

Control commands are semantic (`PrepareA1`, `SafeOff`, `BeginRecording`, and so on), not raw
firmware strings or remote setting changes. Automated mutations require an owner-issued lease;
leases expire unless renewed. Owner snapshots carry an instance ID and freshness deadline so an
orchestrator can detect reloads and stale state.

Photodiode messages contain only bounded summaries and named PDQ receipts. Raw ADC arrays never
cross the JSON context. The finalized receipt names the PDQ/sidecar, SHA-256, byte and frame
counts, contiguous sample range, stream integrity, and validity. Analysis reads the finalized PDQ
through `stage-a-io`.

`SynchronizationV1::Unsynced` is a first-class state. Missing firmware configuration revisions,
owner restarts, stream-epoch changes, stale snapshots, or run-ID mismatches must be reported as
UNSYNCED rather than inferred away.

