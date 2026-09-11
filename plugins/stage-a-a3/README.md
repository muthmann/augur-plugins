# Stage-A A3 Threshold

A3 records slow optical contrast sweeps for offline ON/OFF threshold estimates,
spatial effective-threshold dispersion, and a bias-to-threshold map. It uses the
same acquisition implementation as A1 in `stage-a-sine-acquisition`, with its own
`stage-a.a3` identity. It does not need a fitted cutoff or a live event analysis.

## Run at the bench

1. Install a matching host and plugin bundle. Enable the modulation and photodiode
   owners and A3. Keep A1/A2 idle while A3 owns the devices.
2. Apply the measured Pockels calibration and optical log-sine drive. Keep the
   current emission PD after the fluorescence filter and 50:50 splitter, its
   measured dark reference, and J24 synchronization. Do not move the PD to the
   rejected port or add the A2 comparator path.
3. Verify the saved camera profile `A1-bias-v1-monitoring`: intended ROI/mask,
   `bias-v1`, ERC/STC/trail OFF, and sensor monitoring enabled. The shared profile
   name is intentional; A3 requests and verifies it through the host.
4. In A3, choose **Output folder**, keep or edit **Measurement ID**, and select a
   `.csv` or `.toml` file with **Protocol file**. Press **Run / resume protocol**.
5. Inspect the smoke's actual RAW/PDQ pair, steps, timing markers and companions
   before starting a longer file. **Stop and save** closes the active pair and
   restores the camera configuration. An early stop retains a partial point;
   it is not eligible for reuse on resume.

**New ID** starts an independent repeat. With the same ID and unchanged protocol,
Run records missing points and reuses only finalized local evidence with a matching
A3 schema and protocol hash. Do not combine different samples, calibrations or
bench days under one ID: make a new ID and reconcile incomplete runs offline.
The measurement ID and paths are fixed while a protocol is active.

## Shipped protocols

Durations include an estimate of 11 seconds per file. Additional camera bias
readbacks, setup, manual optical references, retries and file I/O can take longer.

| File | Scope | Recordings | Estimated duration |
|---|---|---:|---:|
| `a3_smoke.csv` | One flux; 2/8/32 Hz; two depths; sparse 0.5 Hz checks and repeated reference | 11 | 4.6 min |
| `a3_frequency_depth_core.csv` | One flux; 2/8/32 Hz; eight depths; two 0.5 Hz checks and references | 32 | 18.3 min |
| `a3_full_baseline.csv` | Three flux settings; three main frequencies; 12 depths; two opposite-order passes; sparse 0.5 Hz checks and references | 318 | 2 h 19 min |
| `a3_full_bias_map.csv` | One flux; four bias variants; three main frequencies; six depths; two passes; sparse 0.5 Hz checks and controls | 248 | 1 h 53 min |
| `a3_full_scientific.csv` | Exact concatenation of baseline and bias extension | 566 | 4 h 12 min |

Use either the combined file **or** the two separate full files. The short core is
an alternative for limited bench time, not a required extra before the full run.
All five files are generated deterministically by `scripts/build_a3_protocols.py`.

As of 2026-09-09, main depth ladders use **2, 8 and 32 Hz**. The full files add
**0.5 Hz at a=0.20 and 1.0 for every flux/bias state and pass**, matching depths
in the main ladders. The core uses a=0.18/0.9 and the smoke a=0.3/0.9 for its
slow checks. No 0.2 Hz recordings remain. Depth/flux/bias coverage, both full
passes and the main reference cadence are retained; the full low-frequency
ladders have been removed. The saved protocol hash changes: use a new measurement
ID instead of treating the revised file as a resume of the old schedule.

The baseline uses `mean_u = 0.15, 0.30, 0.45` and commanded depths
`0.06, 0.08, 0.11, 0.15, 0.20, 0.27, 0.36, 0.48, 0.63, 0.80, 1.0, 1.3`.
Full runs use `duration_s = max(10, 10/f)` and `settle_s = max(2, 2/f)`.
References interrupt each three-depth sub-block; the second pass reverses flux,
frequency and depth order. A ten-cycle point is a planned exposure, not ten
accepted cycles after exclusions. The analysis may require more repeats.

The bias extension measures offsets `(diff_on, diff_off) = (+10,0), (-10,0),
(0,+5), (0,-5)` around the zero-offset reference at `mean_u=0.30`. These are
**offsets from factory trims**, not absolute sensor codes or calibrated contrasts.
All other biases stay frozen. Confirm and register each actual readback before
scientific use. The zero-offset baseline supplies the anchor. This is a local,
one-flux map of each polarity separately, not a full two-dimensional bias map or
a proof that the bias map is flux independent.

## Files and acquisition behavior

Each point saves camera RAW, camera configuration/monitoring companions,
photodiode PDQ/JSON, and `<stem>_config.toml` beneath `<output>/<measurement_id>/`.
Names include the measurement, timestamp and protocol point conditions. The exact
protocol is archived with its SHA-256. `progress.jsonl` lives in the measurement
folder. Existing A1 metadata keys and schemas are unchanged; A3 emits
`stage-a.a3.sidecar.v2`, `stage-a.a3.progress.v1`, `stage-a.a3.sensor.v1` and
A3-prefixed PDQ metadata.

The host first confirms the camera profile/bias readback. The modulation owner
enters the existing A1/J24 firmware mode and confirms each mean/frequency/depth
command. After settling, camera recording starts, then PDQ; the exposure timer
starts after both recorders acknowledge. PDQ finalizes before the camera stops.
Bounded point retries, lease renewal, source hashing, restoration and resume use
A1's coordinator. No second serial owner or firmware mode is added.

A3 only exposes protocol recording; live plots and retained event history are
not enabled. Protocols with fewer than five commanded cycles per point are
rejected before acquisition. The shipped files use at least ten. PDQ remains
required. An unavailable live depth estimate is recorded, not replaced by a
claimed optical measurement. The command is stored as commanded depth; a live
measured value, when available, stays in `depth.measured_a`. Authoritative contrast
comes from the saved PDQ after dark correction and synchronization.

## Low-depth controls and true background

The automated low-depth controls use commanded `a=0.01`. They are deliberately
labelled as weakly modulated controls, **not constant light**: the shared protocol's
`background` role only labels a recording and does not turn modulation off.
The shipped A3 files therefore use normal recording roles for these controls.
Record true dark and constant-light camera/PDQ pairs separately before and after
the session, using A1 Record once with the source blocked or modulation set to
constant, then restore the optical log-sine before A3. Retain those reference IDs.
Do not subtract the low-depth control as if it were a pure spontaneous-event floor.

## What makes the result usable

Completion means acquisition, not a validated threshold. Every point retains
`scientific_status = requires_offline_review`; A3 never claims a qualified cutoff.
Before fitting:

- Check RAW/PDQ integrity, shared markers, actual modulation and positive,
  unsaturated dark-corrected emission. Retain constant-light noise controls.
- At each flux and bias compare ON/OFF counts per cycle against **measured**
  contrast across frequencies. Select a frequency-independent region offline;
  even 0.5 Hz is not guaranteed to be below an unknown cutoff.
- Check local contrast, registered local flux, drift/bleaching and reference
  changes. A global PD does not qualify every pixel. A static multiplicative
  intensity factor cancels in an ideal log ratio; background and flux-dependent
  temporal response do not generally cancel.
- Estimate thresholds from count-vs-contrast slopes or resolved step spacing.
  `a/N` is only a large-count diagnostic. Separate polarities and account for
  finite cycles, background, refractory losses, hot pixels and unresolved steps.
- Report effective thresholds for the documented optical configuration. A
  spatial histogram is not automatically pure comparator-offset dispersion.
  If the depth range or frequency range does not resolve a pixel, keep it
  unresolved rather than forcing a threshold fit.

Bracket each session with PD dark/constant-light references and registered frame
camera images at the flux levels used. Freeze sample, optics, ROI/mask, PD range
and bias IDs; retain a second copy of all files. These manual observations are
not supplied by a CSV and are necessary for the full planned scientific scope.

## Verification and delivery

The shared unit tests cover recording order, A3 routing/schema, resume isolation,
missing/partial evidence, source hashing, Stop and camera restoration. Protocol
fixtures are checked against the modulation owner's real drive calculations.
A local macOS build does not validate Windows DLL loading, USB acquisition or the
physical optical waveform. Complete the short Windows smoke before using A3 for
an unattended bench run.

## Controller confirmation troubleshooting

A1/A3 polls queued controller operations through read-only `QueryRequest`
requests with fresh transport IDs. AugurRS caches the first reply for each
transport ID; replaying that ID cannot retrieve a later completion. Preparation
and each mean/frequency/depth command must still receive their terminal result
before recording. Queries never re-execute the hardware operation.

Install the matching A1/A3 and modulation plugins from the same bundle and fully
restart AugurRS. Older modulation plugins do not understand the query command.
A confirmation timeout identifies the remaining controller commands separately
from the camera-bias readback. A successful software test does not establish the
installed firmware or physical acquisition: first run `a3_smoke.csv` and verify
finalized RAW/PDQ files and the saved effective sample rate before a full run.
