# ADR 048: Matched A4 bright reference with existing device owners

Date: 2026-09-10
Status: Accepted

## Problem

The legacy A4 threshold survey assumes manual constant light/PD capture and can
skip failed rows. The immediate experiment needs a background reference at the
completed A1–A3 camera state and attenuation without repeated manual setup.

## Decision

Add a strict `current_reference` TOML form and an embedded default program to A4.
Resolve its thresholds from the host-confirmed full baseline, preserving every
other camera setting. Use a small device coordinator inside A4 to lease the
existing modulation and PD owners; no new serial-device owner or host API is added.
Prepare the existing continuous A1 acquisition mode before any files are opened,
then set calibrated constant mean_u=0.30. Open RAW at the common root, start PD,
measure the declared interval, finalize PD, stop RAW, release devices safely,
and restore the camera session. Use fresh QueryRequest transport IDs for pending
modulation operations and revisions newer than both owners' preceding states.

Camera retries are bounded and stay on the same point. Device failures stop the
run rather than guessing whether an uncertain recording started. Recovery retains
unconfirmed cleanup state. Keep original telemetry, unique attempts and a durable
progress journal. The journal is not automatic process-restart resume.

## Consequences

The matched bright reference needs no per-point operator input. The AOD/laser
remain external controls, and camera lux is retained without absolute-radiometry
requirements. Legacy threshold protocols keep their external-light semantics.

This delivers the focused A4 requirement without first implementing a universal
A1–A6 controller. Future common orchestration should reuse or extract this code
rather than adding another device owner. Laboratory readiness still requires a
matching Windows build and physical saved-data smoke.
