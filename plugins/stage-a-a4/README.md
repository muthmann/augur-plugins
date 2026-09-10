# Stage-A A4 — bright background reference

Version 0.2.1 adds the matched reference for the completed A1–A3 recordings.
Source and local tests are available. A matching Windows DLL and an installed-build
smoke must be verified before using the new mode on the laboratory PC.

## Laboratory operation

1. Keep the A1–A3 AOD and laser settings. Connect the existing modulation and
   photodiode plugins and retain the applied optical transfer calibration.
2. Open **Stage-A A4 Background**. Leave **Optional custom protocol** empty and
   press **Run protocol**. The PD output folder is reused if A4 has no override.
3. Wait for **3/3 recorded** and **Biases restored**. Keep the complete measurement
   directory, including RAW, PDQ, monitoring CSV and sidecars.

No camera profile, bias, ROI, PD Start/Stop or per-point filename selection is
needed. A new measurement ID is generated if the previous ID already has data.
A4 checks filters off and fresh readback before capture; it does not silently
change a filter or mask to make the run pass.

The program preserves all five biases, ROI, mask and camera settings from the
confirmed session baseline. It sets **constant optical mean_u=0.30** through the
measured Pockels lobe. The AOD/laser attenuation remains a physical setting.
This matches the central illumination reference of the preceding modulated runs;
it is a constant-light background measurement, not another modulation recording.
The camera's actual reported lux is saved; 8 lux is only the operator's approximate
preceding value, never an automatic acceptance target.

**Duration:** 3 × 120 s recording + 3 × 10 s settling = **6 min 30 s**;
allow roughly **7–8 min** including controller/camera/file handshakes. Retries add
time. The nominal schedule is exact; the overhead estimate is not bench-measured.

The same program is available as [a4_bright_reference.toml](protocols/a4_bright_reference.toml).
For the first matching Windows installation, use [a4_bright_smoke.toml](protocols/a4_bright_smoke.toml)
once (10 s capture + 3 s settling), verify RAW/PDQ/monitoring and restoration,
then clear the custom-protocol field to use the full built-in reference.

## Failure handling and evidence

Camera attempts with confirmed idle state are retried twice at the same point.
After three failures, Continue retries that point; Stop ends the run. Unconfirmed
camera stops and failed restoration retain recovery state. Device/PD integrity
failures stop acquisition at the current point and retain available files; they
are not interpreted as valid background data. Failed cleanup can be retried with
Continue. A device failure does not start later points.

RAW is opened directly under the common absolute measurement root. Every attempt
has a distinct filename. The original monitoring CSV is retained; compact JSON
is an additional view. `attempts.jsonl` records each attempted point before the
next point starts. `.devices.json` retains device requests' receipts, optical-lobe
provenance and the confirmed camera snapshot. PD finalized receipts must name the
expected paths and contain clean, nonempty data of sufficient sampled duration.

The built-in reference does not require an independent lux meter or an optical
amplitude estimate. Camera lux is an operating coordinate with unmeasured absolute
accuracy. Absolute photon flux and QE are not outputs of this acquisition.

Automatic continuation after an application restart is not implemented in A4.
The attempt journal and retained files support identifying completed and failed
points. Do not overwrite an interrupted measurement directory.

## Legacy threshold protocols

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

See [`docs/features/stage-a-a4.md`](../../docs/features/stage-a-a4.md),
[ADR 048](../../docs/adr/048-a4-matched-bright-reference.md) for the matched reference and
[ADR 035](../../docs/adr/035-stage-a-a4-threshold-survey.md) for the threshold survey.
