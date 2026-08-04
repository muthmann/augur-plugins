# ADR 032 — Teensy port discovery is platform-aware and lives in `stage-a-io`

**Status:** accepted
**Date:** 2026-08-04
**Feature briefs:** [Stage-A Modulation](../features/stage-a-modulation.md), [Stage-A Photodiode](../features/stage-a-photodiode.md)

## Context

Both Stage-A owner plugins find the Teensy by enumerating serial ports and
probing the candidates: the modulation plugin opens each and keeps the one that
answers `HELLO` (the command port), the photodiode plugin listens on each and
keeps the one streaming CRC-clean PDA1 sample frames (the stream port). The
probe is what identifies the device; enumeration only decides what gets probed.

That candidate filter was written on a Mac and hard-coded the two Unix name
patterns:

```rust
name.contains("cu.usbmodem") || name.contains("ttyACM")
```

Windows names no device. Every serial port is `COMn`, so a correctly attached,
correctly driven Teensy matched neither pattern and was filtered out *before*
any probe could run. Both plugins then reported

```
no USB serial device found (looked for usbmodem/ttyACM)
```

which names the two things the machine cannot produce, so it reads as "nothing
is attached" when the device is in fact attached and enumerated. The first
Windows bundles from CI (ADR 030) made this reachable for the first time.

The filter existed in four places — `serial_ports()` and `port_variants()` in
each plugin — and had drifted into two implementations: modulation went through
`stage-a-io`, photodiode called `serialport` directly with `stage-a-io`'s
`hardware` feature switched off.

## Decision

**One platform-aware candidate filter, in `stage-a-io::transport`.**

`available_ports()` returns `PortInfo { name, label, is_usb }` — the OS path,
the USB manufacturer/product label where the OS reports one, and whether the OS
classified the port as USB at all. `candidate_ports()` narrows that list:

- **macOS** — `cu.usbmodem*`. Every device is listed twice (`tty.*` and `cu.*`)
  and only the callout node may be opened, so the name filter is also a dedupe.
- **Linux** — `ttyACM*`, the CDC-ACM class node a Teensy enumerates as.
- **Windows** — every USB-classified port. The name carries no device
  information, so USB-ness is the only signal available. If the OS classified
  *no* port as USB, the whole list is probed rather than none: a missing
  SetupAPI classification must not be able to hide the device the way the name
  filter did.

Both plugins call `candidate_ports()` for probing and `PortInfo::variant()` for
the settings picker, so the probed set and the listed set cannot disagree.
The photodiode crate enables `stage-a-io`'s `hardware` feature to reach it;
`serialport` was already a direct dependency there, so nothing new enters the
build.

**The filter is a probe-cost optimisation, not the identity check.** It exists
to keep the probes off unrelated ports — notably Windows' phantom Bluetooth
`COM` entries, which can block on open. Being too permissive costs a few
hundred milliseconds of probing; being too strict makes the hardware
unreachable. When in doubt, probe.

`no_candidate_ports_message()` replaces the fixed string with what the OS
actually enumerated, distinguishing "no serial ports found" from "no serial
port looked like a Teensy (the OS offered COM1 (Bluetooth), …)".

The platform branch is a `windows: bool` parameter to a private
`narrow_to_candidates`, not a `#[cfg]`, so the unit tests cover both branches
from any build host — including the Windows regression that motivated this ADR.

**Every port is opened with `dtr_on_open(true)`.** macOS and Linux assert DTR
when a tty is opened; Windows does not — `serialport` sets
`DTR_CONTROL_DISABLE` in the DCB. A Teensyduino sketch that gates its output on
`if (Serial)` (which is `usb_configuration && usb_cdc_line_rtsdtr`) therefore
stays silent on Windows even once the right port is found, and the probes would
report "no port streamed PDA1 sample frames" on a working device. Asserting DTR
on all three platforms makes the port behave the same everywhere; on macOS and
Linux it is a no-op.

## Consequences

- The Stage-A plugins connect on Windows: the ports are found, and the opened
  port has DTR asserted the way the Unix platforms already did implicitly.
- A port picker entry and a probe candidate come from one function, so a port
  that appears in the dropdown is one `auto` would also have found.
- The failure message names the enumerated ports, which is the difference
  between "check the cable" and "the filter dropped my device".
- `available_port_names()` and `available_ports_with_labels()` are replaced by
  `available_ports()`. Both were internal to this repository.
- Windows probes any non-USB port when the OS classifies nothing at all, which
  can add probe latency on a machine with legacy `COM` hardware. Accepted: an
  unreachable device is worse than a slow scan.

## Alternatives considered

**Match the Teensy by USB VID/PID (0x16C0).** The most precise filter, and it
would work identically on all three platforms. Rejected for now: it hard-codes
the vendor of one board revision into the discovery path, and the probes
already establish identity positively — a VID match that skipped probing would
still have to tell the two ports of the dual-serial device apart.

**Probe every enumerated port on every platform.** Simplest possible rule, and
correct. Rejected: on macOS it would open the `tty.*` twin of each device,
which blocks waiting for carrier detect, and on Windows it would sit on
phantom Bluetooth ports.

**Keep the filter in the plugins and add a Windows arm to each.** Rejected: it
was already four copies in two implementations, and the copy that broke was
the one that had drifted.
