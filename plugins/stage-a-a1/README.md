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

## Recording

- **Output folder** — where the A1 config sidecar is written (recommended shared experiment root).
- **Measurement id** — one per `(I_k, f)` pair; auto-generated default, editable, or press **New id**.
- **Duration (s)** — each recording auto-stops and finalizes after this.
- **Start recording** — starts camera RAW, then connects/leases the photodiode and starts PDQ;
  the timer begins after both acknowledge. It auto-finalizes PDQ first, camera second, then writes
  the sidecar. **Stop** saves the current recording early (and aborts a running sweep).
- **Start sweep** — records **Sweep points (count)** amplitudes spanning `[Sweep min a, Sweep max a]`
  (min > 0): per point it renews the modulation lease, issues `SetOpticalDepth`, waits for the
  measured `a` to hold the target for **Sweep settle (s)** (30 s cap, then records anyway), and runs
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
- **Clear a₀ lock table** — after changing the flux point, the calibration or `a₀` itself.

Frequency order, the interleaved low-frequency reference and the repeated blocks stay yours — every
point is one button press.

Files share an `<id>_<timestamp>` stem: `<id>/<id>_<ts>.raw` (camera, under the host output root),
`<id>/<id>_<ts>_pd.pdq` + `.json` (photodiode, under its data root), and
`<id>/<id>_<ts>_config.toml` (A1, under the chosen folder). Point all three roots at the same
experiment directory to co-locate everything. The host also writes its own `<stem>.toml` next to the
RAW with the camera biases/ROI; the A1 sidecar cross-references it.

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
[ADR 012](../../docs/adr/012-stage-a-a1-event-count-depth-lock.md) for the `a₀` lock, and
[docs/features/stage-a-a1-automation.md](../../docs/features/stage-a-a1-automation.md) for the
planned amplitude-sweep automation on top of this.
