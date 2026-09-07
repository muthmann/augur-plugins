# Stage-A A4 — contrast-threshold survey

Reproducible `diff_on`/`diff_off` threshold measurements on the IMX636. At one
fixed optical condition, A4 walks a protocol of bias pairs and records a RAW
file at each, with enough provenance to read an event rate against a threshold
setting months later.

- **Crate:** `augur-plugin-stage-a-a4` · **id:** `stage-a.a4` · **phase:** `raw_events`
- **Host commands:** `start_recording`, `stop_recording`,
  `apply_camera_configuration`, `restore_camera_configuration`
- **Requires:** augur-rs with the generic camera-configuration session
  (augur-rs ADR 037)

## What it changes, and what it does not

A4 changes **two registers**: `diff_on` and `diff_off`. The host offers one
generic verb that carries a whole configuration — there is no A4-specific
command — so the freeze on `fo`, `hpf`, `refr`, the ROI and the pixel mask is
kept by A4 itself: it opens the run by asking the host to confirm the
configuration the bench is on, and every point is that confirmed snapshot with
exactly two fields changed. A test asserts the equality field by field.

They are recorded with every point exactly as A4 found them.

The optical condition is yours. A4 never drives the Teensy and never touches a
filter; a row that needs one says `pause_before` and waits for a button.

## Per point

1. Clone the configuration the session confirmed, set its `diff_on`/`diff_off`,
   and send it back as `ApplyCameraConfiguration`.
2. **Confirm against the sensor's own readback** that the absolute codes on the
   die are `factory_default + offset`. A point whose codes disagree, or whose
   confirming reading is missing or older than the change, is skipped — it is
   not measuring what the protocol says it measures.
3. Settle for `settle_s`, *and* wait for a monitoring sample newer than the
   settle. A settle that produced no fresh telemetry is not a settle.
4. Record for `duration_s`, counting ON/OFF events.
5. Check the receipt — size, hash, duration, clean finalization — and write the
   sidecar. A partial or truncated file is never counted as recorded.

On completion, on Stop, and on any abort, the configuration the bench was on
before the survey is put back — the host preserved it when the session opened,
so `RestoreCameraConfiguration` returns the whole state, not only the two
biases. The run does not close until that restore is answered.

## Refusals vs flags

The split is deliberate.

**Hard refusals** (nothing runs, or the point is skipped) are the things that
make a threshold number mean anything at all:

- STC, Trail or ERC enabled — they discard events before streaming, which is
  the quantity being counted
- no bias readback available — the method's central claim would be uncheckable
- bias codes that disagree with the row, or a stale confirming reading
- no output folder, an unreadable or invalid protocol
- a partial, empty, unhashed or truncated recording

**Flags** (the point is recorded and kept, and marked) are the bench-stability
limits: `max_temperature_drift_c`, `max_illumination_drift_percent`,
`max_event_rate`. Whether a 2 °C drift invalidated a point is a judgement to
make later with the file in hand — a runner that discarded it would have thrown
away the evidence for making it.

A limit whose quantity could never be measured is flagged too, not passed: a
camera with no temperature readback must not silently report every point as
within a drift limit nobody checked.

## Protocols

`protocols/` ships three worked examples, all parsed as test fixtures.

**CSV — one row per recording.** Only `diff_on` and `diff_off` are required;
columns are found by header name, so their order does not matter.

```csv
label,optical_state,diff_on,diff_off,duration_s,settle_s,repeats
threshold-01,LP647+BP700,-20,-10,60,5,2
threshold-02,LP647+BP700,0,0,60,5,2
```

Optional: `pause_before`, `max_temperature_drift_c`,
`max_illumination_drift_percent`, `max_event_rate`, `filter_id`, `flux_id`.

**TOML — blocks and ranges.** A block expands to the **product** of its two
axes, which is the 2D threshold map. A symmetric sweep is a set of specific
pairs, not a product, so it belongs in the CSV form.

```toml
[defaults]
duration_s = 60
settle_s   = 5

[[block]]
diff_on  = { min = -20, max = 20, step = 10 }
diff_off = 0
```

Bias values are **offsets** around the per-unit factory trim — the same numbers
the host settings panel shows. The absolute codes come from the sensor.

Everything checkable is checked on the button press: a bad file is refused
before the first bias moves, not at 3 a.m. on row 37.

## What lands on disk

Under `<output folder>/<measurement id>/`:

| File | What it is |
|---|---|
| `<stem>.raw` | the recording, gathered out of the host's capture folder |
| `<stem>.toml` | the host's own camera/bias sidecar, travelling with its RAW |
| `<stem>.a4.toml` | the A4 sidecar — protocol row, bias codes, bench conditions, QC |
| `<stem>.sensor.json` | the host's telemetry, compacted column-wise |
| `<protocol>.csv` \| `.toml` | a copy of the protocol that ran |
| `<id>.protocol-status.toml` | its hash, and the per-row execution status |

A sidecar is written for failed points too — the record of a failed point is
the reason the survey has a hole in it.

Fields the sensor could not report are **absent**, never `0`: a die temperature
of 0 °C and "this camera has no temperature readback" are opposite facts.

Sensor lux is a stability indicator, not a calibrated optical power. The
sidecar says so in the file.

## Notes

- QC rates are counted from the preview frames the plugin observed, over
  `counted_seconds`; compare that against `recorded_duration_s` for the
  coverage. The authoritative counts come from the RAW offline.
- `Restore biases` is the recovery path for a run that could not restore them
  itself. During a run it is refused. It asks the host to put its own preserved
  configuration back, so a host that was reloaded mid-survey has no session
  left and refuses — that gap is not yet closed.

See [`docs/features/stage-a-a4.md`](../../docs/features/stage-a-a4.md) and
[ADR 035](../../docs/adr/035-stage-a-a4-threshold-survey.md).
