# Stage-A Bench Stack

- **Status:** Two persistent owners plus orchestrated experiment workflows (ADR 007)
- **Firmware:** `stage-a-controller` 0.4.0+ (Teensy 4.1 on Hermit V2r1, `USB_DUAL_SERIAL`)

## Current shape

The Teensy enumerates as **two** USB serial ports, and each is owned by exactly one plugin:

| Port | Content | Owner |
|---|---|---|
| command port (first) | v1 ASCII commands + PDA1 binary frames | [`stage-a-modulation`](./stage-a-modulation.md) |
| stream port (second) | free-running PDA1 `SamplesU16` frames, 20 kSa/s default | [`stage-a-photodiode`](./stage-a-photodiode.md) |

- **`stage-a-modulation`** — Manual/Calibrated operating-band selection plus five independent
  waveform modes under one hard DAC ceiling, transferred to J23 immediately; shows the resolved
  band and board-reported DAC code. Firmware output is set-and-hold; automation uses an explicit
  `SafeOff` operation.
- **`stage-a-photodiode`** — live readout of SMA5/pin 18/A4, raw or inverted to excitation power
  `I_exc = I_tot − I_pd` against a user-set reference.
- **`stage-a-io`** (shared non-plugin library) — PDA1 wire format, typed client with idempotent
  retries, bounded I/O worker, and a firmware-faithful mock (including the 0.3.0 `MOD` verb).
  The photodiode owner uses the parser/PDQ modules; experiment plugins may use
  hardware-free readers/analysis but never open the ports.
- **`stage-a-a1`** — orchestrates both owner services and camera recording; it
  never opens a Teensy port or writes PDQ directly. Architecture: ADR 007.

## History

The earlier commissioning stack (`stage-a-monitor`, `stage-a-funcgen`, old `stage-a-a1` — device
monitor with calibrated contrast, waveform familiarisation, and the A1 minimum-depth Bode sweep)
was removed on 2026-07-15 as too complex for the current bench stage (ADR 006). It remains in git
history; the new A1 implementation uses different statistics and host-routed
orchestration. Device-ownership and safety rules: ADR 005 as amended by ADR 006
and ADR 007.
