# Stage-A A1 — minimum-depth Bode calibration

Measures `a_min(f)`: the smallest optical log-contrast that still produces
phase-locked camera events, per drive frequency. `|H(f)| = C / a_min(f)`;
the knee of the curve is the pixel bandwidth `f_c(I)`, and the plateau of
`a_min` reads out the contrast quantum `C` (which seeds A3). Protocol
design: knowledge base `methodology/camera-calibration.md` (A1) and
`methodology/stage-a-control-software.md`.

## How it decides "events just appeared"

- **Detector — phase, not counts.** Background activity is uniform in
  drive phase; modulation events are phase-locked. Each measurement window
  is folded and tested with the **Rayleigh test**; background is discounted
  automatically instead of subtracting a drifting absolute rate.
- **Cycle fiducial.** With the phase-0 TTL wired into `EXT_TRIGGER`, the
  camera-clock edges from `frame.external_triggers()` mark each cycle.
  Without the cable, the drive frequency is **refined against the events**
  (Rayleigh-power scan over ±ppm around the commanded value — recovers the
  Teensy↔camera clock skew); the scan multiplicity is Bonferroni-charged
  to the significance threshold.
- **Estimator.** The mean phase-locked events per half-cycle comes from the
  positive excess over the median phase-bin occupancy.
- **a_min is a fitted crossing.** The 0→1 step is smeared by shot-noise
  first-passage randomness and per-pixel threshold dispersion, so a_min is
  the fitted `N = 0.5` crossing of a probit in `ln a`, with a profile
  confidence interval. The fitted transition width is a free preview of
  the smear (σ_C + FPT).
- **Hot pixels.** An unmodulated reference window at run start builds a
  median+5·MAD mask; masked pixels never enter the statistics, and the
  mask size is recorded in the sidecar.
- **`a` is measured light.** Every point's contrast comes from the
  photodiode ADC through the calibrated, clipping-guarded estimator in
  `stage-a-io` — never from the commanded DAC code. Invalid windows
  (clipping, CRC/sequence/overrun faults) are re-measured, never patched.

## Run flow

`Arm controller` → `Run A1 sweep`: reference window (hot-pixel mask) →
per frequency: bisection on the drive code until the detection boundary is
bracketed → log-spaced grid across the transition → probit fit →
next frequency. Views: `a_min(f)` with CI, live phase histogram, `N(a)`
staircase, run status. Raw PDA1 frames go to
`~/.augur/stage-a-runs/<run-id>.pdq` with a JSON sidecar and a results
export; final numbers must be recomputed from the camera RAW + PDQ.

ON and OFF are measured in **separate runs** (settings → Polarity) — the
comparator paths are asymmetric and must never be pooled.

## Safety

Fails closed on the ABI v5 execution context exactly like
`stage-a-monitor`: serial I/O only in the active live-capture worker;
Arm/Run/Stop are host actions, never settings; the firmware watchdog
drops to `SAFE_IDLE` independently of host cleanup.

## Current limitations

- The Teensy DDS/DAC firmware is still the ADC-only commissioning build —
  closed-loop sweeps run against the protocol but the final stimulus
  backend is blocked on the hardware freeze (see `stage-a-controller`).
- Marker cycles (periodic full-depth optical anchors) are specced for the
  firmware but not yet emitted; the software frequency lock covers the
  missing-trigger-cable case meanwhile.
- Measured sample cadence validation and the A5 refractory validity bound
  `2fa/C ≪ 1/τ_refr` are recorded, not yet enforced.
