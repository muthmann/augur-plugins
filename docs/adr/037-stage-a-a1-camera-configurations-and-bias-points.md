# ADR 037 — A1 protocols apply host camera configurations and point biases

- **Status:** Accepted
- **Date:** 2026-08-12
- **Relates to:** ADR 027 and `augur-rs` ADR 037

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

A1 routes both the initial selection and every point change through the host's
generic `ApplyCameraConfiguration` command. For a point change, A1 clones the
last host-confirmed complete snapshot and changes only its requested
`diff_on`/`diff_off` fields. Thus A1's own protocol surface stays narrow while
the host remains independent of A1 and has no bias-specific command. The host
applies the complete snapshot immediately. A1 waits for the reply containing a
sensor read taken after the change. It never waits for an extra user Apply
action and never records an unconfirmed point.

Bias control requires a fresh sensor-monitoring context before the drive moves.
A1, not the host, verifies that the confirmed snapshot enables sensor telemetry
and disables STC and Trail. Missing or disabled sensor reading, an out-of-range
offset, a rejected apply, or a mismatched/missing readback fails closed. Drive
retarget replies and the configuration confirmation must both arrive before
settle and recording.

The host-start metadata and A1 sidecar store requested offsets, confirmed
offsets, absolute current and factory codes, readback age, the immutable camera
snapshot, and profile provenance. The host restores a full configuration
session; a bias-only protocol starts the session from the currently applied
complete configuration and restores that same configuration after the run.
Normal completion, Stop, and abort use the same restore path and do not report
success until the restore reply arrives. A1 retries a rejected or timed-out
restore up to three times and reports an explicit error if none is confirmed;
it never labels an unconfirmed restore as successful.

## Compatibility

Existing TOML and CSV protocols have no camera selection and no bias columns,
so their parsed points and runtime path are unchanged. Unknown future snapshot
schemas and invalid profiles are rejected by the host.
