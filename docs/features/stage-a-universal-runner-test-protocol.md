# Universal Runner bench test protocol

Current executable version: **20260911-v5-4h**. Read the [campaign guide](stage-a-universal-campaigns.md) first. Historical v1/v4 files are not the run order.

This protocol verifies the complete hand-off path before a final Stage-A run.
It is a bench acceptance test, not a scientific result. Record the date, host
revision, installed plugin bundle, camera, modulation, photodiode and output
root in the run log.

## Pass criteria before starting

1. Install one bundle built from the same `augur-rs` and `augur-plugins`
   revisions. Fully restart Augur after installation.
   On macOS, verify every installed dynamic library exports
   `augur_plugin_vtable`; on Windows, use the bundle's symbol/export check.
   If Augur reports `GetProcAddress failed`, the plugin is not loaded and the
   bundle must be replaced before continuing.
2. The Universal Runner status must show the selected protocol name, block
   count, expanded owner-point count, owner-time estimate and an absolute
   output folder. The estimate excludes manual pauses and retries.
3. Every listed required plugin must be loaded. For the short final this is
   Universal Runner, A1, A2, A4, Modulation and Photodiode. The broader smoke also
   uses A3 and A5.
4. The status must show camera-lux readback, photodiode connection/data path,
   modulation connection and modulation calibration as ready. If a required
   item is missing, do not press Run.

## Test 1: intentional fail-closed checks

Run these one at a time and confirm that the status explains the correction:

| Condition | Expected result |
| --- | --- |
| Empty output folder | Run is refused; status says to choose an absolute common output folder. |
| Relative output folder | Run is refused; status says the path must be absolute. |
| Missing A2 plugin | The block is refused with the exact target and service error; no later block starts. |
| Missing modulation or photodiode | The owner refuses the block; the Universal Runner retains the block and reports failure. |
| AOD setting empty | Continue confirms; the hand-off records `not entered; camera lux <readback>` as the control value. |
| Continue with no campaign running | Refused; status says to press Run. Nothing starts. |
| AOD value re-typed between two blocks at the same state | The next block waits, A4 prepares the reference again, and Continue records the new value. |
| A4 refuses or fails the optical reference | The campaign shows the error and Stop ends it; no block starts. |
| No camera-lux or PD readback | Continue is refused and identifies the missing readback. |

## Test 2: smoke program

Select `smoke`, use a new empty absolute output folder, and press Run. The
status must show five blocks and five owner points. For every block verify:

- the current block number, experiment owner, measurement ID and next action
  are visible;
- the owner receives the same output root and creates its measurement folder;
- the block changes to completed only after the owner publishes completion;
- a camera recording, photodiode artifact and protocol sidecar are present;
- the next block starts automatically only after the previous completion;
- the final status says complete and the output folder contains all five
  measurement subfolders.

## Test 3: optical-state transition

Use `dim_bias_selection` or `selected_state_final` with a fresh output root.
Confirm the first state, then change only the AOD to the next requested state.
The runner must pause, show the requested state and operator action, and wait
for a new Continue. It must not dispatch the next block with the old optical
confirmation. Verify that the confirmation stores AOD value, camera lux, PD
level, state ID and UTC timestamp in the owner hand-off.

## Test 4: recovery and stop

During a smoke block, disconnect or disable the owner before completion. The
runner must show the failed measurement ID, retain the failed block, and stop
without starting a later block. Re-enable the owner and repeat only the failed
block in a new measurement root. Verify that partial artifacts remain available
for diagnosis and that camera/modulation/photodiode state is restored.

## Final acceptance

The final program may run only after Tests 1–4 pass. Before the real run,
verify once more that the displayed estimate is understood as an owner-protocol
estimate, not a guaranteed wall-clock time: manual AOD changes, stabilization,
retries and hardware recovery add time. Keep the complete output root and its
manifest; never treat a skipped or missing block as a successful final run.

## Version 5 acceptance checks

- Set `selected_candidate` to the measured B0–B5 winner before the final.
  During preparation, enter two distinct finalists when the runner requests them.
- At each light change, wait for the A4-owned constant optical reference to be
  ready. Adjust the AOD or laser, then Continue. Confirm that the reference leases
  are released before the next acquisition starts. Do not use Preview to reset it.
- Confirm that 0.08 and 0.8 lux each expand to 70 A1/A3 rows and that the live
  camera readback contains the requested ON/OFF, fo, hpf, refr and filter state.
- The shortened bias/final sequence uses A1/A2/A4 and keeps the qualified ROI.
  A5 and nested-ROI checks remain standalone diagnostics outside the four-hour run.
- Stop once during a saved smoke record. Confirm terminal RAW/PDQ receipts and
  restoration before resuming from `checkpoint.json`. The original start time
  and completed blocks must remain unchanged. A resumed failed attempt must not
  accept a stale terminal snapshot from its predecessor.
- Inspect `plan.json`, `events.jsonl`, `checkpoint.json` and each block's
  `universal-request*.json` beside the original acquisition artifacts.
- Test a shortened custom campaign deadline before the long run. Non-closing
  acquisitions must stop at their cutoff; only closing blocks may use the closing
  allowance. A stop is not a successful full campaign.

Source tests and a local macOS release build do not establish Windows bench
readiness. Verify the matched Windows host/bundle and this saved-data smoke.

Version 5 budget check: bias 50 minutes, final 180 minutes, plus five minutes
smoke and five minutes selection. Verify the displayed version and these limits
before starting. Both independent runner clocks must fit the overall four-hour
operator window.
