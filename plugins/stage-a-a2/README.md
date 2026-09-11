# Stage-A A2 latency runner

Record camera RAW and continuous photodiode PDQ with a common digital timing anchor;
measure optical t50, contrast and latency offline.

1. Install the matching plugin release bundles and production Teensy firmware.
2. Connect camera EXT_TRIGGER to **J24 phase-zero sync** for these protocols.
3. Apply the optical calibration in the modulation owner. Configure the emission
   PD, reference set and output directory in the photodiode owner. Enable camera
   trigger recording and monitoring, and disable STC/Trail/ERC.
4. Load `protocols/a2_drive_sync_smoke.toml` (37 s plus pauses).
5. After checking its finalized files and common timing anchors, load
   `protocols/a2_production_drive_sync.toml` (43 points, 122 min 17 s plus overhead).

The runner sets every point itself. Pauses explicitly ask to block/open the path;
`blocked_drive_sham` keeps it blocked while the electrical drive runs.
The production protocol has no online PD-amplitude threshold. Missing/corrupt data
still stop the acquisition; timing warnings remain visible for offline review.

J24 pulse falling is not optical OFF. PDQ stores the digital marker alongside the
analog samples. Fit clock offset/drift, then locate optical ON/OFF in the PD waveform.
A completed protocol does not by itself qualify absolute latency or intrinsic jitter.

Existing comparator templates retain their strict defaults. See the complete
[feature contract](../../docs/features/stage-a-a2.md) for modes, evidence, reference
reuse, mean-level conventions, firmware installation and remaining hardware checks.

For the time-limited first laboratory block, load `protocols/a2_core_drive_sync.toml`
(19 points, 19 min 55 s plus overhead). This retains the essential capture controls
but has fewer repeats and does not replace offline timing qualification.

### Measurement control

Like A1, select a **Measurement id** (or **New id**), load the protocol, then use
**Run protocol**, **Continue** at a requested pause, and **Stop** when needed.
The id names the folder; subsequent runs use unique filenames. The status shows
completed points and remaining acquisition/settling time, excluding manual pauses,
controller setup and file finalization. All files go below the data folder chosen
in the photodiode plugin.

This workflow requires the matching Augur host with `StartRecording.root_dir`
support. A2 checks the actual output directory and refuses a split camera/PD run.
After updating, close/reopen the Windows host and verify one short saved pair
before the long protocol. Do not mix the new DLL with an older executable.

Reuse the same measurement ID and exact protocol to continue only missing rows.
The runner requires explicit completion metadata and the four nonempty recording
files in that ID folder. Older records without completion evidence are not skipped.
The status separates reused/new rows and estimates remaining acquisition time.
