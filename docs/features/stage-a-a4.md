# Stage-A A4 Threshold Survey

- **Crate:** `plugins/stage-a-a4` (`augur-plugin-stage-a-a4`), id `stage-a.a4`
- **Status:** built — protocol runner, per-point bias confirmation, QC summary,
  sidecars and run receipt
- **Design:** [ADR 035](../adr/035-stage-a-a4-threshold-survey.md) (a threshold
  point is only real if the sensor confirms it),
  [augur-rs ADR 037](https://github.com/muthmann/augur-rs/blob/main/docs/adr/037-host-owned-camera-profiles-and-plugin-configuration-sessions.md)
  (the generic camera-configuration session it runs on),
  [ADR 027](../adr/027-stage-a-a1-declarative-protocols.md) (the protocol shape
  it follows), [ADR 028](../adr/028-stage-a-sensor-readout-travels-with-the-measurement.md)
  (the telemetry compaction it shares with A1),
  [ADR 031](../adr/031-evesmlm-plugins-share-a-types-crate.md) (why the shared
  code lives in `stage-a-plugin-contract`)
- **User docs:** [`plugins/stage-a-a4/README.md`](../../plugins/stage-a-a4/README.md)

## Purpose

A4 measures the IMX636's contrast threshold. At one **fixed optical
condition** it steps `diff_on`/`diff_off` through a protocol, records a RAW
file at each point, and writes the provenance needed to read an event rate
against a threshold setting months later.

Done by hand this is two sliders, an Apply, a wait and a Record, dozens of
times, with the codes that actually reached the sensor written down in a
notebook. A4 makes it one button — and, more to the point, makes every file
able to prove which absolute bias codes were live on the die while it was
written.

## The host half

The plugin interface could not change a camera bias at all. `HostCommand` had
two verbs, `start_recording` and `stop_recording`.

A4 was first built against a third verb written for it, `apply_biases`, which
was two fields wide precisely so a threshold survey could not disturb anything
else. That verb is gone. The host must not carry plugin- or experiment-specific
commands (augur-rs ADR 037), so what A4 runs on now is the same **generic
camera-configuration session** every other plugin uses:

- **`ApplyCameraConfiguration`** takes a *complete* configuration, from one of
  three sources: the configuration the host is currently on, a named host-owned
  profile, or an immutable snapshot the plugin supplies. The first call in a
  session makes the host preserve the pre-session state.
- **The reply is a readback, not an acknowledgement.** The host applies the
  configuration, waits for a monitoring read taken *after* the change, and
  answers `CameraConfigurationApplied` with the confirmed snapshot, its
  provenance and hash, the absolute bias codes, and the age of the reading. A
  reading older than the change cannot confirm it; one that never arrives, or
  that disagrees, is a rejection.
- **`RestoreCameraConfiguration`** puts the preserved state back. Only the
  plugin that opened the session may restore it.
- **The host owns the interlocks** a plugin cannot enforce: no change during a
  recording or its finalization, none while STC or Trail is on, none without a
  camera, and offsets inside `-85..=140`.
- `GlobalSettings` gained `event_filters` (`stc_enabled`, `trail_enabled`,
  `erc_enabled`) so a survey can refuse *before* it starts and record the state
  as provenance. This host has no event-rate controller, so `erc_enabled` is
  always `false` — the field exists so "ERC was off" is a recorded fact rather
  than an omission.

**The narrowness moved from the wire into A4.** What the old verb made
impossible, A4 now has to keep true itself: it opens each run with
`ApplyCameraConfiguration { Current }`, keeps the snapshot the host confirms,
and builds every point by cloning that snapshot and setting exactly two fields.
`fo`, `hpf`, `refr`, the ROI, the mask and the trigger are copied forward
unchanged rather than being unreachable, and a test asserts a point's
configuration equals the baseline field by field except for the two biases.

The control plane crosses the FFI as JSON, so this was wire-additive:
`PLUGIN_ABI_VERSION` stayed at 6.

## Per point

1. **Apply** the baseline snapshot with the row's two offsets set on it.
   Nothing else is changed.
2. **Confirm** the absolute codes against the sensor's own readback, and that
   the reading is fresh. Codes that disagree, or a missing or stale reading,
   **skip the point** — recording it anyway produces a file that is wrong in a
   way nobody can detect later.
3. **Settle** for `settle_s`, *and* wait for a monitoring sample newer than the
   settle. Waiting out a duration proves only that time passed.
4. **Record** for `duration_s`, counting ON/OFF events.
5. **Check** the receipt — size, hash, duration, clean finalization — and write
   the sidecar. A partial or truncated file is never counted as recorded.

Afterwards, on Stop, and on any abort, the configuration the survey found is
put back with `RestoreCameraConfiguration`; the run does not close until that
restore is answered.

## Refusals vs flags

The split is the design decision worth knowing (ADR 035).

**Hard**, because without them a threshold number means nothing: the event
filters being off, the bias codes being confirmed, a readback existing at all,
and the file being whole.

**Flags**, recorded and carried but never blocking: `max_temperature_drift_c`,
`max_illumination_drift_percent`, `max_event_rate`. Whether a 2 °C drift
invalidated a point is a judgement to make later with the file in hand.

A limit whose quantity could not be measured is flagged rather than passed —
otherwise a camera with no temperature readback silently reports every point as
within a limit nobody checked, which looks like a verified result.

## Protocols

CSV (one row per recording) or TOML (blocks and ranges), in
`plugins/stage-a-a4/protocols/`, all three shipped examples parsed as test
fixtures. Only `diff_on` and `diff_off` are required; columns are found by
header name. `repeats` expands to N separate recordings, each with its own file
and QC verdict, because the drift between two repeats is part of what the
survey measures. `pause_before` stops for a filter change or a dark cap and
waits for **Continue** — once per row, since the filter is already changed by
the time a second repeat starts.

A TOML block expands to the **product** of its two axes, which is the 2D
threshold map; a symmetric sweep is a set of specific pairs, so it belongs in
the CSV form. Axis ranges are `{ min, max, step }` rather than a point count:
bias codes are integers, and an invented spacing would not be a code the
operator chose.

Everything checkable is checked on the button press — a bad file is refused
before the first bias moves.

## What lands on disk

Under `<output folder>/<measurement id>/`: the RAW, the host's own camera/bias
sidecar, the A4 sidecar (`<stem>.a4.toml`), the compacted sensor telemetry
(`<stem>.sensor.json`), a **copy of the protocol**, and
`<id>.protocol-status.toml` with its hash and the per-row execution status.

Failed points get sidecars too. Fields the sensor could not report are absent,
never `0` (ADR 022). Sensor lux is labelled in the file as a stability
indicator, not a calibrated optical power.

## Shared code

A1's CSV record splitter and sensor-telemetry compactor moved into
`stage-a-plugin-contract` as `csv` and `telemetry`, with the schema tag
parameterised (`stage-a.a1.sensor.v1` / `stage-a.a4.sensor.v1`). Both workflows
gather the same host-written CSV, and a second copy would drift the moment the
host adds a column. A plugin crate can never depend on another plugin crate —
they all export `augur_plugin_vtable` (ADR 031) — so the shared home is the
vtable-free contract crate.

## Not built

- No live threshold curve. The rates in the panel are a stability quicklook
  counted from preview frames; the authoritative counts come from the RAW
  offline, which is where the threshold fit belongs.
- No automated filter changes. A filter wheel would remove the pauses, but it
  is a device nobody owns yet.
