# ADR 028 — The sensor readout travels with the measurement, column-wise

- **Status:** Accepted
- **Date:** 2026-08-01
- **Relates to:** ADR 009 (recording coordinator), ADR 022 (sensor conditions on
  every run), [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

ADR 022 put the sensor's own measurements — die temperature, pixel dead time,
scene illumination — into every A1 sidecar as provenance. That is *one* reading,
frozen at the moment the run started.

The host separately polls the camera's monitoring block for the whole recording
and writes `<raw-stem>.sensor-monitoring.csv` beside the RAW. Two problems:

**1. It stayed behind.** A1 gathers the camera RAW, its bias sidecar, the
photodiode PDQ and its sidecar into one measurement folder under one name.
The telemetry was not in that list, so the record of how the bench actually
drifted during a run was separated from the run at the first move — left in the
host's capture folder under the host's own stem, alongside every other
recording's.

**2. The layout is padding by construction.** The channels are polled on
different schedules: the die temperature drifts over minutes, the pixel dead
time is read far more often. A row-per-poll table with a column per channel is
therefore mostly empty cells. On top of that, five of its columns are bias
codes — already recorded in the camera's own bias sidecar, which *does* travel
with the RAW.

## Decision

`gather_into_measurement_folder` picks the telemetry up, rewrites it, and
removes the original.

The rewrite is column-wise: one `{ t_us, value }` pair of arrays per channel,
carrying only the polls where that channel was actually read.

```json
{
  "schema": "stage-a.a1.sensor.v1",
  "measurement_id": "A1-20260801-1a2b",
  "recording": "A1-20260801-1a2b_20260801-120000",
  "polls": 412,
  "channels": {
    "pixel_dead_time_us": { "t_us": [1100,2100,…], "value": [12.7,12.8,…] },
    "temperature_c":      { "t_us": [1100,61100,…], "value": [41.5,41.9,…] }
  },
  "faults": []
}
```

It lands in the measurement folder as `<recording-stem>.sensor.json` and is
named in the sidecar's `[files]` block as `sensor_readout`, so it shares the
measurement's name and id like everything else in there.

Decisions inside the rewrite:

- **Nothing is resampled, interpolated or aligned.** The channels genuinely
  have different rates; a reading exists at the instant it was taken or not at
  all. Padding them onto a common grid would invent data.
- **A sample is timestamped at the midpoint of its poll.** A monitoring read
  takes a few hundred microseconds; attributing it to the start would date
  every reading systematically early.
- **Bias codes are dropped.** The camera's bias sidecar already carries them
  and it travels with the RAW.
- **Failed polls are kept as `faults`,** so a gap in a channel is
  distinguishable from a channel that was never polled — but an ordinary
  "nothing due yet" row is not a fault.
- **Columns are located by name.** A host that inserts a column must not shift
  every reading by one.
- **Rows that cannot be parsed are skipped, not fatal.** A truncated last line
  is normal when a recording is cut short, and losing the other few thousand
  samples over it would be the wrong trade.
- **The JSON is hand-rendered** so each channel's arrays stay on one line.
  These files are read by eye as often as by script, and a pretty printer puts
  one number per line.

The whole path is best-effort: a missing or unreadable telemetry file is normal
(replay, a source with no monitoring block, a host that did not poll) and never
costs the operator the recording that just finished.

## Consequences

- A measurement folder is now self-contained for the bench conditions too: the
  frozen start-of-run reading in the sidecar (ADR 022) *and* the full drift
  across the run in the readout file.
- The host's capture folder is no longer littered with orphaned telemetry.
- The parse/compact core is a pure module with its own tests, including the
  fault, truncation and column-reordering cases.
- The format is ours, versioned by the `schema` field. If the host ever emits
  something richer, the reader changes and the schema tag moves with it.
