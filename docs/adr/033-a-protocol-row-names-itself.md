# ADR 033 — A protocol row names itself, in its file names and in both sidecars

**Status:** accepted
**Date:** 2026-08-05
**Feature briefs:** [Stage-A A1 Analysis](../features/stage-a-a1.md)
**Relates to:** [ADR 027](027-stage-a-a1-declarative-protocols.md) (declarative
protocols), [ADR 015](015-stage-a-a1-recording-robustness.md) (one measurement
folder), [ADR 029](029-stage-a-leases-are-renewed-against-the-granted-deadline.md)
(the sidecar refusal an expired lease produces)

## Context

A protocol run came off the bench as a folder of files that could not be told
apart. Every row had the same duration, and the stems differed only in the
second the row started:

```
A1-survey_20260805-141201_pd.pdq
A1-survey_20260805-141233_pd.pdq
A1-survey_20260805-141305_pd.pdq
```

Mapping a `.pdq` back to the modulation it was recorded under meant sorting by
timestamp and counting rows in the protocol file — a join that is only correct
if no row was skipped, and every row of a survey may be skipped (ADR 027 keeps
a refused point non-fatal, precisely so an overnight run finishes).

Three things were missing, and each was missing for the same reason: **a
protocol is the one recording path where nothing in the panel is armed at
anything that separates its recordings.**

- **The file stem had no row tag.** `begin_recording` builds
  `<id>_<timestamp>[_role]<sweep_tag>`, and `sweep_tag` is non-empty only while
  an amplitude sweep or the event-count workflow is in its recording phase. A
  protocol drives the axes itself and hands off to the same coordinator, so it
  took the empty branch. `ProtocolPoint::tag()` — `u500m_f10Hz_a800m` — already
  existed and had a test, but nothing ever called it.
- **The A1 sidecar had no `[protocol]` section.** It had one for every other
  automatic path (`[sweep]`, `[a0_lock]`, `[frequency_sweep]`). A protocol row
  filled in `[sweep]` with the panel's `min_a`/`max_a` and nothing else, so the
  file did not record what the row had asked for on any of the three axes.
- **The recorder metadata described the drive only partly.** It carried
  `modulation_frequency_hz`, `center_dac` and `amplitude_dac` — a frequency and
  two codes, which do not name the lobe or the operating point. `ū` in
  particular is a whole axis of the survey and appeared nowhere: two rows can
  share `f` and `a` and differ only in the mean illumination they were driven
  around.

The last one matters more than it looks, because the A1 `_config.toml` is not
guaranteed to exist. `write_sidecar` refuses outright without a fresh photodiode
optical summary — deliberately, since it is the quantitative record — and that
refusal is exactly what an expired lease or a drive switched off mid-row looks
like after the fact (ADR 029). A run that hits it keeps its `.pdq` and its
`_pd.json` and loses everything else, so the `_pd.json` has to stand alone.

## Decision

**A protocol row is identified on every leg, by the row itself.**

1. **The file stem carries the point tag.** After the timestamp and role:
   `_p<NN>_u<ū>m_f<f>Hz_a<a>m`, e.g.

   ```
   A1-survey_20260805-141233_p03_u500m_f10Hz_a800m_pd.pdq
   ```

   The 1-based index over the whole expanded protocol comes **first**, so the
   files sort in protocol order rather than by `ū`, and its width follows the
   point count (`_p03` for 12 rows, `_p003` for 400). It is emitted only while
   the run is in `ProtocolPhase::Recording`, so a hand-driven recording is
   unaffected.

2. **The A1 sidecar gains a `[protocol]` section**: `name`, `block`,
   `point_index`/`point_total`, `point_tag`, `requested_mean_u`,
   `requested_frequency_hz`, `requested_depth_a`, `settle_s`, `duration_s`.

3. **The recorder metadata — which reaches both the `_pd.json` and the camera
   leg — carries the same identity** (`protocol_*` keys) **and a full
   description of the drive**: `modulation_waveform`, `pockels_calibration_id`,
   `optical_target`, `requested_mean_u`, `resolved_mean_u`, `commanded_a`,
   `v_null_dac`, `v_peak_dac`.

**What is written is the request, not the result.** `[protocol]` and the
`protocol_*` keys say what the row asked for; `[modulation]` says what the drive
reported back, and `[optical]` what the photodiode measured. Collapsing them
would make a row that missed its point indistinguishable from one that hit it,
which is the failure the survey exists to detect.

**Free text from the protocol file is clamped** to 120 characters before it
enters the metadata map. The photodiode owner bounds values at 1 KiB and refuses
the whole `BeginRecording` if one is over — a chatty block name must not be able
to cost a recording.

## Consequences

- A `.pdq` names its operating point without any join at all, and survives the
  loss of the A1 sidecar with its provenance intact.
- Rows sort in protocol order in a file listing, so a skipped row is visible as
  a gap in the indices rather than as a missing timestamp nobody can place.
- Stems are longer. That is the trade: a name that says what the file is beats a
  short one that says when it was written.
- Two rows at the same `(ū, f, a)` — a deliberate repeat — still share a tag and
  are separated by the point index and the timestamp.
- Nothing changes for hand-driven recordings, sweeps, ladders or event-count
  points; their existing tags and sidecar sections are untouched.
- The refusal to write a quantitative sidecar without a measured optical summary
  is **kept**. This ADR reduces what is lost when it fires; it does not paper
  over the upstream fault that causes it.
