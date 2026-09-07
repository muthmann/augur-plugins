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
