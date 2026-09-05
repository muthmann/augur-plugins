# ADR 040 — Bring-up values are sorted by origin, not typed into one file

- **Status:** Accepted
- **Date:** 2026-08-14
- **Relates to:** ADR 011 (Pockels transfer calibration), ADR 016 (lobe endpoints),
  ADR 019 (calibration measures its own window), ADR 027 (declarative protocols),
  ADR 038 (A2 protocol runner), ADR 039 (A1 sidecar owns experiment provenance)

## Context

The A2 protocol is the aggregate root (ADR 038) and it fails closed, which is
correct. But it asks for every bring-up value in the same way: as a TOML field
the operator types at the bench. `a2_fluorescence_chain_followup.toml` demands
around eighteen of them, and filling it feels like busywork because three
unrelated problems have been conflated into one.

| kind | examples | where it actually lives |
| --- | --- | --- |
| an owner already knows it | `v_null_dac`, `v_peak_dac`, the `min_half_us` floor, camera-profile flags | not in the file at all |
| the result of another measurement | `h4_loopback_id`, `h5_polarity_calibration_id`, `optical_edge_calibration_id`, `local_flux_calibration_id` | a registered calibration artifact, referenced by name |
| genuinely human | filter/splitter part numbers, sample identity, "the beam is blocked" | panel inputs and a guided step list |

The first kind is the same duplication ADR 039 removed for camera and bias
settings, where the host camera sidecar became the single authoritative source.
The contract already publishes what A2 asks the operator to retype:
`OpticalDriveStateV1` carries the resolved lobe endpoints and
`ModulationStateV1::calibration_id` says which measured inversion produced them
(`None` means a hand-entered lobe); `PhotodiodeLevelV1` carries the plateau
levels with an `end_sample_index` that proves a level was measured *after* the
drive was commanded; `PhotodiodeDarkReferenceV1` already generates a traceable
ID; and the A2 runner already enforces `min_half_us >= 5 * pixel_dead_time_us`
from sensor telemetry it makes the operator type anyway.

## Decision

A2 bring-up values are treated according to their origin.

**Kind 1 is resolved from owners.** The lobe endpoints, the `min_half_us` floor
and the camera-profile flags leave the protocol file and are read from the owner
snapshot at preflight. The run records the resolved values together with the
owner's own `calibration_id`, so provenance is not lost by not typing them.

**Kind 2 becomes a registered calibration artifact.** The four `*_id` gate
strings are replaced by a table naming the calibration that produced them. An
artifact carries its own `optical_config_id`, creation time and validity, and is
accepted only when the configuration matches and it is fresh.

**Kind 3 moves into the plugin panel** as typed inputs and an ordered, gated
step list, entered once per frozen optical configuration rather than once per
measurement. The bench operator should not open a TOML.

**This strengthens the gates rather than relaxing them.** Today `real_id()`
checks only that a string is non-empty, is not `"TBD"` and does not contain
`"REPLACE"`. `"asdf"` passes. An H5 polarity calibration from a *different*
optical configuration passes, and nothing downstream would ever notice. An
artifact that names its own `optical_config_id` is machine-checkable; a hand-typed
string is not. Less typing and a sharper check are the same change here.

**A1 and A2 remain separate runners.** Different trigger sources (J24 phase-0
versus comparator), different firmware modes, different estimands, separately
tested. The defect is the *handoff* between them, and a calibration artifact is
what fixes a handoff. Merging the runners would couple a 2.6 h acquisition to a
multi-hour one for no gain.

**Resolution never weakens a gate.** A value that cannot be resolved must still
refuse. `Option<T>` and a refusal, never `unwrap_or_default()`. Convenience that
removes a refusal is a regression, not a feature.

**Calibration protocols are referenced explicitly**, resolved relative to the
referring protocol. Not by scanning a folder: implicit collection eventually
picks up a foreign file, and it cannot be audited afterwards.

**The panel is UI state, not provenance.** What a run cites still comes from
artifacts and owner snapshots, never from what the panel happened to be
displaying when the operator pressed the button.

## Consequences

The protocol file states what the *experiment* is and stops restating what the
instrument already knows. A gate can report itself as resolved, stale or missing
with the artifact ID that decided it, which is not something a `TBD` string can
do.

What remains for the operator is irreducible, and is entered once per frozen
optical configuration:

- what is physically installed that no instrument reports — filter and splitter
  part numbers, the measured split at the fluorescence wavelength, field-stop
  geometry, the sensor-plane power bound;
- sample identity and history;
- confirmation of physical acts — beam blocked, ND swapped, loopback connected.
  These stay pause-before steps, because a click is the cheapest honest
  acknowledgement that a hand moved something;
- scope-derived edge numbers (H19/H3). A 500 kSa/s ADC and 1 µs markers cannot
  resolve a sub-microsecond edge, so this stays a human reading — but a human
  reading recorded once as an artifact, not retyped per measurement.

Existing protocol files must continue to parse and continue to refuse: no
fixture may lose its fail-closed behaviour on the way through.
