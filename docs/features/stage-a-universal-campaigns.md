# Universal Stage-A campaigns, 20260910-v4

The active campaigns implement the response-based bias preparation and the
six-hour selected-state final. Both 0.08 and 0.8 camera-lux are full main levels.
Camera lux is the operational brightness axis; it is not photon flux or QE.

## Executable files

| Campaign | File under `stage-a-universal-runner/protocols/` | Expanded rows | Estimated owner minutes |
|---|---|---:|---:|
| Preparation | `stage-a-dim-bias-selection.toml` | 172 | 114.13 |
| Grand Final | `stage-a-grand-final.toml` | 334 | 233.05 |
| Mixed-owner smoke | `stage-a-smoke-test.toml` | 5 | 1.56 |
| Bright reference | `stage-a-all.toml` | 13 | 11.28 |

Preparation has a separate 150-minute operator budget. The final is planned as
300 minutes of work plus 60 minutes reserve. Estimates include row settling,
recording and 11 seconds per file. They do not guarantee physical file closure
or operator speed. The campaign clock starts at Run, including readiness and
optical pauses; begin any additional initial checks within the operator budget.

`scripts/build_universal_protocols.py` generates all active manifests and 28
actual owner protocols in `protocols/acquisition/` beneath the shared runner
crate. `--check` detects drift. The compiled catalog includes the same CSV/TOML
contents, row counts and duration totals. Editing display estimates alone is
rejected. Historical bright studies and the separate v1 low-light bundle remain
archives, not additional steps in this campaign.

## Scientific scope

Preparation pairs static background with modulated response for all six B0–B5
candidates at 0.08 lux. Two operator-selected finalists are checked at 0.8 and
0.03 lux. B4 is not an automatic winner. Each candidate uses hpf=0, refr=235,
filters off and the qualified ROI; ON/OFF and fo vary as defined in the catalog.

Each main level has 96 frequency/depth rows, two slow guards and eight repeated
reference rows, split into three owner files. The 0.03-lux extension has 70 rows.
A2 adds depth, cadence and held-out temporal checks. A4 supplies matched static
and laser-off backgrounds. A5 supplies effective load/recovery checks, including
one centered half-width/half-height ROI comparison. It does not identify an
intrinsic refractory time by itself. A6 is integrated lux/PD provenance; the
standalone A6 reference file now delegates a static record to A4.

Step protocols correct the arithmetic mean to maintain the intended geometric
midpoint. Measured optical input, polarity-separated response, misses, background
and stationarity remain necessary for offline interpretation. Keep held-out rows
out of fitting. Spatial microscopy validation remains separate.

## Operator sequence

1. Install matching host and plugins, restart, and complete the
   [saved-data smoke](stage-a-universal-runner-test-protocol.md).
2. Use an absolute common output folder and a new campaign prefix. Run preparation.
3. At each optical pause wait for the constant reference, set the AOD/laser, then
   Continue. Lux and PD observations are captured automatically with the AOD value.
4. Enter two distinct measured finalists when requested. Review signal and
   background together, then freeze the final readback state in the bench log.
5. Set `selected_candidate` and run `selected_state_final` with a fresh prefix.

Readiness is checked for every recorder. A4 owns the temporary constant reference
and releases it before recording. A5 directly starts the A2 recorder through the
service interface, preserving the A5 routing identity and attempt number.

## Deadline, recovery and records

The final has a 21,600-second campaign limit and a 19,800-second acquisition
cutoff. Non-closing blocks receive the cutoff; closing blocks receive the final
deadline. Optional 0.3-lux blocks are omitted first, then reduced 0.03-lux blocks.
Every omission is retained. A required block that cannot fit is missing coverage,
not a successful measurement. Deadline expiry requests safe owner stop; file
finalization must complete and can take longer if hardware or storage fails.

The runner accepts a terminal result only from the current owner, measurement ID
and attempt. Automatic retries are bounded to three attempts. A1/A2/A5 can reuse
their completed point artifacts; A4 retries use distinct IDs. No failed point is
silently represented as measured. Continue cannot launch duplicate active blocks.

Each campaign stores `plan.json`, `events.jsonl`, `checkpoint.json` and a previous
checkpoint. Each measurement retains its exact `universal-request*.json` plus
owner RAW, PDQ and sidecars. Persistence failure stops the campaign. To recover,
first ensure all owners are idle and finalized, set `resume_checkpoint`, and Run.
Completed blocks remain completed and optical state must be reconfirmed. Resume
retains the original clock; an expired six-hour run cannot be extended by resume.
Manual recovery requires an operator review of the failure and artifacts.

## Verification boundary

Production parsers validate all generated owner files. Tests exercise bias
overrides, AOD reference lifecycle, active Continue, owner/attempt identity,
deadlines, full sequencing, resume and A5 handoff. Release compilation is local
macOS verification. A matching Windows installation, physical saved-data smoke,
actual optical stability and backup verification remain bench acceptance work.
