# Stage-A A1 Analysis

- **Crate:** `plugins/stage-a-a1` (`augur-plugin-stage-a-a1`)
- **Status:** Recording coordinator + live quicklooks + amplitude sweep + `a₀` lock
  + unattended frequency ladder
- **Design:** [ADR 009](../adr/009-stage-a-a1-recording-coordinator.md),
  [ADR 010](../adr/010-stage-a-a1-amplitude-sweep.md) (sweep + button
  press forwarding),
  [ADR 015](../adr/015-stage-a-a1-recording-robustness.md) (one folder, full
  duration, named failures),
  [ADR 014](../adr/014-stage-a-a1-frequency-ladder.md) (the unattended ladder),
  [ADR 013](../adr/013-stage-a-a1-event-count-depth-lock.md) (exact-event-count
  `a₀` lock),
  [ADR 017](../adr/017-stage-a-rail-detection-and-withheld-a-reasons.md) (a
  withheld `a` names its gate; Live analysis vs. trigger),
  [ADR 018](../adr/018-stage-a-a1-required-vs-optional-inputs.md) (the output
  folder is the only required input; every gate is asked before the drive moves;
  the panel speaks to the operator),
  [ADR 020](../adr/020-stage-a-a1-depth-source.md) (`a` comes from the
  photodiode or from the commanded drive, and every artefact says which),
  [ADR 021](../adr/021-stage-a-a1-no-search-for-a-commanded-depth.md) (no `a₀`
  search when `a` is the command; the ladder skips it),
  [ADR 022](../adr/022-stage-a-a1-sensor-conditions-on-every-run.md) (die
  temperature, pixel dead time and scene illumination on every run),
  [ADR 023](../adr/023-stage-a-a1-nested-depth-frequency-sweep.md) (the
  frequency ladder is an outer loop: a whole depth sweep per frequency gives the
  `q_p(a, f)` surface in one press),
  [ADR 027](../adr/027-stage-a-a1-declarative-protocols.md) (surveys are run
  from a file, and `I_k` becomes a sweepable axis),
  [ADR 028](../adr/028-stage-a-sensor-readout-travels-with-the-measurement.md)
  (the sensor readout travels with the measurement, column-wise),
  [ADR 029](../adr/029-stage-a-leases-are-renewed-against-the-granted-deadline.md)
  (a leased run heartbeats against the deadline the owner granted, so a point
  longer than the owner's TTL cap no longer loses the drive mid-recording),
  [ADR 034](../adr/034-a1-time-estimates-are-the-plan-corrected-by-the-measured-pace.md)
  (every long run says how much longer it has: the plan's own seconds, re-scaled
  by the pace the bench is actually keeping)
- **Automation roadmap:** [Stage-A A1 Automation](./stage-a-a1-automation.md)
- **Second workflow:** [Stage-A A1 Exact Event Count](./stage-a-a1-event-count.md)
  — hold one *measured* depth `a₀` across the frequency sweep

## Purpose

A1 has two jobs on the Stage-A bench, both deliberately thin:

1. **Recording coordinator.** One *Start recording* button records the camera
   **RAW** stream and the photodiode **PDQ** stream together for a fixed duration,
   grouped under a per-`(I_k, f)` measurement id, and writes an A1 **config
   sidecar** (`.toml`) linking them with everything needed to reproduce and
   analyse the run offline.
2. **Live sanity quicklooks.** The rolling half-period response `S_p(t)` and the
   response probability `q_p`, folded on the modulation period `T`.

A1 owns no hardware and never drives the Teensy. The optical drive is armed in the
modulation plugin; A1 only *reads* its published settings.

## The recording workflow

The experiment sweeps the modulation depth `a = ln(I_max/I_min)` at a fixed
illumination `I_k` and frequency `f`, taking several recordings per `(I_k, f)`
pair (a background `a≈0`, a bright pilot, then settled amplitudes). One
**measurement id = one `(I_k, f)` row**; every recording under it lands in the same
folder. A1 makes each recording one button press:

| Control | Meaning |
|---|---|
| **Depth `a` source** | where every depth-dependent path reads `a` from: the **photodiode** (measured, default) or the **modulation drive** (commanded, open loop) — see below (ADR 020) |
| Output folder | **the only required field**: where the A1 config sidecar is written (recommended shared experiment root) |
| Measurement id | one per `(I_k, f)` row; auto-generated default, editable, or press **New id**. Optional — a blank field is filled in on the first recording and written back, so the panel shows the id that was used (ADR 018) |
| Duration (s) | each recording auto-stops and finalizes after this; applies to every button |
| Settle time (s) | dwell the depth or frequency must hold after being retargeted, before the recording starts; ignored by **Record once** |
| Depth axis: min a / max a / points | the `a`-range **Sweep a** walks, stored in every sidecar |
| Frequency axis: min f / max f / points / order / seed / repeat-lowest | the `f`-ladder **Sweep f** walks: log-spaced, visit order and interleaved reference repeats |
| **Record once** | one recording with the light exactly as armed: start camera RAW → connect and lease photodiode → start PDQ → auto-stop and save both → sidecar. Nothing is retargeted |
| **Sweep a** | per point: lease the modulation owner → retarget the calibrated drive to `a_i` → settle → one recording (`…_pNN`) → next |
| **Sweep f** | one recording per frequency at the same depth — the exact-event-count workflow, see [its brief](./stage-a-a1-event-count.md) |
| **Sweep a × f** | the **`q_p(a, f)` surface**: the whole depth sweep at every frequency, on one lease — see [below](#the-q_pa-f-surface-in-one-press-adr-023) |
| Record pilot | records a bright reference (`…_pilot`) **and** freezes the ON/OFF windows for the row from the live signal |
| Record background | records an unmodulated reference (`…_background`) **and** captures the false-response floor `q0` |
| **Stop** | stops whatever is running — a recording, a sweep, a ladder or a protocol — at its next safe point, so the file in flight is still finished and saved |

All of it lives in **one Record section**. It used to be spread over three
(`Recording`, `Depth sweep at every frequency`, `Same depth at every
frequency`), each carrying part of the settings the others needed — so the
frequency axis was configured in the a₀ section and read by a button two
sections above it. **Live analysis** moved to the top of the panel for the same
reason: almost everything reads it.

The record and sweep buttons stay **disabled until an output folder is
selected**.

### Where `a` comes from (ADR 020)

`a = ln(I_max/I_min)` is a property of the light, so the photodiode measurement
is the default and the source of record. It is also **fail-closed**: the
photodiode publishes no `a` unless it can prove its estimator window covers
whole modulation cycles, which it does from the firmware phase-0 **marker
frames** on its own stream port. If those markers never arrive — no trigger, or
a firmware build that does not stamp them — it refuses forever, with a reason
that reads like a settings problem:

```
No stretch of samples covers two whole modulation cycles between triggers
(0 trigger(s) in the last 3446784 samples) — lower the frequency, or raise the
photodiode cache length
```

Zero markers in millions of samples is a missing marker stream, not a short
window, and no setting fixes it. **Depth `a` source** is the way past:

| Setting | `a` is | Needs | Verified against the light |
|---|---|---|---|
| `photodiode (measured)` — default | the photodiode's measured excitation log-contrast | phase-0 markers, a confirmed `I_tot` anchor, an unclipped window | yes |
| `modulation drive (commanded, open loop)` | the depth the modulation owner's calibrated drive is commanding (`optical_drive.depth_a_milli`) | an applied Pockels calibration and `OPTICAL_LOG_SINE` armed | **no** |

The commanded depth is still a *calibrated* number — the modulation plugin
inverts the measured `V_null` / `V_peak` curve to produce it — it is simply not
checked afterwards, so it carries the calibration's error plus any drift since.
It is not a datasheet value and it is not a DAC excursion: a manual DAC band or
a constant level publishes no optical drive, and the gates refuse rather than
inventing a depth.

Open loop there is **nothing to search for**, so `Find a₀` is not used at all and
the frequency ladder skips it — see [below](#a-and-the-a-ladder-adr-021). The
photodiode's window-length and clipping checks are skipped in this mode too,
because neither bounds a commanded depth; the operator's settle dwell still
applies.

**Every artefact says which source it used**: the sidecar's `depth_a_source` /
`depth_a`, `[a0_lock].depth_source`, the recorders' `depth_a_source` metadata,
the `depth_source` field in `a0_locks.json`, and the *a from* column of the a₀
lock view. `measured_a` keeps its narrow meaning — a number the photodiode
actually measured — so an open-loop run carries none, rather than carrying a
commanded value under that name. Runs destined for the final `q_p(a, f)` fit
should be photodiode-measured.

### `a₀` and the a₀ ladder (ADR 021)

`Find a₀` exists for one reason: the Pockels inversion is measured once and is
therefore **static**, while the depth the cell delivers **rolls off with
frequency**. Holding one *measured* `a₀` across a ladder means re-finding the
commanded depth that produces it at each frequency
(`a_cmd ← a_cmd · a₀/a_measured`). That is real work — and it only exists for a
measured depth.

With the **commanded** source the loop measures the number it commands, so the
correction ratio is exactly 1. A search would command `a₀`, read back `a₀`, stop,
and store one identical row per frequency. So it is not run:

| | photodiode (measured) | modulation drive (commanded) |
|---|---|---|
| `Find a₀` | trims the depth per frequency, stores a lock | **not needed** — disabled, and says so |
| `Record a₀ point` | replays the stored converged lock | commands `a₀` directly |
| Ladder per rung | lease → set `f` → **confirm via camera markers** → search → record | lease → set `f` → **confirm via the modulation owner's ACK** → record |
| `a0_locks.json` | one row per frequency | untouched — nothing was found |
| Needs camera EXT_TRIGGER + Live analysis | **yes** | no |

The ladder's frequency check follows the same logic. Measured mode holds out for
the camera's phase-0 markers, because they define the period *and* anchor the
fold the point is scored in. Commanded mode asks the modulation owner instead —
the same owner, and the same acknowledged state, it already trusts for `a`. The
cost is confined to the live quicklook (the `q_p` fold goes free-running without
markers); the recorded RAW and PDQ that the offline fit reads are unaffected.

**Net effect:** with the commanded source the ladder runs on a bench with no
photodiode `a`, no camera trigger and Live analysis off — set `a₀`, press
*Record all frequencies*. The trade is that nothing verifies the light reached
`a₀` at each frequency, and the roll-off the search corrects is real, so switch
back to the photodiode once its markers work.

### The `q_p(a, f)` surface in one press (ADR 023)

The frequency ladder is an **outer loop**, and what it records at each rung is a
mode:

| button | per frequency | produces |
|---|---|---|
| *Record all frequencies* | one event-count point at `a₀` | `q_p(a₀, f)` — the same depth everywhere |
| **Record depth sweep at every frequency** | the **whole** `[min_a, max_a]` sweep | `q_p(a, f)` — one response curve per `f` |

The second is the experiment `a50(f)` is fitted from, and it was previously a
manual loop: set `f`, press *Record depth sweep*, wait, repeat. It now runs
unattended as `frequency points × depth points` recordings **on a single lease**,
so the operator's drive settings stay locked out from the first frequency to the
last instead of being re-applied between blocks.

It adds **no new settings**. The depth axis is the Recording section
(`Sweep min a` / `max a` / `points` / settle / duration); the frequency axis is
the ladder in the a₀ section (`Sweep min f` / `max f` / `points` / order / seed /
reference repeats). Ordering, the interleaved low-frequency reference,
per-frequency confirmation, skip-and-report and the summary are all the
unchanged ADR 014 machinery.

**No `a₀` and no `Find a₀` are involved at any point**, in either depth source —
a depth sweep commands and settles every `a` in its range itself, so there is
nothing for a lock to contribute. With the photodiode source each point is still
fully closed-loop against the measured `a`; it simply has no `a₀`.

Points are named `…_f<f>Hz_pNN`, so the surface sorts by frequency and then by
depth. A frequency whose curve cannot be recorded is skipped and named in the
summary rather than stopping the block — and a rung counts as done only when its
inner sweep recorded *every* point, not when its last recording happened to
succeed.

### Protocol — a survey from a file (ADR 027)

The four sweep buttons each move one axis and leave the others wherever they
are. That is right for exploring and wrong for a survey: `I_k` could not be
swept at all, and what a block recorded lived in the panel rather than in
anything that travels with the results.

A **protocol** is a file naming every axis for every recording. The reader is
chosen by extension, and both produce the same flat list of points. Both
tolerate what a spreadsheet writes: CRLF line endings, and the UTF-8
byte-order mark Excel's "CSV UTF-8" prepends — unstripped, the BOM becomes
part of the first header cell and the file is refused for missing a column it
visibly has.

**CSV — one row per recording**, and the one to reach for: it opens in a
spreadsheet, comes straight out of a script, and each row carries its own
duration.

```csv
label,mean_u,frequency_hz,depth_a,duration_s,settle_s,role
floor,0.50,10,0.02,20,3,background
windows,0.50,10,2.00,20,3,pilot
ladder,0.40,1,0.80,40,4,
ladder,0.40,200,0.80,10,2,
```

Required: `mean_u`, `frequency_hz`, `depth_a`. Optional: `duration_s`
(default 10), `settle_s` (default 2), `role` (`normal`/`pilot`/`background`),
`label`. Columns are located by header name, `#` comments and blank lines are
skipped, a blank cell falls back to the default, and an error names the file
line number.

Two capabilities follow from the row form:

- **A different duration per recording** — a 1 Hz point needs 40 s of cycles
  and a 200 Hz point does not.
- **A `role` column**, so a file carries its own background floor and pilot and
  then the points scored against them: a complete measurement rather than one
  that needs two button presses first.

**TOML — blocks and ranges**, kept for a dense regular sweep:

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

Each axis takes a single value, an explicit list, or a `{ min, max, points }`
range with `linear` (default) or `log` spacing; a block records the product of
its three.

- **`mean_u` is the `I_k` axis** — the normalized cycle-mean lobe point, driven
  by the new `ModulationCommandV1::SetOperatingPoint`. Dimensionless, not
  physical flux, but the one control that moves the mean illumination without
  touching the depth.
- **Points run `ū` outermost, then `f`, then `a`** — the order of how expensive
  each change is to settle. Any other nesting spends the run settling.
- **All three axes are commanded at every point,** and the point waits for all
  three acknowledgements before recording. A point that inherited an axis from
  its predecessor would be recorded under parameters the file does not name.
- **The file's `duration_s` wins** over the panel's, or the survey would not be
  reproducible from the protocol alone.
- **Validated up front**: ranges, bounds, the `MAX_POINTS = 4096` product limit
  and the same whole-cycle window check the ladder makes against its lowest
  frequency — all on the button press, before the drive moves. The point count
  and expected bench time are reported first.
- **A refused point is skipped, not fatal**, carrying the modulation owner's own
  wording. Because the per-point message is overwritten within the same tick,
  the reasons are kept on the run and shown in the status pane and the closing
  summary.
- **Every row names itself, in the file names and in both sidecars**
  ([ADR 033](../adr/033-a-protocol-row-names-itself.md)). A protocol is the one
  path where nothing in the panel is armed at anything that separates its
  recordings, so the files carry the row's own identity:

  | where | what |
  |---|---|
  | file stem | `_p<NN>_u<ū>m_f<f>Hz_a<a>m` after the timestamp — e.g. `A1-survey_20260805-141233_p03_u500m_f10Hz_a800m` |
  | A1 `_config.toml` | a `[protocol]` section: `name`, `block`, `point_index`/`point_total`, `point_tag`, `requested_mean_u`, `requested_frequency_hz`, `requested_depth_a`, `settle_s`, `duration_s` |
  | `_pd.json` / camera metadata | `protocol_name`, `protocol_block`, `protocol_point_index`, `protocol_point_total`, `protocol_point_tag`, `protocol_mean_u`, `protocol_frequency_hz`, `protocol_depth_a`, `protocol_settle_s` |

  The index is 1-based over the whole expanded protocol and comes first, so the
  files sort in protocol order rather than by `ū`. What the row *asked* for is
  written here; what the drive reported back is in `[modulation]`, and what the
  photodiode measured is in `[optical]`, so a row that missed its point can
  still be told apart from one that hit it.

One lease covers the whole file. `plugins/stage-a-a1/protocols/example.toml` is
a commented file to copy.

### How long will this take? (ADR 034)

Every run long enough to walk away from — the depth sweep, either mode of the
frequency ladder, an `a₀` point, a protocol — states its remaining time on the
button press and then keeps stating it, in one status line:

```
Estimated time: ≈ 41 min left of ≈ 1 h 5 min — done by 03:41 UTC
```

- **The plan is the starting point, the bench is the correction.** The first
  number is what the run asks for (`duration_s + settle_s` per point, per row
  for a protocol) plus a fixed allowance for the start/finalize handshake, and
  it says of itself *(from the plan until the first point finishes)*. From the
  first finished point on, the estimate is re-scaled by the pace the run is
  actually keeping — settling, handshakes, `a₀` trials and all — so a survey
  running at half speed says so within one point instead of in the morning.
- **A ladder rung** is costed as the frequency retarget, the trigger
  confirmation (four marker periods, so the low frequencies dominate), the `a₀`
  lock where one runs at all, and then whatever the rung records.
- **One line, for the run the operator started.** A ladder's estimate already
  covers the inner depth sweep it is currently running, and a protocol covers
  both.
- **Skipped points count**, the estimate counts down inside the point in
  flight, and the finish clock (shown above ten minutes) is UTC like every
  filename this plugin writes.

It is advisory only: nothing is skipped, shortened or refused because of an
estimate. It is deliberately *not* the lease TTL, which is a worst case that
must cover every remaining settle timeout and would quote half an hour for a
five-minute sweep.

### Bench conditions on every run (ADR 022)

Every recording — normal, pilot, background, sweep point, a₀ point — also
records what the camera measures about itself, from the host's
`CTX_SENSOR_MONITORING`:

| quantity | sidecar `[sensor]` | recorder metadata |
|---|---|---|
| die temperature, °C | `temperature_c` | `sensor_temperature_c` |
| pixel dead time (refractory period), µs | `pixel_dead_time_us` | `sensor_pixel_dead_time_us` |
| scene illumination, lux | `illumination_lux` | `sensor_illumination_lux` |
| staleness of the reading, s | `reading_age_s` | `sensor_reading_age_s` |
| absolute bias codes | `bias_diff_on/_off/_fo/_hpf/_refr` | — |

All three bear directly on `q_p(a, f)`: the dead time caps events per pixel per
half-cycle, the lux *is* the physical `I_k` axis, and temperature moves the
biases. The values are **frozen when the recording starts** (they drift, and the
sidecar is written at finalize), mirrored even with Live analysis off, and are
**provenance only** — no A1 result depends on them, or a live run would disagree
with an offline re-run of the same data. A quantity the sensor cannot report is
**omitted**, never written as `0`; replay and cameras without a monitoring block
produce no `[sensor]` section at all.

For manual recordings A1 never drives the Teensy: set the drive (high `a` for
the pilot, `a≈0` for the background) in the modulation plugin, then press the
matching button — the recording captures whatever `a` is currently set.

**The sweep is the one scoped exception.** Start sweep leases the modulation
owner (`SERVICE_STAGE_A_MODULATION_CONTROL_V1`) and, per point, issues
`ModulationCommandV1::SetOpticalDepth` — which only retargets the *depth* of the
drive the operator already armed (frequency, normalized cycle mean `ū`, and
calibration stay untouched). The modulation owner accepts this command only
with an applied measured calibration and `OPTICAL_LOG_SINE`; manual, constant,
DAC-sine, square, and optical-linear modes are rejected. It renews the lease per
point *and* on a heartbeat between points, waits for a fresh, marker-bounded photodiode `a` from a confirmed `I_tot`
anchor to settle, hands the point to the normal recording
coordinator, and releases the lease at the end or on abort. Sweep points
require `min a > 0` — record `a≈0` with the background button instead. Sidecars
of sweep recordings additionally carry `sweep.requested_a`, `sweep.point_index`
and `sweep.point_total`. After the sweep releases the lease, the drive holds the
last sweep amplitude until the operator's own `depth a` setting is re-applied
(any modulation settings change re-sends it) — which is exactly why an
event-count point re-applies its locked depth under the lease instead of trusting
the drive to still be where a previous action left it (ADR 013).

**Leases are kept alive against the deadline the owner granted, not the one A1
asked for** (ADR 029). Both owners cap the TTL they hand out — a client that
dies must not hold the laser — so the whole-run TTL a sweep, a ladder or a
protocol asks for is *not* what it gets. A1 reads the real
`expires_at_unix_ms` off the owner's own snapshot and renews on a heartbeat once
less than 20 s of the granted window is left. Without it, any point longer than
the cap outlived its lease mid-recording and the owner did what an expired lease
must do — `STOP`, output off — which then read as three separate faults at once:
`the modulation owner requires an active automation lease`, `cannot write a
quantitative A1 sidecar without a fresh photodiode optical summary`, and a
`Camera: … no trigger signal` line that looked exactly like an unplugged
`EXT_TRIGGER` cable but was the drive being off.

**Naming.** Files share an `<id>_<timestamp>[_role][_point]` stem under an `<id>/`
subfolder:

- `<id>/<id>_<ts>.raw` — camera RAW, with the host's own `<stem>.toml` sidecar
  (camera biases, ROI) next to it.
- `<id>/<id>_<ts>_pd.pdq` + `_pd.json` — photodiode PDQ + sidecar.
- `<id>/<id>_<ts>_config.toml` — the A1 sidecar.

The optional tags say which run within the measurement this is — without one, a
timestamp is the only thing separating two files:

| tag | written by |
|---|---|
| `_pilot` / `_background` | the reference runs |
| `_p<NN>` | an amplitude-sweep point (prefixed `_f<f>Hz` when nested in the frequency ladder) |
| `_ec_f<f>Hz` | an event-count point |
| `_p<NN>_u<ū>m_f<f>Hz_a<a>m` | a protocol row (ADR 033) |

**Everything lands under `<A1 output folder>/<id>/`** (ADR 015). That folder is
the only setting deciding where a measurement ends up — the host output root and
the photodiode Data directory no longer have to be kept aligned by hand:

- **The PDQ and its sidecar are written there directly.** A1 names the
  destination root in the start spec (`PdqStartSpecV1::root_dir`), which replaces
  the photodiode's own Data directory for that run. An A1-driven recording
  therefore does not depend on the photodiode's folder setting at all.
- **The camera RAW and the host's bias `.toml` are moved there after
  finalization.** The host resolves plugin recording paths below *its* output
  directory and rejects absolute ones, so A1 cannot name the destination up
  front; instead it gathers the file once the host reports it closed and hashed.
  A rename on one volume, a size-verified copy across volumes. A file that cannot
  be moved stays where it is and the sidecar points at it there.

- **The host's sensor telemetry is compacted in on the way.** The host writes a
  wide `<raw-stem>.sensor-monitoring.csv` beside the RAW; A1 rewrites it
  column-wise as `<stem>.sensor.json` in the measurement folder and removes the
  original. One `{ t_us, value }` pair of arrays per channel, carrying only the
  polls where that channel was actually read — the channels sample on different
  schedules, so a row-per-poll table is padding by construction. Bias codes are
  dropped: the camera's own bias sidecar already carries them. Nothing is
  resampled or aligned, failed polls are kept as `faults`, and the whole path is
  best-effort (ADR 028).

  **The companion CSV only exists if the host is asked for it.** It is governed
  by the host's own **Record sensor monitoring** checkbox in the recording
  panel, which A1 cannot set and cannot query — so no telemetry file means no
  `.sensor.json`, whatever the camera supports. That switch used to reset to off
  on every app start, which is how a survey could record forty runs and keep the
  bench conditions of none of them; it is now persisted across restarts
  (augur-rs). A1 reports it either way: when a finished run wrote no readout,
  the panel names the switch rather than leaving the absence silent. The
  single-point die temperature / dead time / illumination in `[sensor]` come
  from the context bus and are recorded with every run regardless (ADR 022).

**A1 config sidecar** captures: `measurement_id`, file
stem, role, start/finalize
timestamps, duration; the sweep `[min_a, max_a]`; modulation settings from the
acknowledged snapshot (frequency, center/amplitude DAC, waveform, transfer
`calibration_id`, optical target, requested and resolved normalized mean `ū`,
internal `u_g`/`u_c`, requested `a`, `V_null` and `V_peak`); the depth this run
was driven and judged by with its provenance (`depth_a`, `depth_a_source`); the
photodiode-measured `a`, extrema, geometric pedestal,
headroom, clip fractions, ADC id, and the learned `I_tot` anchor with its
provenance (ADR 024); ROI +
masked-pixel count + `N_valid`; the `[sensor]` bench conditions (die temperature,
pixel dead time, illumination — ADR 022);
trigger info (marker-anchored, marker count,
measured period); and the resolved paths of the RAW (+ its camera-config
sidecar), the PDQ (+ its sidecar) and the `sensor_readout`. The **pilot** run
additionally records the frozen ON/OFF windows and the **background** run the
floor `q0`, so returning to a measurement (folder + id) auto-reloads them for
the `q_p` plot.

**Mechanism.** A small control-plane state machine in `process_control` starts
the host camera recorder first and waits for its receipt. Only after the host
has completed the Preview → Recording switch does A1 connect and lease the
photodiode and open the PDQ with the same run id. The duration begins when the
PDQ start receipt arrives, so setup time is never deducted from the requested
recording. On completion A1 atomically finalizes the PDQ and releases its lease
while camera effects are still live, then stops the host recorder, waits for its
final receipt, and writes the config sidecar. A recording is successful only
when the host receipt is complete and the photodiode returns a valid finalized
receipt with both PDQ paths. The status panel shows only the current phase and
one concise result or error message; it does not render an internal event log.
A1 declares `host_commands = ["start_recording", "stop_recording"]` in its
manifest. Every role uses this same lifecycle.

**When something is wrong** (ADR 015):

- **Before the camera starts**, A1 refuses the recording — writing nothing — if
  the photodiode is not reporting status, is not connected, or is leased by
  someone else. The same hint fills the status `message` cell while idle, so it
  is visible before the button is pressed. (The photodiode's *Data directory* is
  deliberately not among these: A1 supplies the destination itself.)
- **If the photodiode fails once the camera is running**, the camera keeps
  recording for the full requested duration and closes normally. The run is
  marked camera-only: `recording_completed_ok` stays false (so a sweep stops),
  but the RAW is complete rather than a truncated stub.
- **The first, most specific failure is what you see.** The closing message is
  `Recording <id> incomplete: <cause> — metadata saved to <path>`; later fallout
  cannot overwrite the original cause.
- **Starting and stopping the host recorder restarts the capture pipeline**, which
  the host reports as a `SourceChanged` discontinuity — twice per recording. While
  a recording or sweep is in flight that boundary resets only the event fold, not
  the row's pilot windows, background floor, or collected response points.

**Host-side note.** The camera RAW leg restarts the host pipeline into
Recording mode and stops it again at finalize. After the file is finalized, the
host restores Preview before returning the receipt, so a sweep or another button
press can start the next recording automatically.

**File locations.** One place: `<A1 output folder>/<id>/` holds `<stem>.raw`
(+ the host's `<stem>.toml`), `<stem>_pd.pdq` + `<stem>_pd.json`, and
`<stem>_config.toml`.

**The PDQ says how the drive was shaped, not only how fast it ran** (ADR 033).
The photodiode's file is the photodiode owner's own, and travels on its own — a
trace whose sidecar names a frequency and two DAC codes cannot say which lobe,
or which operating point `ū`, produced it. Alongside `modulation_frequency_hz`,
`center_dac`, `amplitude_dac`, `depth_a`/`depth_a_source` and `measured_a`, the
recorder metadata therefore also carries `modulation_waveform`,
`pockels_calibration_id`, `optical_target`, `requested_mean_u`,
`resolved_mean_u`, `commanded_a`, `v_null_dac` and `v_peak_dac` — all of it
already published by the modulation owner, and all of it also in the A1
sidecar's `[modulation]`.

> The A1 `_config.toml` is the quantitative record and is **refused outright**
> without a fresh photodiode optical summary, which is what an expired lease or
> a drive that was switched off looks like after the fact (ADR 029). The run
> then reports `Recording <id> finished, metadata save failed: …` and only the
> `_pd.json` remains — which is the other reason it has to be self-describing.

## The two live plots

Both fold the camera event stream on `T` (from the firmware phase-0 `EXT_TRIGGER`
marker spacing, which *defines* the frequency; the modulation acknowledged waveform
is the only fallback). Enable **Live analysis** to keep them updating.

With **Live analysis** off nothing is ingested at all — no events *and* no
phase-0 markers — so the status line says so by name rather than reporting
`0 events; free-running (no EXT_TRIGGER)`, which reads as a wiring fault. The
frequency ladder refuses on the marker count and distinguishes the two cases in
its message (ADR 017).

Marker hygiene: preview windows overlap, so the same trigger edge arrives on
several consecutive frames — the marker buffer is sorted and deduplicated on
every merge (duplicates used to fail marker validation and blank the plots).
When marker validation still rejects a fold (dropped-trigger jitter), the
quicklook falls back to the free-running fold on `T` instead of going empty.

1. **Rolling half-period response**

   ```math
   S_p(t) = \frac{N_p(t-T/2,\,t]}{N_\text{valid}}
   ```

   events per valid pixel in the trailing half-cycle, ON and OFF. A live indicator:
   are events appearing, does the ON/OFF timing look sane, is the response
   saturating? It counts *every* event in the ROI, so a noisy pixel weighs heavily
   — it is a quicklook, not the response metric.

   `N_valid` is **ROI area minus masked pixels**, the same denominator `q_p` uses,
   and the numerator counts only events inside that same region. The two are shown
   side by side and have to mean the same thing; normalising `S_p` over the whole
   sensor under-reported it by the ROI/frame ratio while counting events from
   outside the ROI.

2. **Response probability** `q_p`

   ```math
   z_{i,c,p} = \mathbf{1}[\text{pixel } i \text{ fires in } W_p \text{ during cycle } c],
   \qquad
   \hat q_p(a,f) = \frac{1}{N_\text{valid} M}\sum_i\sum_c z_{i,c,p}
   ```

   the fraction of valid pixel-cycles that fire at least once in the ON/OFF phase
   window `W_p` — each pixel-cycle counts **once** (unlike `S_p`). The windows come
   from the row's **pilot** when one has been recorded (frozen, held across the
   whole row), otherwise from the trigger-anchored fold automatically: since the
   `EXT_TRIGGER` fixes the phase, ON and OFF live in opposite half-cycles, so each
   window is anchored on its histogram peak and grown outward until events fall
   below the **window floor** (default 10 % of the peak) or the opposite polarity
   takes over. `Record point` appends one `(measured a, q_on, q_off)` dot. The ROI
   and masked pixels come from the augur-rs camera config
   (`N_valid = |ROI| − |masked|`).

   **Why the pilot is per row.** The window phase depends on the event latency,
   which is a *phase* shift `τ·f` — negligible at low `f`, up to a full cycle at
   high `f` — and also drifts with `I_k`. So the windows must be defined **per
   `(I_k, f)` row** and held fixed across that row's `a`-sweep (re-deriving them
   per amplitude would bias the curve). One pilot per measurement id captures that
   exactly. This live `q_p` stays a quicklook; the **authoritative** `q_p(a, f)`
   fit (`a50`, background floor) is computed offline from the recordings.

## Button presses across the UI-mirror / live-worker split

The host loads two instances of every dynamic plugin: a **UI mirror** (renders
the settings, never touches hardware) and the **live worker** (runs
`process_frame` / `process_control`, owns the recording state machine). A
`SettingKind::Button` click calls `set_setting(key, true)` **on the mirror
only**; the worker receives settings through the host's snapshot, which carries
whatever `get_setting` returns. A1 therefore exports every button as a
**monotonic press counter** (`PressLatch`): the mirror increments it per click,
the snapshot transports it, and the worker treats a counter advance as exactly
one press edge (the first value a freshly loaded worker sees is adopted
silently, so reloads never replay old presses). This is why the record buttons
used to do nothing — the presses died on the mirror.

Related: A1 overrides `on_discontinuity` to ignore `SettingsChanged` (raised on
*every* settings sync of any plugin), so the response curve, pilot windows and
background floor survive ordinary UI interaction. Source changes and seeks reset
everything **unless** a recording or sweep is in flight, in which case the
boundary is A1's own pipeline restart and only the event fold resets (ADR 015).

## Where the inputs come from

| Input | Source |
|---|---|
| camera events, valid pixels | retained **EventStore** over a trailing analysis window; falls back to `frame.events()`, trimmed to the same window |
| phase-0 markers | rising `frame.external_triggers()` — the host **banks trigger edges from dropped preview frames** into the next processed frame (drain-to-newest and the preview throttle drop whole frames; at low modulation frequencies the survivors alone rarely held 2 markers inside the analysis window) |
| modulation period `T` | measured from the `EXT_TRIGGER` marker spacing; else the modulation plugin's acknowledged waveform — which, since the board-echo fallback, includes the **operator-armed UI drive**, not only service-path (leased) targets |
| optical modulation depth `a` | per the **Depth `a` source** setting (ADR 020). *Photodiode* (default): fresh optical summary (`measured_log_contrast`) from complete marker-bounded cycles and a confirmed `I_tot` anchor — always the *excitation* contrast, independent of display mode (ADR 012); when absent, `optical_unavailable` from the same snapshot carries the owner's refusal reason (ADR 017), and A1 appends the way past it. *Commanded*: the modulation owner's `optical_drive.depth_a_milli`, published only for a calibrated optical drive — open loop, tagged as such everywhere it is recorded |
| ROI, masked pixels | augur-rs camera config (`CTX_GLOBAL_SETTINGS`) |
| die temperature, pixel dead time, illumination, bias codes | host `CTX_SENSOR_MONITORING` (`SensorMonitoringV1`), mirrored every frame regardless of Live analysis and frozen at recording start. Provenance only — absent on replay, imports and cameras without a monitoring block (ADR 022) |

## Tests

`cargo test -p augur-plugin-stage-a-a1` covers trigger-defined period, marker-anchored
folding, ON/OFF separation of the rolling dataset, auto-window detection and the `q_p`
path, file-safe id generation, UTC timestamp formatting, the config-sidecar builder,
the pilot-window round-trip through the measurement folder, press-latch edge/baseline
semantics, the jittery-marker free-running fallback, sweep-point spacing, the
sweep-point sidecar fields, the ordered camera → PDQ → PDQ finalize → camera
finalize lifecycle (including envelope identity/revision and save location), the
selective discontinuity reset, and the `a₀`-lock and frequency-ladder sets listed
in the [exact-event-count brief](./stage-a-a1-event-count.md).

Three of them guard the recording defects fixed in ADR 015: a photodiode leg that
cannot start is refused before any host command is sent; a photodiode failure
mid-run keeps the camera recording for the full duration, names the cause in the
closing message, and still gathers the RAW and its bias sidecar into the
measurement folder; and a self-inflicted `SourceChanged` during a recording keeps
the row's response points and pilot windows while still resetting the event fold.

Four more cover the depth source (ADR 020): a withheld photodiode `a` keeps the
owner's own reason *and* names the setting that gets past it; the commanded
source reports a depth with no photodiode present at all, and refuses a drive
that is not a calibrated optical one; and both the recorder metadata and the
config sidecar carry `depth_a_source` on every run, with `measured_a` present
only when something actually measured it.

Three cover the simplified ladder (ADR 021): `Find a₀` refuses to search for a
depth it is commanding and takes no lease doing so; an a₀ point is armed with no
stored lock and `trials: 0`; and the whole ladder runs to `3/3 points recorded`
with no photodiode `a`, **no camera trigger markers** and an empty lock table,
panicking if it ever enters the search phase.

Two cover the bench conditions (ADR 022): the start-of-run snapshot wins over a
drifted live reading and reaches both the metadata and the sidecar's `[sensor]`
section; and a quantity the sensor cannot report is omitted rather than written
as a zero.

Two cover the nested sweep (ADR 023): the whole 2 × 3 block records every depth
at every frequency in depth order, on exactly **one** lease acquisition, never
entering the search phase and finishing with `2/2 frequencies × 3 depths`; and a
nested point's file stem carries both axes (`…_f50Hz_p03`).

Eight cover the time estimate (ADR 034): duration wording from seconds to hours;
an unmeasured run reporting the plan and still counting down inside the point in
flight; a finished point re-scaling what is left; the bounded pace correction;
elapsed measured from the run start; a protocol stating its bench time on the
button press *and* on the status pane, labelled as the plan; a sweep whose first
point ran at half speed doubling the four points left; and exactly one estimate —
the ladder's, not its inner sweep's — when both runs are live.
