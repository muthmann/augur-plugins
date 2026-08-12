# ADR 033 — The photodiode ring sizes itself to the drive

- Status: accepted
- Date: 2026-08-07
- Supersedes: nothing. Extends [ADR 020](./020-stage-a-a1-depth-source.md) and
  [ADR 027](./027-stage-a-a1-declarative-protocols.md).

## Context

The photodiode's optical log-contrast `a` is fail-closed: it is estimated only
over a marker-bounded window covering at least **two complete modulation
cycles**, so it needs three retained phase-0 markers. The window can never be
longer than the raw ring, and the ring was sized by one operator setting —
**Cache length**, 20 s by default, 130 s maximum, hard-capped at 16 M samples
(32 s at the bench's 500 kSa/s).

Two cycles at the A1 laboratory protocols' 0.075 Hz floor are 26.7 s. The
default retains 20 s. So every sub-hertz rung of those files was structurally
incapable of producing an `a` — and the cost was paid at the worst possible
moment:

- A1 pre-checks the photodiode before it starts a recording, but right after a
  retarget the ring still holds markers from the *previous, faster* rung. The
  check passed on those.
- The old markers then aged out during the recording, and the refusal arrived at
  `write_sidecar`, i.e. after the point had run its full 120–267 s. A1 counts
  such a point as skipped, so the run kept its RAW and PDQ files and lost the
  metadata that makes them quantitative.

A bench session on 2026-08-07 reported `point 4/49 — 1 recorded, 2 skipped`
against exactly this. The recorded mitigation was documentation: "set and verify
the photodiode cache at 30 s before starting this file", asserted by a test that
pinned the 30 s setting. That is a precondition no software checks, that has to
be recomputed per file from its lowest frequency, and whose omission is only
discovered a recording at a time.

## Decision

The ring is sized by the drive, not only by the setting:

```
capacity = clamp(max(cache_seconds × rate, (CONTRAST_WINDOW_CYCLES + 1) × period),
                 2, RING_MAX_SAMPLES)
```

where `period` is the marker-measured modulation period in samples. The
operator's **Cache length** becomes a floor rather than the whole answer.

- The period comes from the **newest** marker interval, falling back to the mean
  over retained markers. The newest interval moves to the new period on the
  first marker after a retarget, where the mean still carries the previous rung
  and would grow the ring one cycle at a time. It also survives eviction, so a
  period longer than the ring itself — the case this exists for — is still known.
- One cycle beyond the estimator's window, so a whole window still fits once the
  oldest marker ages out of it.
- Sizing follows the drive **both** ways: eviction re-reads the capacity every
  ingest, so the ring shrinks again when the frequency goes back up.
- `RING_MAX_SAMPLES` still binds. Below ~0.06 Hz at 500 kSa/s nothing retains two
  cycles and the estimator refuses — correctly, and now for a reason no setting
  can talk it out of.

Independently, A1's sidecar refusal quotes the owner's published
`optical_unavailable` reason instead of naming the `I_tot` anchor whatever the
real gate was. That refusal is the entire report an unattended protocol run
leaves behind for a point it lost.

## Consequences

- A sub-hertz A1 protocol runs with no cache preconditions. The
  "verified 30 s cache" step is removed from the feature brief, the plugin
  README and the shipped protocol headers.
- Worst case memory is unchanged: `RING_MAX_SAMPLES` was already the documented
  ceiling, the ring just reaches it on its own at low `f` (32 MiB of codes plus
  ~2 MiB of summary cells).
- A bogus period estimate — one dropped marker doubles the interval — grows the
  ring toward that same ceiling and self-corrects on the next marker.
- A cache set shorter than the drive is no longer a way to starve the estimator,
  so the unit test that produced `IncompleteModulationCycles` that way now
  produces it the way the bench does: a drive whose cycles have not gone by yet
  (the first marker after a retarget or a segment restart).
- Still not fixed by this ADR: the pre-recording check can pass on a summary
  built from the previous rung's markers. It is now only a decision about
  whether to *start*, because the window at the end of a recording is what the
  sidecar records, and every shipped protocol row runs at least two cycles.
