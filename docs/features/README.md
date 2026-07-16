# Feature Briefs

Repository-level feature notes for larger plugin suites, interface migrations, and architectural workflows.

## Available Briefs

- [Stage-A Bench Stack](./stage-a.md) — Teensy-driven Stage-A bench: two serial ports, two minimal plugins, and the shared `stage-a-io` library.
- [Stage-A Modulation](./stage-a-modulation.md) — capped power slider + constant/sine/square laser-modulation drive on the command port, applied immediately.
- [Stage-A Photodiode](./stage-a-photodiode.md) — live SMA5/A4 photodiode readout from the PDA1 stream port at 20 kSa/s with envelope decimation and a period-synced moving average: raw values or excitation power `I_exc = I_tot − I_pd`.
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
