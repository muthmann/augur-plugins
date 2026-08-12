# ADR 037 — A1 protocols apply host camera configurations and point biases

- **Status:** Accepted
- **Date:** 2026-08-12
- **Relates to:** ADR 027, ADR 035, `augur-rs` ADR 036 and ADR 037

## Context

An A1 series can depend on the camera configuration and on different contrast
thresholds per point. Requiring an operator to move settings and click Apply
between rows is not reproducible. A1 must not open camera hardware directly or
invent plugin-local copies of host-owned global settings.

## Decision

An A1 protocol can select one complete camera configuration for the series:

- TOML uses `[camera] profile = "name"` or an inline `snapshot`, exactly one;
- CSV uses one consistent `camera_profile` value for the series.

Per-point threshold offsets use only the host's canonical names `diff_on` and
`diff_off`. TOML supports defaults and block overrides; CSV supports the two
columns per row. The values are relative offsets around the sensor's factory
trim. No `bias_on` or `bias_off` aliases are introduced.

A1 routes full configuration selection through
`ApplyCameraConfiguration` and point changes through the existing narrow
`ApplyBiases` command. The host applies them immediately. A1 waits for the host
reply containing a sensor read taken after the change. It never waits for an
extra user Apply action and never records an unconfirmed point.

Bias control requires a fresh sensor-monitoring context before the drive moves.
Missing or disabled sensor reading, an out-of-range offset, a rejected apply,
or a mismatched/missing readback fails closed. Drive retarget replies and the
bias confirmation must both arrive before settle and recording.

The host-start metadata and A1 sidecar store requested offsets, confirmed
offsets, absolute current and factory codes, readback age, the immutable camera
snapshot, and profile provenance. The host restores a full configuration
session; a bias-only protocol restores the offsets measured before the run.
Normal completion, Stop, and abort use the same restore path and do not report
success until the restore reply arrives.

## Compatibility

Existing TOML and CSV protocols have no camera selection and no bias columns,
so their parsed points and runtime path are unchanged. Unknown future snapshot
schemas and invalid profiles are rejected by the host.
