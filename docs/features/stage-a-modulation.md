# Stage-A Modulation

- **Crate:** `plugins/stage-a-modulation` (`augur-plugin-stage-a-modulation`)
- **Firmware:** `stage-a-controller` 0.3.0+ (`MOD` capability), Teensy **command port**
- **Status:** Active (2026-07-15) — replaces `stage-a-funcgen` and the drive half of
  `stage-a-monitor`

## What it is

Laser-modulation control for the Stage-A bench with two orthogonal axes:

- **Drive method** defines the DAC operating band. `MANUAL` uses Power + Min threshold;
  `CALIBRATED` derives it from `V_null`, `Vπ`, normalized cycle mean `ū`, and optical depth `a`.
- **Mode** defines the shape that fills the band: `CONST`, `DAC_SINE`, `SQUARE`,
  `OPTICAL_LOG_SINE`, or `OPTICAL_LINEAR_SINE`. All five remain available under both methods.

The always-visible **max limit** is the hard DAC ceiling for every manual and calibrated drive.
The settings schema shows only the selected method's parameter block and refreshes when Method
changes; Manual is the default.

| Mode | Manual band `[min, power]` | Calibrated band from `ū`, `a`, `V_null`, `Vπ` |
|---|---|---|
| `CONST` | hold `power` | hold the DAC code for `ū` |
| `DAC_SINE` | DAC sine across the band | DAC sine across the band |
| `SQUARE` | DAC square across the band | DAC square across the band |
| `OPTICAL_LOG_SINE` | intensity log-sine across the band | mean `ū`, converted to `u_g=ū/I_0(a/2)` |
| `OPTICAL_LINEAR_SINE` | intensity linear-sine across the band | centre/mean `u_c=ū` |

Manual optical modes reuse the persisted `V_null`/`Vπ` lobe parameters and derive effective
`(u, a)` from the manual DAC band through the forward `sin²` transfer. Both optical modes then
use the same inversion path described in [Optical waveform drive](./stage-a-optical-waveform.md).
`ū` is dimensionless and must not be confused with physical cycle-mean A1 flux `I_k`.

`V_null`/`Vπ` are measured, not typed: the Calibration section sweeps settled `CONST` codes
against the photodiode and fits the lobe — see
[Pockels transfer calibration](./stage-a-pockels-calibration.md).

Every accepted setting change is transferred to the Teensy **immediately** as one `MOD` command —
no Apply button, no experiment state machine. The panel shows the modulation and live DAC code the
board *reports* (`MOD` reply + 2 Hz `STATUS` poll), not merely the commanded values.

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
  requested `a`, `V_null`, and `Vπ` as an additive V1 field; A1 sidecars no
  longer have to infer these from DAC endpoints.
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
- **`protocol_run` forwarding**: the UI mirror records the request and the settings snapshot
  starts/stops the protocol on the live worker (which owns the device link); only value
  *transitions* act, so re-applied snapshots cannot restart a finished protocol.
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
