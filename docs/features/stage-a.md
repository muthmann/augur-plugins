# Stage-A calibration plugins (`stage-a-io`, `stage-a-monitor`, `stage-a-a1`)

> Feature brief — first delivery of the Stage-A camera-calibration stack.
> Design source of truth: knowledge base
> `methodology/stage-a-control-software.md` and
> `methodology/camera-calibration.md` (A1 protocol).

## Architecture

```text
AugurRs generic host (camera, RAW, EXT_TRIGGER delivery, execution context — ABI v5)
        │
        ├── stage-a-monitor  — commissioning: live photodiode view, manual control
        ├── stage-a-funcgen  — familiarisation: manual waveform drive (see stage-a-funcgen.md)
        └── stage-a-a1       — A1 minimum-depth a_min(f) sweep
                 │  (exactly one armed plugin owns the device)
                 ▼
        stage-a-io (this repo, plain lib) ── USB serial ── Teensy stage-a-controller
```

AugurRs itself gains no Teensy or serial abstraction — device ownership
lives entirely in these removable plugins (ADR 005).

## Crates

| Crate | Role |
|---|---|
| `stage-a-io` | PDA1 wire protocol (fragmentation-tolerant, CRC-resyncing parser), v1 ASCII commands with idempotent sequence retries, bounded background I/O worker, `.pdq` writer, JSON run sidecar, calibrated clipping-guarded optical-contrast estimator, firmware-faithful mock controller (0.2.0 surface + opt-in v2 waveform extension) |
| `plugins/stage-a-monitor` | Live decimated waveform, live `a`, integrity status, gated manual CONFIG/START/STOP + expert drive modal |
| `plugins/stage-a-funcgen` | Manual waveform drive (sine/square/saw, frequency, DAC depth) with photodiode-measured `a`; in-process mock port for hardware-free familiarisation |
| `plugins/stage-a-a1` | Phase-locked detection (Rayleigh), hardware/software cycle fiducials, bisection + grid sweep, probit `a_min` fit with CI, hot-pixel mask, PDQ + sidecar + results export |

## Safety model

- Serial ports open only when `HostContext::execution()` reports
  `LiveCapture` **and** `effects_allowed` (host constructs this fail-closed;
  only the active live-capture worker qualifies). Replay can never re-arm
  hardware, even from a sidecar that contains a runnable setup.
- All hardware commands are host **actions**; persistent settings never
  start hardware after a reload.
- Any CRC error, frame-sequence gap, or ADC overrun invalidates the
  measurement point; invalid points are re-measured, never patched, and
  the run sidecar records the counters.
- The firmware watchdog (1.5 s) drops the controller to `SAFE_IDLE`
  independently of host-side cleanup.

## Statistics (A1)

Detection is a phase-uniformity test (background activity is uniform in
drive phase; signal is phase-locked), with the frequency-scan multiplicity
Bonferroni-charged when the software clock-skew lock substitutes for the
missing trigger cable. `a_min` is the fitted `N = 0.5` crossing of a
probit in `ln a` with a profile CI — not a raw bisection endpoint — and
`a` is always the photodiode-measured contrast. Details and rationale:
`plugins/stage-a-a1/README.md`.

## Verification

`cargo test` (38 tests): wire fragmentation/CRC-resync/overrun, retry
idempotency against the mock controller, worker round-trip + clean STOP,
estimator recovery/clipping/headroom guards, Rayleigh calibration on
uniform and locked phases, background-immunity, clock-skew recovery
(300 ppm), fiducial folding, probit fit recovery, sweep convergence to a
synthetic `a_min`, exhaustion/invalid-window handling, hot-pixel masking,
dataset/schema consistency.

## Known gaps

- Final Teensy DDS/DAC firmware is blocked on the hardware freeze; the
  sweep and the function generator run against the reserved waveform-drive
  protocol (`stage-a-controller/docs/features/waveform-drive.md`) and the
  waveform-extended mock meanwhile — firmware 0.2.0 rejects the drive
  fields with `unknown_config_field` (feature detection).
- Marker cycles are protocol-reserved but not yet emitted
  (`stage-a-controller/docs/features/a1-marker-cycles.md`).
- `stage-a-a2` / `stage-a-a3` plugins are not yet implemented; A2
  additionally requires the physical trigger cable.
