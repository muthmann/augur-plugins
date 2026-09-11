# ADR 039 — The A1 sidecar owns experiment provenance, not camera configuration

- **Status:** Accepted
- **Date:** 2026-08-13
- **Relates to:** ADR 020, ADR 022, ADR 027, ADR 034, ADR 037

## Context

The host writes a camera configuration sidecar beside every RAW. A1 also copied
the complete snapshot, confirmed readback, ROI, mask and absolute/factory bias
codes into its own config sidecar and into recorder metadata. The copies were
not independent measurements and could disagree. At the same time, the A1
sidecar did not identify the exact protocol file that produced the point, and
commanded and measured optical depth appeared under several overlapping names.

The detector can also move. The historical PBS rejected-port geometry measures
`I_pd = I_tot - I_exc`. A detector behind a camera/emission-path beamsplitter
measures the local signal directly; applying the complement model there is a
scientific error.

## Decision

New A1 sidecars use `schema = "stage-a.a1.sidecar.v2"`.

- `[protocol]` records name, optional author version, source filename, SHA-256,
  archived copy, point index/count/label/role and requested axes/bias offsets.
- `[depth]` separates `analysis_a`, `commanded_a` and `measured_a`, with an
  explicit `analysis_source`.
- `[photodiode]` records detector placement and splitter fraction. `I_tot`
  remains only in the photodiode owner's artefact when rejected-port geometry
  uses it; A1 does not copy it.
- `[sensor]` keeps only dynamic conditions: temperature, dead time, scene lux
  and reading age. Camera configuration, ROI/mask and bias codes remain in the
  host sidecar referenced by `[files].camera_config_sidecar`.
- The exact protocol source is copied once per content hash into the
  measurement folder. The original path is not treated as durable provenance.

`PhotodiodePlacementV1` distinguishes `rejected_port`, `camera_path` and
`emission_path`. Only `rejected_port` uses the learned full-extinction anchor.
Direct paths use a session-local lamp-off dark reading. They fail closed until
the operator explicitly captures or enters it. The calibration and artifacts
carry its value, source, ID, capture time, and age; a typed value is labelled
`manual` and is never confused with a measured lamp-off reference. `splitter_fraction` is
provenance and does not rescale log contrast.

## Compatibility

Old JSON control snapshots that omit placement decode as `rejected_port`, the
only geometry supported by those owners. Existing v1 A1 sidecars remain valid
input to offline analysis. Readers accept both layouts:

| legacy v1 | v2 |
| --- | --- |
| `depth_a_source` | `depth.analysis_source` |
| `depth_a` | `depth.analysis_a` |
| `modulation.requested_a` / `sweep.commanded_a` | `depth.commanded_a` |
| `optical.measured_a` | `depth.measured_a` |
| `optical.*` | `photodiode.*` |

The writer emits only v2. It does not retain duplicate deprecated fields.

## Consequences

The host camera sidecar is the single source of truth for camera configuration.
The A1 sidecar is the source of truth for schedule identity, optical provenance
and cross-file links. A direct-path measurement can no longer be blocked by or
silently corrected with an unrelated `I_tot` estimate.
