# Stage-A Function Generator

Manual control of the Stage-A Pockels-cell drive for familiarisation with the
bench: pick a waveform (**sine**, **square**, **sawtooth**), a frequency, and a
DAC modulation depth, hit *Apply drive*, and watch the photodiode respond live.

## Why the amplitude is "measured", not set

`amplitude_dac` commands the *phase*-modulation depth of the Pockels cell. The
cell's voltage→transmission response is non-linear (≈ sin²), so the same DAC
excursion produces different optical amplitudes at different working points.
The plugin therefore always reports the **measured** optical log-contrast

```
a = ln(V_max / V_min)      (dark-corrected photodiode voltages)
```

computed by `stage-a-io`'s calibrated, clipping-guarded estimator — never a
value inferred from the commanded DAC codes.

## Ports

| Port | Behaviour |
|---|---|
| `mock` (default) | Runs the waveform-extended mock controller in-process: full command round trip, synthetic photodiode stream through a Pockels-like sin² transfer. Zero hardware, zero risk. |
| `auto` / explicit device | Real Teensy over USB serial. Firmware 0.2.0 has **no waveform backend** and rejects the drive fields (`unknown_config_field`); the plugin reports this clearly. Real drive control needs the future v2 DDS firmware (`stage-a-controller/docs/features/waveform-drive.md`), which is blocked on the hardware freeze. |

## Views and actions

- **FuncGen photodiode** — live decimated waveform (volts vs. ms).
- **Function generator** status table — state, firmware, waveform-backend
  capability, commanded drive, measured `a`, clipping, stream integrity.
- Actions on the status table: *Connect*, *Disconnect*, *Apply drive*,
  *Stop drive*.

## Safety model

Same contract as `stage-a-monitor`:

- serial/mock connections open only while the execution context is
  `LiveCapture` with effects allowed — replay can never drive hardware;
- waveform/frequency/amplitude are persistent *settings*, but nothing reaches
  the controller until the explicit *Apply drive* **action**;
- drives whose `center ± amplitude` leave the 0–4095 DAC range are refused
  locally before any command is sent;
- `process_frame()` only drains the bounded I/O worker queues;
- watchdog `!FAULT` notices from the controller are surfaced immediately.
