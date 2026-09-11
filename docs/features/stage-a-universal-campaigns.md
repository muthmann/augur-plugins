# Universal Stage-A campaigns, 20260911-v5-4h

Bias preparation and the selected-state final share a **four-hour operator
budget**. This replaces the v4 six-hour final plus separate preparation. Both
0.08 and 0.8 camera-lux remain main levels. The old v4 files remain available in
Git commit `c96508a`; do not mix their checkpoints or measurement IDs with v5.

## Four-hour schedule

| Elapsed window | Work | Maximum minutes |
|---|---|---:|
| 00:00–00:05 | Saved-data smoke and setup check | 5 |
| 00:05–00:55 | `dim_bias_selection`, including finalist checks and light changes | 50 |
| 00:55–01:00 | Select and freeze the winner; new final measurement ID | 5 |
| 01:00–04:00 | `selected_state_final`, including closing and recovery | 180 |
| Total | Preparation, selection and final | **240** |

The two runner clocks are independent. Their limits total 230 minutes; the
remaining ten minutes cover smoke and selection. Start the overall laboratory
clock before smoke and do not let a delayed final start move the four-hour stop.
The final should start by overall T+60 min. If setup or selection exceeds its
allowance, stop by the original overall deadline and record missing coverage.
The runner does not share a clock automatically between these separate programs.

## Executable files and measured-row estimates

All paths below are under `stage-a-universal-runner/protocols/`.

| Campaign | File | Blocks | Rows | Estimated owner minutes |
|---|---|---:|---:|---:|
| Bias preparation | `stage-a-dim-bias-selection.toml` | 20 | 64 | 37.13 |
| Grand Final | `stage-a-grand-final.toml` | 18 | 161 | 104.15 |

Estimates include recording, settling and 11 seconds per file. The difference
from the operator limits is deliberate recovery/handling margin, not additional
requested acquisitions. Do not consume spare time with the older extension files.
The generator also retains standalone diagnostic protocols; these are not part
of the shortened bias/final sequence. Its 28-file catalog is not a 28-step run.

`scripts/build_universal_protocols.py` generates the actual owner CSV/TOML rows,
compiled catalog and campaign manifests together. `--check` detects drift. The
plugins embed these files; an empty external protocol path selects the built-ins.
Changing display estimates alone is rejected. The short program names are unchanged.

## Bias selection

Keep all six B0–B5 candidates, hpf=0, refr=235, filters off and the qualified ROI.
At 0.08 lux each candidate receives a 45-second static record and seven sine rows:
0.5/8 Hz crossed with depths 0.08/0.20/0.80, plus 0.25 Hz at depth 0.80. Use at
least eight cycles and eight recording seconds per sine row, with two settling
cycles or three seconds, whichever is longer. Repeat B0 static and two 2-Hz
reference rows after the third and sixth candidate.

Review stimulus-locked response, misses, polarity balance, background and drift
together. Pick two distinct finalists; the runner checks each at 0.8 lux using
a 45-second static record and 0.5/8 Hz crossed with depths 0.12/0.80. There is no
0.03-lux finalist check in this version. The final winner is selected manually;
B4 is not predetermined. The smaller screen ranks candidates coarsely, not a
continuous optimum. Short static records give weaker rare-noise estimates.

## Grand Final

Run only laser-off, 0.08 lux and 0.8 lux states. At both lit levels retain:

- 60 A1/A3 grid rows: 0.25/0.5/1/2/4/8/16/32/128/512 Hz crossed with depths
  0.04/0.08/0.12/0.20/0.36/0.80.
- Two 0.125-Hz guards at depths 0.20/0.80 and eight 2-Hz reference rows,
  giving **70 rows per main level**, split into three owner files.
- At least 12 cycles and 12 recording seconds per sine row; two settling cycles
  or three seconds. Repeated anchors bracket the chunks.
- 90-second static records before and after each main level.
- A2 steps at depths 0.12/0.36/0.80, one-second plateaus and 25 transitions per
  polarity. At 0.08 lux add 0.5/2-second cadence checks at depth 0.36.
- At 0.08 lux keep four held-out conditions: depths 0.24/0.60 crossed with
  0.35/1.5-second plateaus, 20 transitions per polarity. Exclude these from fitting.

Open with 90 seconds laser-off. Close with a 0.08-lux static record, two response
anchors and 90 seconds laser-off. The 8/0.3/0.03-lux blocks, A5 load/recovery,
nested ROI, extra cadence repeat and very-low-frequency extension are removed.
This avoids additional optical states and recorder/ROI transitions. It does not
establish independent refractory behavior, bright-load limits, sub-0.08-lux
performance or a precise cutoff in an unsampled frequency gap.

Fit the measured optical input, separate ON/OFF and background, and report bounds
when a response plateau or roll-off is not resolved. This dataset supports a
local behavioral twin at the frozen state. Camera lux is an operating coordinate,
not independently calibrated photon flux or QE; spatial validation stays separate.

## Controls, deadlines and delivery

Use the [bench smoke procedure](stage-a-universal-runner-test-protocol.md) with a
matching Windows bundle. The short final uses A1, A2 and A4 plus the existing
modulation/photodiode owners; no A5 acquisition or ROI change is requested.
The broader standalone `smoke` still exercises all owners and lasts about 1.56
owner minutes. A4 prepares/releases a constant reference around each AOD pause.
Set the requested AOD/laser state, wait for stable readings, then Continue.

Bias acquisition cutoff is 45 minutes, with a 50-minute campaign limit; the last
finalist pair is closing work. Final acquisition cutoff is 165 minutes, with a
180-minute campaign limit and 15 minutes for closing. Limits include in-program
pauses and retries. Safe file cleanup can exceed a deadline after hardware or
storage failure; software cannot guarantee physical completion in that case.
Any omitted required block means incomplete coverage. Do not select a bias from
an incomplete candidate comparison without explicitly reviewing that loss.

Keep the existing bounded retries, explicit owner/measurement/attempt terminal
identity, `plan.json`, `events.jsonl`, checkpoints and `universal-request*.json`
alongside RAW/PDQ/sidecars. Resume retains the original program clock. Use a fresh
campaign for v5; old v4 checkpoints have different sampling and timing. Preserve
an incremental second copy rather than postponing all copying until the end.
