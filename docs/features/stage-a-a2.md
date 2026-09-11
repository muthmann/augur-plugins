# Stage-A A2 latency automation

A2 records synchronized camera RAW and photodiode PDQ at calibrated optical step
points. It leases the existing modulation and photodiode owners and uses the host
camera recorder. Latency, measured contrast, optical t50 and timing uncertainty are
estimated offline. A completed capture is not proof of a qualified latency result.

The recorder lives in the non-runtime `stage-a-step-acquisition` crate;
`plugins/stage-a-a2` is the entry point that exports its vtable and A5 embeds the
same recorder (ADR 050). The protocol files stay under `plugins/stage-a-a2/protocols/`.

## Production capture

Use `plugins/stage-a-a2/protocols/a2_drive_sync_smoke.toml` first, then
`a2_production_drive_sync.toml`. They explicitly select:

```toml
timing_reference = "drive_sync"
trigger_validation = "offline_review"
```

The 43-point production schedule takes 7,337 seconds (122 min 17 s), including
settling and excluding operator pauses, device acknowledgements and finalization.
It has dark brackets, a blocked-drive reference, three cadence controls, seven
repeated reference rows, and two passes through 3 flux targets × 5 depths. Each
condition receives 200 commanded transitions per polarity across the two passes;
partial boundary cycles must be removed offline. The 3-point smoke takes 37 seconds
plus pauses and file operations. The older comparator templates remain available
for qualified comparator experiments and keep their original strict defaults.

For a time-limited first block, `a2_core_drive_sync.toml` records 19 rows in
1,195 s (19 min 55 s) plus operator/file overhead. It keeps three flux targets,
three depths (0.28/0.45/0.80), dark/sham/cadence controls and four short reference
rows. Each grid condition has 50 commanded transitions per polarity before startup
and boundary exclusions. It is a prioritized first dataset, not the same precision
or drift replication as the full two-pass matrix. Keep the full file as an optional
extension; do not claim A2 scientific closure solely from the shortened capture.
A [separate Windows watcher](stage-a-lab-watch.md) can report missing file progress
and saved A2 errors. It does not qualify the data or protect against PC/network loss.

The camera trigger cable must receive **J24 digital phase-zero sync** for
`drive_sync`. Do not leave it on the comparator output. Firmware confirms
`J24_PHASE0`, an unarmed comparator and `SQUARE` before capture. J24 emits a narrow
pulse once per period. Its falling edge ends that pulse; it is **not** the optical
OFF transition. The PD stream records phase-zero markers (`source=1`) in its sample
index space. Comparator captures instead use `source=2`, with explicit level,
sample index and device microsecond tick. Wire framing remains PDA1-compatible.

The pulse is recorded as a digital marker alongside the PD samples; it is not
added electrically to the analog photodiode voltage. Both camera and PD must
observe corresponding timing anchors. Their device clocks remain independent.
A quiet interval precedes capture, and the stimulus is restarted only after both
recorders are open. The first new pulse train gives an identifiable common onset;
exclude the optical startup cycles from repeated-step estimates.
Offline analysis must match pulse sequences, reject gaps, unwrap device ticks and
fit offset and drift over each uninterrupted segment before mapping the measured
PD t50 to camera time. Nominal half-period is only a search window for optical OFF.
Never label the sync pulse's falling edge as optical OFF or align all rows by wall
clock/edge ordinal alone.

## Acquisition contract

The panel accepts a protocol and a measurement ID. It uses the PD owner's data
folder and archives the exact TOML by SHA-256. Camera RAW, camera configuration,
PDQ and both acquisition sidecars must remain together under `<data folder>/<ID>`.
The host receives this explicit root; A2 checks the returned paths before continuing.

Applied optical lobe/calibration, PD placement (`emission_path`), splitter fraction
(0.5), gain/load and selected reference ID come from the owners (ADR 040). No manual
copy of those values into TOML is required. A2 uses the current host camera state,
applies and confirms it before recording, then requests restoration on every exit.
EXT_TRIGGER and sensor telemetry are enabled; STC, Trail and ERC are disabled.

`drive_sync` skips comparator calibration and optical amplitude gates. No 20 mV
minimum, peak-to-peak noise limit or measured dead-time gate prevents its capture.
Unrepresentable DAC/frequency settings, unavailable owners, failed leases, missing
or empty recordings, sample loss/discontinuity, short PD coverage and failed file
writes remain acquisition errors. These conditions can destroy the requested data.

A2 `mean_u` is the **geometric** step pedestal: low/high targets are
`mean_u * exp(-a/2)` and `mean_u * exp(a/2)`. The production TOML uses
`mean_u = target_cycle_mean / cosh(a/2)`, rounded to 0.001, to match A1's nominal
cycle means 0.15, 0.30 and 0.45. Depths are 0.15, 0.28, 0.45, 0.80 and 1.30.
A1's sixth depth 1.70 is intentionally not part of this A2 matrix. Large depths are
model checks, not the small-step approximation. Matching commanded lobe means is
not evidence of matching camera-port flux: use the concurrent emission PD.

`comparator` remains the backwards-compatible timing default. Each auto-threshold
point measures eight separate PD windows per plateau after a sample-clock settling
guard. It checks clipping, stream identity, spread/uncertainty of window means,
threshold-DAC range and representability. Raw peak-to-peak noise becomes a recorded
warning rather than a 20 mV criterion. These are threshold-placement diagnostics,
not proof that an individual noisy edge has a precise t50. The threshold is measured
again at each point, so an earlier reference is not silently reused after drift.
The ADC 3.3 V reference and threshold DAC 2.5 V reference remain separate.

## Recording, evidence and failures

A2 prepares the stimulus before recording and keeps the free-running 500 kSa/s DMA
PD stream. It never issues command-port `START` inside PDQ capture: that command
switches the ADC sampler and resets acquisition counters. `CONFIG rate_hz=20000`
is the supported portable sampler setting; it does not reduce the independent
DMA stream rate. Dark uses safe-off; point end stops the waveform explicitly.
`STOP` alone stops the command sampler and is not a stimulus-off command.

Lease renewals have separate reply kinds and cannot advance acquisition phases.
Replies must match request, owner and run. Timeouts and cleanup retries are bounded.
PD sample progression has a five-second watchdog. Finalized receipts must attest
nonempty, contiguous, clean data of the requested duration. Failures retain paths,
hashes and the first cause; sidecar failures and unconfirmed cleanup are visible.
An aborted or failed point cannot become a successful completed protocol.

A2 sidecar schema 2 includes timing policy, actual command values, firmware marker
loss counters at command boundaries, PDQ marker counts, raw paths/hashes and
cleanup evidence. Photodiode final receipts carry additive optional marker counts;
old owners decode but missing evidence needs review. Live preview trigger counts
and event-load peaks are best-effort diagnostics, not an authoritative RAW audit.
`offline_review` retains these warnings and continues; it never converts them into
a valid timing claim. `strict` stops on timing warnings. Hard acquisition errors
stop either policy. Offline H4/H5 review is required even if acquisition checks pass.

## Firmware and scientific limits

Production `stage-a-controller` firmware now uses the DMA write cursor for the
stream marker index (`STATUS marker_clock=dma_cursor_v1`). Foreground delays no
longer enter its index calculation. Skipped DMA blocks advance physical sample time
and increment loss evidence instead of compressing the time axis. Ambiguous cursor
snapshots are counted as marker losses. Interrupt latency, ADC conversion/pipeline,
2 µs sample quantization and the actual camera trigger path still need calibration.
This implementation is not a hardware measurement of their error bounds.

The firmware build to flash for normal operation is the `teensy41` build. The
`a2_comparator` build is a standalone diagnostic and cannot run this capture path.
A new host plugin alone does not update the Teensy. Verify the firmware marker-clock
status, install matching runtime bundles, and fully restart the host before smoke.

Noise references can characterize offset, noise and electrical crosstalk. A separate
reference cannot reconstruct the random fluctuations in a later optical edge.
With approximately 380 mV peak-to-peak fluctuations, 20 mV modulation is not by
itself evidence of identifiable single-cycle t50. Preserve raw PD samples. Estimate
mean edges over repeated cycles and report the residual per-cycle timing uncertainty;
do not subtract reference noise as if it were the same noise realization.
Intrinsic pixel jitter and absolute latency require bounded synchronization/input
uncertainty. Otherwise report fluorescence-chain latency or drive-relative response.

See [ADR 041](../adr/041-stage-a-a2-drive-sync-capture.md) for the compatibility decision.

## Controller completion and initial records (2026-09-07)

A2 uses the shared queued controller service and retains its existing completion, integrity and cleanup checks. Initial point metadata is now written before camera start. A write failure ends preparation through normal cleanup. Return to A1 restores running ADC acquisition even when A2 drive-sync left the controller in A1 mode but stopped.

See [ADR 046](../adr/046-stage-a-command-completion-and-record-preservation.md) for the contract and Windows bench verification.

## Consistent recording workflow (2026-09-08)

A2 now follows the A1 interaction pattern: Measurement id, New id, Protocol,
Run protocol, Continue and Stop. A blank id is generated at start. A supplied id
names the subfolder and is retained for subsequent runs. Each run adds its own
timestamp to point filenames and a separate progress journal, so repeating a
protocol under one measurement id does not overwrite the earlier run.
Ids accept up to 100 ASCII letters/digits, hyphens and underscores; Windows device
names are refused before hardware commands. Measurement settings cannot change
while a run is active. Continue is offered only at a manual pause.

The photodiode's chosen data folder is resolved once to an absolute root. Camera
StartRecording and PD BeginRecording receive that same root. A2 verifies the
actual camera/PD parent directories before starting the stimulus and stores the
opened paths immediately. A host that ignores the root is stopped before PD
acquisition instead of silently splitting the files. The required generic host
change is documented in augur-rs ADR 028. **Install a matching host and plugin
build; this fix cannot be deployed by replacing the A2 DLL alone.** A1 also passes
its selected root and retains its historical post-finalization collection fallback.

Loading a valid protocol shows its acquisition-plus-settling duration. During a
run the status shows point count, completed recordings, output folder and remaining
timed duration. The current recording/settle timer counts down. Manual pause time,
controller acknowledgement/auto-comparator qualification and disk finalization
are not predicted; they are explicitly additional time. Zero remaining timed
seconds during cleanup does not mean the files have finished closing.

Modulation requests that report InProgress are polled with the same request id and
revision, at most five times per second, against the owner's retained completion
history. The original deadline is retained. A status response for another run is
ignored. A host preview reset does not erase an active acquisition. Explicit camera
start rejection does not send StopRecording for a different recording.

Each invocation writes `<timestamp>_progress.jsonl` in its measurement folder.
It retains run start, point start/final result, run completion, paths and failure
or cleanup details. Point JSON is replaced atomically after syncing a temporary
file, so a failed update preserves the last complete version. The evidence field
`acquisition_complete` is separate from `valid` and offline scientific status:
preview timing warnings do not turn an intact offline capture into a failed file.
Hardware/recording failures remain visible and stop the run through cleanup.

Tests cover full dark/step completion, root disagreement, changed owner settings,
metadata preservation, repeated ids, asynchronous completion identity, reset,
manual pauses, timing countdown and cleanup evidence. Windows USB/camera timing
still requires a short saved RAW/PDQ pair on the laboratory computer.

## Automatic continuation

Run with the same measurement ID and unchanged protocol to resume. A2 checks the
existing folder before acquiring hardware. It skips only original row numbers with
matching protocol SHA-256, matching point data, explicit `acquisition_complete`
and all four nonempty local artifacts (RAW, camera TOML, PDQ, PD JSON). Repeated
conditions remain separate protocol rows. Missing, partial or legacy records without
explicit completion are acquired again; existing files are kept with unique attempt
names. Offline timing warnings do not by themselves require a repeat.

The display counts reused and newly acquired rows and estimates the remaining
acquisition and settling time. A completed protocol makes no hardware requests.
On resume, confirm the optical path for the first missing point; pauses crossed by
skipped rows are retained at the next acquired point. This prevents a saved dark or
blocked-drive reference from silently leaving the next measurement in the wrong
physical state. Resume verifies file presence, not a full offline integrity analysis.

The UI mirror does not own the active run. Continue and Stop therefore stay
accessible in the panel; the live worker accepts Continue only at a manual pause
and treats Stop with no active run as a no-op. An early Continue click is consumed
and cannot acknowledge a later pause.
