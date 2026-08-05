# Stage-A A1 Analysis

`stage-a-a1` is the Stage-A **recording coordinator** plus two live sanity quicklooks. One button
records the camera **RAW** stream and the photodiode **PDQ** stream together for a fixed duration,
groups them under a per-`(I_k, f)` measurement id, and writes an A1 config sidecar (`.toml`) linking
the files with the modulation settings, the measured modulation depth `a`, the ROI, and the trigger
info needed to reproduce and analyse the run offline. A second button, **Start sweep**, repeats that
per amplitude: it leases the modulation owner, retargets the armed calibrated drive to each `a` in
`[Sweep min a, Sweep max a]`, waits for the photodiode-measured `a` to settle, and records every
point (`…_pNN`). Outside the leased sweep A1 owns no hardware and never drives the Teensy — arm the
optical drive in the modulation plugin; A1 only reads its published settings.

## Where `a` comes from

**Depth a source** picks what every depth-dependent path — the sweep, **Find a₀**, the frequency
ladder, the response curve — reads as `a`:

- **photodiode (measured)** — the default and the source of record. Fail-closed: the photodiode
  publishes no `a` unless firmware phase-0 markers on its stream port prove the estimator window
  covers whole modulation cycles. If those markers never arrive (no trigger, or firmware that does
  not stamp them) it refuses forever — *"0 trigger(s) in the last 3446784 samples"* is a missing
  marker stream, not a short window, and no setting fixes it.
- **modulation drive (commanded, open loop)** — the depth the modulation owner's calibrated drive is
  commanding (`optical_drive.depth_a_milli`). Still a calibrated number, inverted from the measured
  `V_null`/`V_peak` curve, but **not checked against the light**: it carries the calibration's error plus
  any drift since. Needs an applied calibration and `OPTICAL_LOG_SINE` armed; a manual DAC band
  publishes no optical drive and the gates refuse rather than inventing a depth.

Open loop there is **nothing to search for**, so `Find a₀` is not used and the frequency ladder skips
it (see below), and the photodiode's window-length and clipping checks are skipped because neither
bounds a commanded depth. Every artefact records the source: `depth_a_source`/`depth_a` in the sidecar,
`depth_source` in `[a0_lock]` and in `a0_locks.json`, and the *a from* column of the a₀ lock view.
`measured_a` stays reserved for a number the photodiode actually measured. See
[ADR 020](../../docs/adr/020-stage-a-a1-depth-source.md).

## Recording

- **Output folder** — where the A1 config sidecar is written (recommended shared experiment root).
  The only field that has to be filled in before recording.
- **Measurement id** — one per `(I_k, f)` pair; auto-generated default, editable, or press **New id**.
  Optional: a blank field is filled in on the first recording and written back (ADR 018).
- **Duration (s)** — each recording auto-stops and finalizes after this.
- **Start recording** — starts camera RAW, then connects/leases the photodiode and starts PDQ;
  the timer begins after both acknowledge. It auto-finalizes PDQ first, camera second, then writes
  the sidecar. **Stop** saves the current recording early (and aborts a running sweep).
- **Start sweep** — records **Sweep points (count)** amplitudes spanning `[Sweep min a, Sweep max a]`
  (min > 0): per point it renews the modulation lease, issues `SetOpticalDepth`, waits for the
  fresh marker-bounded measured `a` to hold the target for **Sweep settle (s)**; timeout aborts
  rather than recording an unsettled point. The modulation owner also requires an applied
  transfer calibration and `OPTICAL_LOG_SINE`, and runs
  one normal recording. Sidecars carry `sweep.requested_a` / `point_index` / `point_total`.
- The record/sweep buttons are disabled until an output folder is selected.

## Exact event count (`a₀` lock)

The second Stage-A workflow holds **one** photodiode-measured depth
`a₀ = ln(I_exc,max / I_exc,min)` constant while the frequency varies. Because the measured Pockels
inversion is static, the delivered depth rolls off with frequency — so the depth must be found by
measurement, not calculated.

- **a₀** / **a₀ tolerance** — the frozen measured depth and its convergence band (default ±0.02).
- **Find a₀** — per frequency: leases the modulation owner and iterates
  `commanded a ← commanded a · a₀/measured a` (≤ 8 trials, averaging three fresh photodiode summaries
  per trial after **Sweep settle (s)**) until the photodiode measures `a₀`. Records nothing, leaves the
  drive at the depth it found, and stores one row per frequency — shown in the **A1 a₀ locks** view and
  mirrored to `a0_locks.json`. An unreachable `a₀` is reported (drive limit or the owner's own
  rejection) before any data is recorded.
- **Record a₀ point (event-count)** — re-applies the locked depth under the lease (so the amplitude
  cannot change during the recorded interval), waits for the measured `a` to hold `a₀`, and records
  one atomic frequency point named `…_ec_f<f>Hz` with an `[a0_lock]` sidecar section.
- **Clear a₀ lock table** — after changing the illumination, the calibration or `a₀` itself.

Frequency order, the interleaved low-frequency reference and the repeated blocks stay yours — every
point is one button press.

### With the commanded depth source there is no search

Everything above describes the *measured* workflow. `Find a₀` exists only because a **measured** `a₀`
has to be re-found per frequency against the static inversion's roll-off. A **commanded** depth is
the number being commanded, so the correction ratio is exactly 1 and a search would command `a₀`,
read back `a₀` and stop. It is therefore not run at all (ADR 021):

| | photodiode (measured) | modulation drive (commanded) |
|---|---|---|
| `Find a₀` | trims per frequency, stores a lock | **disabled** — says why |
| `Record a₀ point` | replays the stored lock | commands `a₀` directly |
| ladder per rung | set `f` → confirm via **camera markers** → search → record | set `f` → confirm via the **modulation owner's ACK** → record |
| `a0_locks.json` | one row per frequency | untouched |
| needs EXT_TRIGGER + Live analysis | **yes** | no |

So open loop the whole workflow is: set `a₀`, press **Record all frequencies**. The trade is the one
the lock removes — nothing verifies the light reached `a₀`, and the roll-off is real — so switch back
to the photodiode once its markers work.

## Depth sweep at every frequency (the `q_p(a, f)` surface)

The frequency ladder is an **outer loop**; what it records per rung is a mode:

| button | per frequency | produces |
|---|---|---|
| **Record all frequencies** | one event-count point at `a₀` | `q_p(a₀, f)` |
| **Record depth sweep at every frequency** | the whole `[min a, max a]` sweep | `q_p(a, f)` — a curve per `f` |

The second runs the block `a50(f)` is fitted from, unattended: `frequency points ×
depth points` recordings on **one lease**, so the drive cannot move between rungs.
It adds no new settings — the depth axis is `Sweep min a`/`max a`/`points` from
**Recording**, the frequency axis is `Sweep min f`/`max f`/`points`/order/seed from
the a₀ section — and reuses the ladder's ordering, reference repeats,
per-frequency confirmation and skip-and-report unchanged.

No `a₀` and no **Find a₀** are involved in either depth source: a depth sweep
commands and settles every `a` itself. Points are named `…_f<f>Hz_pNN`. A rung
counts as done only when its inner sweep recorded every point. See
[ADR 023](../../docs/adr/023-stage-a-a1-nested-depth-frequency-sweep.md).

## Bench conditions on every run

Every recording, in every mode, also records what the camera measures about itself (host
`CTX_SENSOR_MONITORING`): die **temperature** (°C), pixel **dead time / refractory period** (µs),
scene **illumination** (lux), the reading's age, and the absolute bias codes. They land in the
sidecar's `[sensor]` section and in both recorders' metadata as `sensor_*`.

Frozen when the recording starts (they drift), mirrored even with Live analysis off, and provenance
only — no result depends on them. A quantity the sensor cannot report is **omitted, never `0`**;
replay and cameras without a monitoring block produce no `[sensor]` section at all. See
[ADR 022](../../docs/adr/022-stage-a-a1-sensor-conditions-on-every-run.md).

The host also polls those quantities for the *whole* recording and writes a wide
`<raw-stem>.sensor-monitoring.csv` beside the RAW. A1 gathers it into the measurement folder as
`<stem>.sensor.json`, rewritten column-wise — one `{ t_us, value }` pair of arrays per channel,
carrying only the polls where that channel was read. The channels are sampled on different
schedules, so a row-per-poll table is padding by construction; the bias columns are dropped because
the camera's own bias sidecar already carries them. Named in the sidecar's `[files]` block as
`sensor_readout`. See
[ADR 028](../../docs/adr/028-stage-a-sensor-readout-travels-with-the-measurement.md).

Files share an `<id>_<timestamp>` stem: `<id>/<id>_<ts>.raw` (camera, under the host output root),
`<id>/<id>_<ts>_pd.pdq` + `.json` (photodiode, under its data root),
`<id>/<id>_<ts>.sensor.json` (sensor readout), and
`<id>/<id>_<ts>_config.toml` (A1, under the chosen folder). Point all three roots at the same
experiment directory to co-locate everything. The host also writes its own `<stem>.toml` next to the
RAW with the camera biases/ROI; the A1 sidecar cross-references it.

## Protocol — run a survey from a file

The four sweep buttons each move one axis and leave the others wherever they are. A **protocol**
names every axis for every recording instead, in a file that travels with the results. The reader
is chosen by extension.

### CSV — one row per recording (the one to reach for)

```csv
label,mean_u,frequency_hz,depth_a,duration_s,settle_s,role
floor,0.50,10,0.02,20,3,background
windows,0.50,10,2.00,20,3,pilot
ladder,0.40,1,0.80,40,4,
ladder,0.40,200,0.80,10,2,
```

| column | | |
|---|---|---|
| `mean_u` | required | normalized cycle-mean lobe point `ū` — the brightness (`I_k`) axis, 0.01–1.0 |
| `frequency_hz` | required | 0.01–2000 |
| `depth_a` | required | `a = ln(I_max/I_min)`, 0.01–6 |
| `duration_s` | optional, default 10 | seconds for **this** row, 1–3600 |
| `settle_s` | optional, default 2 | dwell after retargeting, 0–60 |
| `role` | optional, default `normal` | `normal`, `pilot` or `background` |
| `label` | optional | free text for the status line and sidecar; quote it if it contains a comma |

Columns are found **by name**, so their order does not matter and one can be left out entirely.
Blank lines and `#` comments are skipped, and a blank cell falls back to the default. Errors carry
the **file line number**, which is what your editor and spreadsheet both show.

Files saved by a spreadsheet load as-is: Windows line endings and the byte-order mark that Excel's
"CSV UTF-8" writes are both absorbed, so the first column is not silently reported missing.

Two things the row form gives you that blocks cannot without one block per value: **a different
duration per row** (1 Hz needs 40 s of cycles, 200 Hz does not), and **a `role` column**, so a file
can open with its own background floor and pilot and then record the points scored against them —
a complete measurement, not one that needs two button presses first.

### TOML — blocks and ranges

Kept for a dense regular sweep, which a 96-row CSV states badly:

```toml
[defaults]
duration_s = 10
settle_s   = 2.0

[[block]]
name         = "frequency-ladder"
mean_u       = [0.3, 0.6]
frequency_hz = { min = 1.0, max = 200.0, points = 6, spacing = "log" }
depth_a      = 0.8
duration_s   = 20
```

Each axis takes a single value, a list, or a `{ min, max, points }` range (`linear` default, `log`
for per-decade ladders); a block records the product of its three, `ū` outermost then `f` then `a`,
which settles the slow axis least often. `duration_s`/`settle_s` are per block.

### Either way

**`mean_u` is the `I_k` axis** — the normalized cycle-mean lobe point, driven by the new
`SetOperatingPoint` command, and the axis no button could sweep. All three axes are commanded at
every point and the point waits for all three acknowledgements before recording, so nothing is
filed under parameters the file does not state. The file's `duration_s` wins over the panel's. One
lease covers the whole run; a point whose drive the modulation owner refuses is skipped carrying
its wording, and the reasons are kept on the status pane and in the closing summary. The whole file
is validated on the button press, before the drive moves, and the point count and expected bench
time are reported first. Use **Stop** in the Record section to end a run early.

`protocols/example.csv` and `example.toml` are commented files to copy, installed to
`~/.augur/plugins/stage-a-a1/protocols/`. See
[ADR 027](../../docs/adr/027-stage-a-a1-declarative-protocols.md).

## How long will this take?

Every run long enough to walk away from — **Sweep a**, **Sweep f**, **Sweep a × f**, an `a₀` point
or a protocol — reports its remaining time on the button press and keeps reporting it on the status
pane:

```
Estimated time: ≈ 41 min left of ≈ 1 h 5 min — done by 03:41 UTC
```

The first number is the plan's own time (`Duration` + `Settle time` per point, or each protocol
row's own) plus a fixed allowance for the camera/photodiode handshake, and it says so:
*(from the plan until the first point finishes)*. After that, every finished point re-scales what
is left by the pace the bench is actually keeping — settling, handshakes, `a₀` trials and the
marker periods a new frequency has to be confirmed over. Only the outermost run states an estimate:
a ladder's already covers the depth sweep inside it. It is advisory — nothing is skipped or
shortened because of it. See
[ADR 034](../../docs/adr/034-a1-time-estimates-are-the-plan-corrected-by-the-measured-pace.md).

## Live quicklooks

- **Rolling half-period response** `S_p(t) = N_p(t−T/2, t] / N_valid` — events per valid pixel in the
  trailing half-cycle, ON and OFF. A live "are events appearing, is the ON/OFF timing sane?" check.
- **Response probability** `q_p` — fraction of valid pixel-cycles that fire at least once in the
  ON/OFF phase window (each pixel-cycle counts once, unlike `S_p`). The windows come from the row's
  **pilot** when one has been recorded (frozen and held across the row), otherwise auto-detected
  from the trigger-anchored fold (each grows out from its histogram peak to the window floor,
  default 10 % of peak). `Record pilot` / `Record background` (in the Recording section) capture the
  frozen windows and the floor `q0` into the measurement folder and are auto-reloaded when you
  return to that folder + id. Record one point per amplitude vs the photodiode-measured `a`. The
  authoritative `q_p(a, f)` fit is computed **offline** from the recordings; this is a quicklook.

The period `T` comes from the firmware phase-0 `EXT_TRIGGER` marker spacing (the trigger *defines*
the frequency), falling back to the modulation plugin's acknowledged waveform. The ROI and masked
pixels come from the augur-rs camera config.

See [docs/features/stage-a-a1.md](../../docs/features/stage-a-a1.md) for the full brief,
[ADR 009](../../docs/adr/009-stage-a-a1-recording-coordinator.md) for the coordinator design,
[docs/features/stage-a-a1-event-count.md](../../docs/features/stage-a-a1-event-count.md) plus
[ADR 013](../../docs/adr/013-stage-a-a1-event-count-depth-lock.md) for the `a₀` lock,
[ADR 014](../../docs/adr/014-stage-a-a1-frequency-ladder.md) for the unattended
frequency ladder,
[ADR 015](../../docs/adr/015-stage-a-a1-recording-robustness.md) for the
recording coordinator's one-folder/full-duration guarantees,
[ADR 017](../../docs/adr/017-stage-a-rail-detection-and-withheld-a-reasons.md)
for why an `a₀` gate refused and how Live analysis is distinguished from a
missing trigger, and
[docs/features/stage-a-a1-automation.md](../../docs/features/stage-a-a1-automation.md) for the
planned amplitude-sweep automation on top of this.
