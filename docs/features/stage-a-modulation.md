# Stage-A Modulation

- **Crate:** `plugins/stage-a-modulation` (`augur-plugin-stage-a-modulation`)
- **Firmware:** `stage-a-controller` 0.3.0+ (`MOD` capability), Teensy **command port**
- **Status:** Active (2026-07-15) — replaces `stage-a-funcgen` and the drive half of
  `stage-a-monitor`

## What it is

Laser-modulation control for the Stage-A bench with two orthogonal axes:

- **Drive method** defines the DAC operating band. `MANUAL` uses Power + Min threshold;
  `CALIBRATED` derives it from the lobe endpoints `V_null`/`V_peak`, the normalized cycle
  mean `ū`, and the optical depth `a`.
- **Mode** defines the shape that fills the band: `CONST`, `DAC_SINE`, `SQUARE`,
  `OPTICAL_LOG_SINE`, or `OPTICAL_LINEAR_SINE`. All five remain available under both methods.

The always-visible **max limit** is the hard DAC ceiling for every manual and calibrated drive.
The settings schema shows only the selected method's parameter block and refreshes when Method
changes; Manual is the default.

| Mode | Manual band `[min, power]` | Calibrated band from `ū`, `a`, `V_null`, `V_peak` |
|---|---|---|
| `CONST` | hold `power` | hold the DAC code for `ū` |
| `DAC_SINE` | DAC sine across the band | DAC sine across the band |
| `SQUARE` | DAC square across the band | DAC square across the band |
| `OPTICAL_LOG_SINE` | intensity log-sine across the band | mean `ū`, converted to `u_g=ū/I_0(a/2)` |
| `OPTICAL_LINEAR_SINE` | intensity linear-sine across the band | centre/mean `u_c=ū` |

Manual optical modes reuse the persisted `V_null`/`V_peak` lobe parameters and derive effective
`(u, a)` from the manual DAC band through the forward `sin²` transfer. Both optical modes then
use the same inversion path described in [Optical waveform drive](./stage-a-optical-waveform.md).
`ū` is dimensionless and must not be confused with physical cycle-mean A1 flux `I_k`.

`V_null`/`V_peak` are measured, not typed: the Calibration section sweeps settled `CONST` codes
against the photodiode and fits the lobe — see
[Pockels transfer calibration](./stage-a-pockels-calibration.md). Both are **absolute DAC codes**
an operator can point at on the transfer curve; the half-wave span between them is derived and
never entered, and `Vπ` no longer appears anywhere the operator sets something (ADR 025).

## Achievable ranges — settings clamp, they never refuse

`ū` and `a` are coupled through one constraint: the peak of the swing has to stay under the top of
the lobe and under the max limit. `waveform::PeakLaw` names how the peak follows from the two, one
variant per mode (`Constant`, `LogSwing` for the DAC sine/square, `LogSine`, `LinearSine`), and
solving it for one variable at a time gives the achievable range.

Edits **clamp into that range**; nothing reverts. Only the control the operator just touched is
limited — dragging `a` up means "more depth", so `a` is what stops and `ū` stays put — and a lobe,
ceiling or mode change settles brightness first, depth second. Modes and methods are always
accepted.

Both bounds are live in the control labels (`Optical depth a (0..1.37 at ū=0.50)`) and on the
status line, along with where the current drive actually peaks. An un-sendable drive is reported as
`drive not sent: …` rather than blocking the edit. Previously a leftover `a` made an optical mode
simply unselectable, with an error naming a control the operator was not editing — see
[ADR 025](../adr/025-stage-a-drive-settings-clamp-not-refuse.md).

Every accepted setting change is transferred to the Teensy **immediately** as one `MOD` command —
no Apply button, no experiment state machine. The panel shows the modulation and live DAC code the
board *reports* (`MOD` reply + 2 Hz `STATUS` poll), not merely the commanded values.

## Port discovery

`auto` opens every candidate port and keeps the one that answers `HELLO` — the probe, not the port
name, tells the command port from the photodiode stream port of the same dual-serial device. Which
ports are candidates is platform-specific and shared with the photodiode plugin through
`stage-a-io::transport::candidate_ports()`: `cu.usbmodem*` on macOS (the callout node only, since
every device is listed twice), `ttyACM*` on Linux, and every USB-classified `COMn` on Windows,
where the name carries no device information at all (ADR 032). The settings picker lists exactly
the same set with each port's USB label. When nothing qualifies, the error names the ports the OS
did enumerate.

## Contract

- Owns the Teensy **command port** exclusively (one owner per port, ADR 006). The photodiode
  stream port belongs to `stage-a-photodiode`.
- Uses `stage-a-io` (`StageAClient`, `Command`) for framing and idempotent retries; slider drags
  coalesce into a single pending command the device thread drains.
- **Frame-independent**: connecting is a checkbox setting and all serial I/O lives in a
  plugin-owned device thread, because the host only calls `process_frame()` while camera frames
  flow — bench control must work with no camera attached. `process_frame()` only disconnects
  defensively in replay mode.
- Firmware output is **set-and-hold** (`stage-a-controller` ADR 002): disconnecting does not stop
  the modulation. Manual Power at 0 drives 0 V; automation has an explicit `SafeOff` operation.
- Safety invariants enforced plugin-side: `min_level ≤ level ≤ max_level` for Manual and every
  resolved calibrated/optical peak must be `≤ max_level`; invalid drives are refused.
- Status and commanded summaries include Method and the resolved `(lo, hi, hold)` DAC band.
- `ModulationStateV1.optical_drive` publishes the exact resolved optical
  target, requested and resolved normalized mean `ū`, internal `u_g`/`u_c`,
  requested `a`, `V_null` and `V_peak` as an additive V1 field; A1 sidecars no
  longer have to infer these from DAC endpoints. `v_peak_dac` replaced the
  earlier `v_pi_dac`, and carries the absolute peak code rather than the span
  (ADR 016, ADR 025).
- `ModulationStateV1.optical_lobe` publishes the applied measured calibration
  independently of the currently armed mode and point. Protocol runners such as
  A2 use this field to command their own `mean_u` and `depth_a`; the operator
  does not prepare those points in the modulation UI.
- `mock` port runs the firmware-faithful `MockController` in-process for hardware-free tests.
- The workflow-owner service and `WaveformV1` automation path remain exact-waveform contracts and
  do not use the UI Drive method.
- **`SetOpticalDepth`** (ADR 010): under an automation lease the service can retarget the *depth*
  `a` through the same `drive_command()` builder as the UI path. It is accepted only with an
  applied transfer calibration and armed `OPTICAL_LOG_SINE`; no device link, manual/constant,
  DAC/square/linear modes, or an unidentified hand-entered lobe are refused. Used by the A1
  amplitude sweep.
- **Link watchdog**: the device thread exits after 5 consecutive serial failures (marking the
  device disconnected/faulted), and the control tick reaps a finished device thread and
  auto-reconnects with a 2 s backoff while `connect` stays requested. Previously a wedged or dead
  link silently swallowed every queued command — the UI kept accepting mode changes while the
  board held the old waveform.
- **`SetOperatingPoint`** (ADR 027): the leased counterpart for the *operating point* `ū` — the
  third axis, alongside depth and frequency, and the one that moves the mean illumination without
  touching the depth. Calibrated method only; the owner parks the operator's own `ū` on the first
  retarget and restores it when the lease ends. Used by the A1 protocol runner's `I_k` axis.
  Unlike an interactive edit it **refuses** rather than clamping: a protocol asked for a specific
  brightness, and quietly recording a different one would put the wrong `ū` in every sidecar.
- **The applied lobe crosses to the UI mirror** (ADR 026): "Apply to V_null / V_peak" used to do
  nothing, because the fit lives on the live worker while the settings snapshot is collected from
  the mirror — so the mirror's stale codes overwrote the applied ones on the next sync. The applied
  lobe is now published through a process-global generation the mirror adopts.
- **No protocol section.** The undocumented TOML `MOD`-step runner was removed; declarative
  recording protocols belong to the A1 plugin, which can also record what they produce
  ([ADR 027](../adr/027-stage-a-a1-declarative-protocols.md)).
- **Board-echo `acknowledged` fallback**: the published `ModulationStateV1.acknowledged` now falls
  back to a revision-0 target built from the board's `MOD`/`STATUS` echo (`mod_wave`, `mod_level`,
  `mod_min`, `mod_freq_mhz`) when no service-path acknowledgement exists. UI-driven drives never
  produce a service ACK, so consumers (A1's fallback modulation period) previously saw no waveform
  at all for the normal operator workflow. WARP (optical) echoes map to `Periodic` — the fallback's
  consumers only need the frequency.

## Verification

`cargo test -p augur-plugin-stage-a-modulation` covers method/mode enum index round-trips,
conditional settings blocks, method-resolved bands, manual optical-band inversion, hard-ceiling
rejection, immediate mock transfer, board-code echo, square drive, and owner-service fail-safe
behavior.
