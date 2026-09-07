# ADR 041 — Separate A2 synchronized capture from optical timing qualification

- Date: 2026-09-06
- Status: Accepted in source; hardware qualification pending

## Context

A2's comparator preparation could reject correct firmware readback and used a rate
unsupported by the portable CONFIG sampler. The runner also issued START during a
continuous PD stream. Strong optical noise makes an online voltage threshold a poor
acquisition prerequisite. A shared electrical anchor and simultaneous raw PD samples
permit an independent optical-edge analysis after recording.

## Decision

Add explicit `timing_reference` (`comparator` default or `drive_sync`) and
`trigger_validation` (`strict` default or `offline_review`) to point protocols.
Production selects drive sync and offline review. It uses calibrated square endpoints,
J24 camera pulses and source-1 PD markers. Both recorders open during a quiet
interval before a new pulse train starts, avoiding an ambiguous whole-cycle offset.
It does not arm the comparator or fit t50.
Keep raw storage/stream and owner lifecycle failures fatal under both policies.
Record timing warnings, marker-loss counters and missing evidence without calling
those points qualified. Separate lease renewals from acquisition acknowledgements.

Extend version-1 service payloads with defaulted timing reference and optional marker
diagnostics/counts. Old JSON remains decodable; old firmware or owners supply no
DMA-clock evidence and cannot silently satisfy the new timing qualification.
A2 sidecars advance to schema 2. No PDA1 wire-format change is required.
Use DMA-cursor marker indexing in production firmware and expose its method via STATUS.

## Consequences

J24 wiring and matching firmware must be verified at the bench. Optical timing is
still measured from the concurrent emission PD after fitting the two device clocks.
The pulse's trailing edge cannot represent optical OFF. A separate dark/noise reference
cannot remove a later random noise realization. Low-SNR captures can remain useful,
but an unresolved optical time origin limits absolute latency and intrinsic jitter.
Existing comparator protocols and their stricter intent remain supported.

A2 mean_u retains its geometric-pedestal meaning. Matched production templates convert
cycle-mean targets into geometric pedestals explicitly, preserving old protocols.
Tests cover command grammar/readback, owner/mock prepare, waveform-off cleanup,
reply routing, malformed/empty/short receipts, low-SNR threshold diagnostics, raw marker
round trips, schedule coverage and DMA-clock arithmetic. Software tests do not
substitute for the end-to-end bench smoke or H4/H5 timing measurements.
