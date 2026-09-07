# Stage-A A2 latency automation

A2 records synchronized camera RAW and photodiode PDQ at calibrated optical step
points. It leases the existing modulation and photodiode owners and uses the host
camera recorder. Latency, measured contrast, optical t50 and timing uncertainty are
estimated offline. A completed capture is not proof of a qualified latency result.

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

The panel asks for the protocol. The runner creates the measurement ID, uses the
PD owner's data folder and archives the exact TOML by SHA-256. The host may put RAW
in its own output folder; the A2 sidecar stores the returned absolute path and
links camera-configuration and sensor-monitoring companions. Keep both folders.

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
