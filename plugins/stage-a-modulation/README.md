# Stage-A Modulation

Controls the laser modulation input (Hermit J23, `DAC1.4`) through the Teensy **command port**
(the first of the two USB serial ports enumerated by `stage-a-controller` firmware 0.3.0+).

## What it does

- **Drive method** selects how the DAC operating band is defined:
  - `MANUAL`: **Power** is the peak/operating code and **Min threshold** is the lower endpoint.
  - `CALIBRATED`: `V_null`, `Vπ`, `I_k`, and optical depth `a` determine the endpoints.
    Measure `V_null`/`Vπ` with the built-in transfer sweep — see [Calibration](#calibration--measuring-v_null-and-vπ).
- **Mode** independently selects the waveform that fills that band. All five modes are available
  under both methods.
- **Max limit** is always visible and is the hard DAC ceiling for every drive.
- Every accepted change is sent to the Teensy **immediately** (one `MOD` command); there is no
  Apply button.
- The panel shows the modulation and live DAC code the **board reports** (from the `MOD` reply and
  a 2 Hz `STATUS` poll), plus the selected method and resolved DAC band.

| Mode | Manual band `[min, power]` | Calibrated band from `I_k`, `a`, `V_null`, `Vπ` |
|---|---|---|
| `CONST` | hold `power` | hold the DAC code for `I_k` |
| `DAC_SINE` | DAC sine across the band | DAC sine across the band |
| `SQUARE` | DAC square across the band | DAC square across the band |
| `OPTICAL_LOG_SINE` | optical log-sine across the band | optical log-sine about `I_k` |
| `OPTICAL_LINEAR_SINE` | optical linear-sine across the band | optical linear-sine about `I_k` |

Manual optical modes reuse the stored `V_null`/`Vπ` lobe and derive their effective `(I_k, a)`
from the slider band through the forward optical transfer.

In calibrated `CONST`, `a` is irrelevant: the hold is
`V_null + (2Vπ/π)·asin(sqrt(I_k))`. With `V_null=1630` and `Vπ=860`, this is
2490 at `I_k=1` and 1685 at `I_k=0.01`. Periodic modes still need optical
headroom and reject impossible `I_k`/`a` combinations without changing the
displayed setting or leaving it out of sync with the board.

## Calibration — measuring `V_null` and `Vπ`

Do not type these in from a datasheet. Static birefringence, alignment, PBS extinction, driver
gain, temperature, and the actual electrical load all enter the realised map, so measure them:

1. Connect the command port **and** the photodiode plugin (the sweep reads its published level;
   it needs no lease and takes no recording).
2. Set **Detector port**. Stage-A watches the PBS *reject* port, where the detector is
   **brightest** at `V_null` — the default. This cannot be inferred from the sweep: a bright and
   a dark extremum fit the measured curve equally well, and only the optics say which one is zero
   excitation. Getting it wrong puts `V_null` a quarter wave out.
3. Press **Measure transfer curve**. It steps settled `CONST` codes across `0..max limit`, up and
   back down (~20 s), and fits the lobe. Your armed drive is restored afterwards, on every exit
   path.
4. Read the result in the **Pockels transfer curve** view and the status line, then press
   **Apply to V_null / Vπ**. Apply refuses, naming the reason in the status line, while the
   residual exceeds 2 % of the detector span, the sweep covered less than three quarters of a
   lobe, or any point clipped.

The view also works *before* any measurement: it draws the lobe your current `V_null`/`Vπ` claim,
on a normalised axis, with markers at `V_null` and `V_null + Vπ`.

Two properties worth knowing:

- `V_null`/`Vπ` need **no** dark measurement and **no** total-power anchor — the fitted offset and
  amplitude absorb the dark level and the front-end gain.
- The detector level at the null is reported as a **lower bound** on the total-power anchor
  `I_tot`, *not* as the anchor. On the reject port the residual transmitted floor is not separable
  from it; freezing a real anchor needs a transmitted-port power measurement.

Set a **Calibration folder** to archive each applied calibration (points, fit, residual,
hysteresis) and stamp `calibration_id` into the state snapshot, so recordings can cite the
inversion they used. Full detail: [feature brief](../../docs/features/stage-a-pockels-calibration.md),
[ADR 011](../../docs/adr/011-stage-a-pockels-transfer-calibration.md).

## Connecting

- **Connect** is a checkbox in the plugin settings — it opens/closes the command port and works
  **without a running camera** (device I/O lives in a plugin-owned thread, independent of the
  host's frame-driven plugin passes). Connecting never changes the output; only changes made
  while connected are transferred.
- The firmware output is **set-and-hold**: disconnecting, closing the GUI, or a crash leaves the
  last modulation running (`stage-a-controller` ADR 002). In Manual mode, Power `0` drives `0 V`;
  automated workflows use their explicit `SafeOff` command.

## Ports

**Use `auto` (default recommendation):** it probes every attached usbmodem/ttyACM device and
connects to the one that answers `HELLO` — that is always the Teensy command port, never the
photodiode stream port. Explicit ports remain selectable; `mock` runs an in-process simulated
controller for hardware-free testing.

Replaying a recording disconnects the plugin defensively; live control itself needs no
capture session.

## Workflow-owner service

This plugin is the sole command-port owner for manual operation and automated Stage-A workflows.
The live-worker instance exposes `stage_a.modulation.control.v1` under the stable plugin ID
`stage-a.modulation`; UI-mirror and offline instances never open the port or apply hardware
effects. Automated clients acquire a renewable lease and submit semantic, idempotent commands
(`SetWaveform`, `PrepareA1`, `StartAcquisition`, `StopAcquisition`, `SafeOff`) rather than changing
UI settings or sending raw firmware strings. While leased, manual control settings are locked.

The bounded `stage_a.modulation_state.v1` snapshot keeps requested and board-acknowledged semantic
revisions separate. Lease expiry, replay/effects revocation, or owner shutdown during an automated
run performs a best-effort controller `STOP` followed by `MOD wave=OFF` before releasing the port.
Automation specifies exact waveforms and therefore does not use the UI Drive method.
