# ADR 027 — A1 records surveys from a declarative protocol, including the `I_k` axis

- **Status:** Accepted
- **Date:** 2026-08-01
- **Relates to:** ADR 009 (recording coordinator), ADR 010 (amplitude sweep),
  ADR 014 (frequency ladder), ADR 023 (nested depth/frequency sweep),
  ADR 025 (clamping vs. refusing),
  [Stage-A A1 Analysis](../features/stage-a-a1.md)

## Context

A1 could sweep two of the three axes the experiment has:

| axis | what moves it | how it is swept |
| --- | --- | --- |
| `a` — optical depth | `SetOpticalDepth` | amplitude sweep (ADR 010) |
| `f` — frequency | `SetDriveFrequency` | frequency ladder (ADR 014) |
| `I_k` — mean illumination | *nothing* | by hand, in the modulation plugin |

Each sweep button moves its own axis and leaves the others wherever the
operator last put them. For exploring, that is the right shape. For a survey it
is not:

- the `I_k` axis could not be swept at all, so a brightness series was one
  manual edit per point with a lease released in between;
- what a block actually recorded lived in the UI at the time it ran, not in
  anything that travels with the results;
- reproducing a survey six months later means reconstructing the panel state
  from the sidecars it produced.

The modulation plugin *did* have a protocol runner — a TOML list of timed `MOD`
steps. It was undocumented, unreferenced by any feature brief, drove raw DAC
codes rather than calibrated optical parameters, explicitly refused the optical
warp modes, and recorded nothing. It was removed.

## Decision

A1 gains a declarative protocol: a file naming every axis for every recording,
run by a supervisor built like the frequency ladder. Two front-ends produce the
same flat list of points, chosen by file extension, so nothing downstream knows
which was used.

**CSV — one row per recording, and the one to reach for.** One line is one
recording, every parameter is a column, and the file opens in a spreadsheet or
comes straight out of a script:

```csv
label,mean_u,frequency_hz,depth_a,duration_s,settle_s,role
floor,0.50,10,0.02,20,3,background
windows,0.50,10,2.00,20,3,pilot
ladder,0.40,1,0.80,40,4,
ladder,0.40,200,0.80,10,2,
```

Columns are located **by header name**, so their order does not matter and one
can be omitted entirely; blank lines and `#` comments are skipped; a blank cell
falls back to the default. Errors carry the **file line number**, because that
is what an editor and a spreadsheet both show.

Two capabilities fall out of the row form that the block form cannot express
without one block per value:

- **Per-recording duration and settle.** A 1 Hz point needs 40 s to cover
  enough cycles and a 200 Hz point does not. This was the concrete ask.
- **A `role` column** (`normal` / `pilot` / `background`), so a file can carry
  its own references — background floor first, pilot to freeze the ON/OFF
  windows, then the points scored against them. A survey becomes a complete
  measurement rather than something that needs two button presses first.

**TOML — blocks and ranges.** Kept because it expresses a dense regular sweep
compactly, which a 96-row CSV does not:

```toml
[defaults]
duration_s = 10
settle_s   = 2.0

[[block]]
name         = "frequency-ladder"
mean_u       = [0.3, 0.6]
frequency_hz = { min = 1.0, max = 200.0, points = 6, spacing = "log" }
depth_a      = 0.8
duration_s   = 20
```

Each axis takes a single value, an explicit list, or a `{ min, max, points }`
range with `linear` (default) or `log` spacing. A block records the full
product of its three axes.

**`I_k` is `ū`.** The third axis is the normalized cycle-mean lobe point the
modulation plugin already exposes — dimensionless, not physical flux, but the
one control that moves the mean illumination without touching the depth. It is
driven by a new contract command, `ModulationCommandV1::SetOperatingPoint`,
scoped exactly like its two siblings: leased only, calibrated method only, and
the owner parks the operator's own value on the first retarget so `end_lease`
hands it back.

**In the block form, points run `ū` outermost, then `f`, then `a`.** That is the
order of how expensive each change is to settle — the operating point makes the sensor
re-adapt, a frequency has to be confirmed against the phase-0 trigger, and the
depth is the cheap innermost step. Any other nesting spends the run settling.

**All three axes are commanded at every point.** Not just the ones that
changed: a protocol states a whole operating condition, and a point that
inherited an axis from its predecessor would be recorded under parameters the
file does not name. A point waits for all three retargets to be acknowledged
before recording — recording after two of them would file the run under a
condition the bench was not at.

**The file's `duration_s` wins over the panel's.** A survey whose recording
lengths silently came from the UI would not be reproducible from the protocol
alone.

**Validation is up front.** Ranges, spacing, bounds and the total point count
are checked on the button press, before the drive moves, along with the same
whole-cycle window check the frequency ladder makes against its lowest
frequency. `MAX_POINTS = 4096` catches a three-axis product with one zero too
many *before* the bench spends a night on it. The status line reports the point
count and the expected bench time before the first recording starts.

**A refused point is skipped, not fatal.** A `ū`/`a` pair that runs off the top
of the lobe is the ordinary failure in a long survey. The point is skipped
carrying the modulation owner's own wording, the run continues, and — because
the per-point message is overwritten within the same tick — the reason is kept
on the run and surfaced both in the status pane and in the closing summary.

One lease covers the whole file.

## Consequences

- A survey is a file. It can be reviewed, diffed, version-controlled and
  archived next to the data it produced.
- `plugins/stage-a-a1/protocols/example.csv` and `example.toml` ship as
  commented starting points, installed alongside the plugin, and tests parse
  both — a stale example is worse than none. The CSV test additionally asserts
  that the shipped file really does use several different durations and both
  reference roles, so it demonstrates what it claims to.
- The CSV reader shares its field splitter with the sensor readout compactor
  (`src/csv.rs`); both locate columns by name for the same reason.
- The parse/expand core is a pure module with its own tests, so the axis
  algebra is verified without a bench.
- `ū` is a normalized lobe coordinate, not a calibrated physical flux. Sweeping
  it walks the brightness axis reproducibly; converting a point to photons
  still needs the illumination calibration, exactly as before.
- The four sweep buttons remain. They are the right tool for exploring, and the
  protocol is the right tool for the run that follows.
