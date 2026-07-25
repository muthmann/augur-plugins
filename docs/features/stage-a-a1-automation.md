# Stage-A A1 Automation — Plan (partially implemented)

- **Crate:** `plugins/stage-a-a1` (`augur-plugin-stage-a-a1`)
- **Status:** **Partially built.** §1 (scoped A1→modulation control path), §2
  (settle detection), §3 (per-point recording) and the single-row core of §4
  (the amplitude loop) now exist as the **Start sweep** button — see
  [ADR 010](../adr/010-stage-a-a1-amplitude-sweep.md). Still open: scout phase,
  randomized point order, multi-`f`/multi-`I_k` rows, `UNIDENTIFIABLE` stop
  rule, and the offline `a50` fit (§5–§6).
- **Relates to:** [Stage-A A1 Analysis](./stage-a-a1.md),
  [Optical Waveform Drive](./stage-a-optical-waveform.md),
  [ADR 007](../adr/007-stage-a-owner-orchestration.md) (the earlier full
  orchestrator this deliberately re-adds in a *focused* form),
  [ADR 009](../adr/009-stage-a-a1-recording-coordinator.md) (the per-recording
  RAW + PDQ + sidecar coordinator, now built — see §3).

> **Update (2026-07-23):** the manual per-recording coordinator in §3 now exists
> (ADR 009): one *Start recording* button records camera RAW + photodiode PDQ +
> an A1 config sidecar per `(I_k, f)` measurement, over an operator-set duration.
>
> **Update (2026-07-23, later):** the single-row amplitude sweep now exists
> (ADR 010): *Start sweep* leases the modulation owner, retargets the armed
> drive per point via `SetOpticalDepth`, waits for the photodiode-measured `a`
> to settle (tolerance + dwell, 30 s cap), and records each point through the
> §3 coordinator with `sweep.requested_a` / `point_index` / `point_total` in
> the sidecar. Remaining below: scout/randomized order, multi-`f`/`I_k`
> iteration, the `UNIDENTIFIABLE` rule, and the `a50` fit.
>
> **Update (2026-07-25):** the *second* workflow — **exact event count**, holding
> one measured depth `a₀` across the frequency sweep — now has its own blocks
> (ADR 012, [brief](./stage-a-a1-event-count.md)): a closed-loop **Find a₀** per
> frequency (§1–§2 applied the other way round — measure, then correct the
> *commanded* depth) and a **Record a₀ point** that replays the locked depth under
> the lease through the §3 coordinator. The multi-`f` iteration below stays
> deliberately manual there: randomising the frequency order, interleaving a
> low-frequency reference and repeating independent blocks are scientific ordering
> decisions, so A1 exposes them as per-frequency button presses rather than one
> opaque run.

## Goal

Semi-automate the researcher's normal A1 workflow: for one illumination `I_k`
and frequency `f`, sweep the modulation depth `a` and record the response curve
`q̂_p(a,f)`, automatically starting/stopping/saving each recording with proper
naming and full parameters. Later, repeat over `f` and over `I_k`.

## Already in place (the foundation)

- Two live plots `r_{p,k}` and `S_p`, plus the response curve `q̂_p(a)`.
- Marker-anchored phase folding from the firmware **phase-0 EXT_TRIGGER**; the
  trigger **defines the frequency** (measured marker spacing); event-latency
  handling (marker shift + optional self-alignment).
- **Pilot capture** → frozen ON/OFF phase windows; manual "record point"
  appends `(measured a, q_on, q_off)` to the curve.
- **ROI + masked pixels** come from the host camera config (`GlobalSettings`);
  `N_valid = |ROI| − |masked|`.
- Photodiode-measured **`a`** (rejected-complement geometry) published and
  surfaced in A1.
- Optical drive with a **fixed operating point `I_k`** and swept `a`
  (`OPTICAL_LOG_SINE`/`OPTICAL_LINEAR_SINE`), power-capped.
- Events sourced exactly from the retained **EventStore** over a sliding window.

## To build (the automation)

### 1. A1 → modulation control path (scoped)
Re-introduce a *focused* control path (the contract + modulation plugin still
support it): acquire a modulation lease, set the optical drive
`(target, I_k, a, f, V_null, Vπ)`, start, stop, release. No full workflow zoo —
just set-amplitude / start / stop. Fixed `I_k`, only `a` varies within a curve.

### 2. Settle detection
Before collecting a point, wait until the photodiode confirms the optical
waveform has stabilised at the new `a` — e.g. the published `measured_a` is
within tolerance of the target and clip-free for a short dwell. Only then start
the counting window.

### 3. Per-point recording (proper naming + parameters)
For the pilot, background, and every amplitude point, orchestrate:
- host camera **RAW** recording (re-add the host recording commands),
- photodiode **PDQ** recording (`stage-a-photodiode` begin/finalize),
- a **config sidecar** with everything needed to reproduce/replay: `I_k`, `f`,
  requested + measured `a`, ON/OFF windows, ROI, masked pixels, `N_valid`, `M`,
  latency, biases, run/session ids, timestamps, settle/clip status.
- **Deterministic naming**: `<data_root>/A1/<date>/<session>/<f>/<point>-<a>...`.

### 4. Sweep state machine
`SAFE → BACKGROUND(a=0) → PILOT(high, freeze windows) → SCOUT(locate the
transition) → SWEEP(5–7 settled amplitudes spanning ~10–90 %, randomized or
alternating order) → NEXT_FREQUENCY → … → NEXT_ILLUMINATION`. Windows are frozen
from the pilot and **must not** be re-derived from measurement points.

### 5. Classification and stop rules
- `q̂_p(a,f) = (1/(N_valid·M)) Σ_i Σ_c z_{i,c,p}`, ON/OFF independent
  (already implemented for a single point — the sweep just repeats it).
- If the transition cannot reach ~90 % within the safe amplitude range, mark the
  frequency **`UNIDENTIFIABLE`** (do not keep increasing `a`).

### 6. Final fit (per curve)
Fit the background-floor logistic `p(a) = p0 + (1−p0)·logistic((a−a50)/slope)`
to get **`a50`** with a cycle/spatial-tile bootstrap interval; label quality
(VALIDATED / DEGRADED-no-background / UNVALIDATED-no-pilot). (This was the old
`response.rs`; re-add as the sweep's summary output.)

## Known hard problem (only if per-cycle cross-stream correlation is ever needed)

The camera trigger and the photodiode stream marker are the *same* firmware
phase-0 on two clocks, consumed **independently** today — nothing pairs cycle
*k* across the two streams, and nothing needs to. If a future metric correlates
per-cycle optical depth with per-cycle camera response, ordinal matching is
fragile (start offset, asymmetric drops, drift). The robust fix is a **cycle
counter** in the PD `MarkerPayload` plus a **distinctive fiducial pattern**
(e.g. a periodic marker cycle) visible in both streams to align on and detect
drops — see the controller's `a1-marker-cycles.md`.

## Open decisions to confirm at build time

- Amplitude list: explicit list vs. min/max/count range (randomized order).
- Mask source already resolved: host `GlobalSettings.masked_pixels`.
- Whether RAW+PDQ per point is always on or gated by a "record" toggle
  (user already asked for full RAW + PDQ + params per recording).
- Live is a quicklook; the **RAW/PDQ replay is authoritative** for the final fit.
