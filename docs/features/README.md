# Feature Briefs

Repository-level feature notes for larger plugin suites, interface migrations, and architectural workflows.

## Available Briefs

- [Stage-A Bench Stack](./stage-a.md) — Teensy-driven Stage-A bench: two serial ports, two minimal plugins, and the shared `stage-a-io` library.
- [Stage-A Modulation](./stage-a-modulation.md) — orthogonal Manual/Calibrated drive methods and five waveform modes under one hard DAC ceiling, applied immediately on the command port.
- [Stage-A Optical Waveform Drive](./stage-a-optical-waveform.md) — pre-warps the DAC from measured `V_null`/`Vπ`, with target-specific headroom, Bessel-normalized cycle mean `ū`, and an explicit separation from physical flux `I_k`.
- [Stage-A Pockels Transfer Calibration](./stage-a-pockels-calibration.md) — one-button sweep of settled `CONST` DAC codes against the photodiode level, fitting `V_null`/`Vπ` from the light instead of a nominal datasheet, with a transfer-curve view that makes the two parameters legible before anything is measured.
- [Stage-A Photodiode](./stage-a-photodiode.md) — live SMA5/A4 readout plus fail-closed excitation log-contrast `a`, computed from complete phase-marker cycles in reject-port geometry only after a named `I_tot` anchor is explicitly confirmed.
- [Stage-A A1 Analysis](./stage-a-a1.md) — synchronized camera RAW + photodiode PDQ coordinator and fail-closed calibrated log-sine amplitude sweep, with physical `flux_point_id`, transfer/anchor provenance, and live response quicklooks.
- [Stage-A A1 Automation](./stage-a-a1-automation.md) — roadmap to semi-automate the amplitude sweep; the single-row sweep core is **built** (ADR 010), scout/multi-row/`a50` fit remain planned.
- [Stage-A A1 Exact Event Count](./stage-a-a1-event-count.md) — per-frequency `a₀` lock: closed-loop trim of the commanded depth until the photodiode *measures* the one frozen log contrast `a₀` over whole modulation cycles, a per-frequency lock table on disk, a one-button atomic frequency point recorded at exactly `a₀` under the modulation lease, and an unattended log-spaced frequency ladder that locks and records every planned `f` on a single lease.
- [EVE Temporal Diagnostics](./evesmlm-temporal-diagnostics.md) — temporal candidate tracking, boundary overlays, and rejected-fit datasets for the eveSMLM pipeline.
- [Plugin Authoring Docs Refresh](./plugin-authoring-doc-refresh.md) — repo docs synced to the current runtime-only interface, host views, and `GlobalSettings`.
- [Plugin Install And Reload](./plugin-install-reload.md) — macOS dylib identity fix so installed plugins do not keep pointing back at Cargo's build tree during reloads.
- [Investigation Workspace Alignment](./investigation-workspace-alignment.md) — in-tree plugins updated for stable ids, linked 2D/3D/table datasets, and candidate-stage accepted/rejected event inspection.
- [Plugin Runtime Migration Notes](./plugin-api-v0-2.md) — historical runtime-migration brief, updated with the current interface additions that matter to this repo.
- [Plugin Host Views](./plugin-host-views.md) — generic host-rendered datasets, cache generations, and shared view ids.
- [TableV1 Declarative Metadata](./tablev1-declarative-metadata.md) — plugin-side adoption of row provenance, display formats, and cross-dataset relations for trustworthy table rendering.
- [Clickable 2D Overlays via Marker `source_row`](./clickable-overlays-source-row.md) — plugin-api ABI 4 `source_dataset_id`/`source_row_id` plumbing and failed-fit click-to-select loop.
- [Action Requests And Single-Cluster Refit](./action-requests-and-refit.md) — plugin-declared host actions, eveSMLM refit/commit/discard flow on the `augur.evesmlm.refit_preview` dataset.
- [Reconstruction Workflow](./reconstruction.md) — accumulated localization tables rendered and exported by the host.
- [eveSMLM Pipeline](./evesmlm.md) — candidate finding, fitting, and post-processing as three chainable plugins.
