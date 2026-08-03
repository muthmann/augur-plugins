# ADR 022 — Every A1 run records the bench conditions the sensor measured

- **Status:** Accepted
- **Date:** 2026-07-31
- **Relates to:** ADR 009 (recording coordinator), ADR 015 (recording
  robustness), ADR 020 (depth source provenance),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

The A1 sidecar already reproduces everything about the *drive* — frequency,
depth, calibration, anchor, ROI, trigger — but nothing about the physical state
of the sensor while a run was taken. Three quantities the camera measures for
itself are now available on the host's per-frame context bus
(`CTX_SENSOR_MONITORING` → `SensorMonitoringV1`):

| quantity | field | why it matters to `q_p(a, f)` |
| --- | --- | --- |
| pixel dead time (refractory period), µs | `pixel_dead_time_us` | caps how many events a pixel can emit per half-cycle; at high `f` it *is* the ceiling the response saturates against |
| scene illumination, lux | `illumination_lux` | the physical `I_k` axis the whole experiment is stratified on |
| die temperature, °C | `temperature_c` | moves the biases, so two rows at nominally identical settings are not comparable across a large drift |

None of these is derivable from the recording afterwards, and all three drift
over a session. A row that cannot be compared to another has to be identifiable
as such at analysis time, which means the numbers belong in the artefact, not in
a lab notebook.

## Decision

A1 mirrors `SensorMonitoringV1` every frame and writes it into **every**
recording, in every mode — normal, pilot, background, amplitude-sweep point and
event-count point alike. This needs no per-mode work: both write paths are
already shared, so the values go into `recording_metadata()` (which both the
camera and PDQ recorders embed) and into a new `[sensor]` section of the A1
config sidecar.

Four properties are load-bearing.

**Mirrored above the `live` gate.** `process_frame` returns early when Live
analysis is off, and recordings are made that way at least as often as not. The
mirror therefore sits with the ROI mirror, before the gate.

**Frozen at recording start.** These quantities drift; the sidecar is written at
finalize, seconds to minutes later. The number that belongs to a run is the one
that held when it began, so `begin_recording` snapshots `sensor_at_start` before
any of the start handshake runs, and the writers prefer it over the live value
(falling back to the live one only if the run began before any frame carried a
reading).

**Absent, never zero.** Every field is optional at three levels: the host
publishes nothing at all during replay, decoded imports and offline re-runs
(there is no device to ask), a sensor without a monitoring block publishes
nothing, and an individual quantity can be `None` on a sensor that has one. A
`0 °C` die or a `0 lx` scene reaching an analysis script as a measurement is the
failure mode this exists to avoid, so a missing quantity omits its key entirely
rather than defaulting.

**Provenance only, never an input.** No result A1 computes may depend on these
values. The API's own docs are explicit about why: a plugin whose answers vary
with them would disagree between a live run and a deterministic offline re-run
of the same data. `age_s` is recorded alongside (`sensor_reading_age_s` /
`reading_age_s`) because the host polls at a few hertz — a reading is never
simultaneous with the run it is attached to, and the sidecar says how stale it
was rather than implying it was not.

The absolute bias codes that arrive with the same struct are written too
(`bias_diff_on`, `bias_diff_off`, `bias_fo`, `bias_hpf`, `bias_refr`). The host
camera config expresses biases as *relative* offsets around a per-unit factory
trim, so these are the only absolute record of what the sensor was actually
programmed to.

## Consequences

- The A1 sidecar gains an optional `[sensor]` section; both recorders' metadata
  gains `sensor_temperature_c`, `sensor_pixel_dead_time_us`,
  `sensor_illumination_lux` and `sensor_reading_age_s`.
- The status panel shows the live reading on one line when the host reports one,
  and stays silent when it does not.
- Sidecars written from replay or from a camera without a monitoring block have
  no `[sensor]` section at all — an analysis script must treat it as optional,
  exactly as it must treat `optical.measured_a` under ADR 020.
- Only `refr` among the biases has a vendor-documented physical unit, which is
  why the other four are recorded as codes and not converted.
