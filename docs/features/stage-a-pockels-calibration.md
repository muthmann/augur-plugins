# Stage-A Pockels Transfer Calibration

- **Crate:** `plugins/stage-a-modulation` (`calibration.rs`)
- **Depends on:** `stage-a-photodiode` publishing `PhotodiodeStreamV1.level`
- **Status:** built
- **ADR:** [ADR 011](../adr/011-stage-a-pockels-transfer-calibration.md),
  [ADR 016](../adr/016-stage-a-lobe-endpoints-not-a-distance.md) (what the two
  settings ask for)
- **Knowledge base:** `methodology/pockels-waveform-linearisation.md` §4,
  `setup/optical-path.md`

## Why

`V_null` and `V_peak` drive every calibrated waveform through the optical inversion
([Stage-A Optical Waveform Drive](./stage-a-optical-waveform.md)), but they were
two bare number fields whose tooltip said *"measure it; do not trust nominal
Vπ"* — with no way to measure it. Nothing in the UI connected a DAC code to an
observed photodiode value, so the operator had to hand-sweep `CONST`, watch a
chart in another panel, and do the arithmetic by eye.

## What it does

One button. The modulation plugin steps settled `CONST` DAC codes across
`0..max_level` (49 points up, then the same 49 back down, ~20 s), reads the
photodiode level at each, and fits the lobe:

```math
P(c) = p_0 + p_1 \sin^2\!\left[\frac{\pi (c - V_\text{null})}{2 V_\pi}\right]
```

The fit is then reviewed and applied by a second, explicit press.

## Why the modulation plugin owns it

It already owns the `V_null`/`V_peak` settings and the DAC. The fit still
derives the internal span `Vπ = |V_peak - V_null|`. The host broadcasts every plugin's
control snapshot to every plugin's inbox, so it reads photodiode levels **read
only** — no lease, no service command, no coordinating plugin, and no
photodiode recording. The photodiode simply needs to be connected.

## Three things the physics forces

**The detector port is an input, not a result.** `sin²` is symmetric about its
peak, so `(v, p_0, p_1)` and `(v + V_\pi, p_0 + p_1, -p_1)` fit the measured
curve *identically* — the data cannot say which extremum is zero excitation.

The setting asks one observable question: *when the light reaching the sample
gets brighter, does the photodiode reading go up or down?* Stage-A's photodiode
sits on the PBS **reject** port and reads the light the sample does not get,
`I_pd = I_tot − I_exc`, so it falls as the sample brightens — and reads its
**maximum** at `V_null`. That is `REJECT PORT`, the default. `DIRECT` is for a
detector watching the sample beam itself. Declaring it wrong places `V_null`
one half-wave-voltage span off and runs the drive on the inverted branch.

**The shape needs no dark measurement and no anchor.** `p_0` absorbs the dark
level and any DC offset; `p_1` absorbs the front-end gain. `V_null` and `Vπ`
are immune to both, which is why this procedure is one button and not a
protocol.

**The absolute scale is *not* recoverable here.** On the reject port the
residual transmitted floor cannot be separated from the total-power anchor
`I_tot` (knowledge base §4.4). The detector level at the null is therefore
reported as a **lower bound** on `I_tot`, explicitly not as the anchor, and no
maximum achievable `a` is derived from it. Freezing a real anchor still needs a
transmitted-port power measurement.

## How the fit works

Because `sin²(x) = (1 − cos 2x)/2`, the model is a constant plus **one sinusoid
of period `2Vπ`**, and a sinusoid of known period is linear in its quadrature
components. So for each candidate `Vπ` the phase (hence `V_null`) and both
amplitudes come from a 3×3 linear solve, and only `Vπ` is searched: a
log-spaced scan over every period the sweep can resolve, then a golden-section
refine.

Seeding the period from the measured extrema — the obvious approach — breaks on
exactly the sweeps that matter. At a realistic `Vπ ≈ 860` the DAC range holds
~2.4 lobes, so the global minimum and maximum can sit whole periods apart.

Several nulls are valid when a sweep spans multiple lobes; the fit reports the
**lowest** one whose `[V_null, V_null + Vπ]` fits inside the max limit — least
voltage across the crystal, most headroom, and a rule the operator can predict.

## Settling is proven, not timed

Every published level carries `end_sample_index` and `sample_count` on the
device sample clock. A point is accepted only from a window that *began* at
least `SETTLE_SAMPLES` (2 000 ≈ 100 ms at 20 kSa/s) after its code was
commanded. No shared wall clock, no sleeps, immune to control-tick jitter.

## The sweep owns the DAC while it runs

`send_modulation` is silent for the duration. The host re-applies the *whole*
settings snapshot on every sync and most drive handlers push to the board
unconditionally, so without this the operator's armed waveform would be
re-armed on top of every commanded code — the board would play the armed drive
through the sweep, every point would read the same waveform-averaged level, and
the fit would report "the detector level did not change" on a bench where the
light was plainly modulating. Same shape as the automation-lease guard: a sweep
is another owner of the DAC.

Settings changed mid-sweep are withheld, not rejected, and reach the board when
the sweep ends — the restore prefers the current drive and falls back to the
command captured at sweep start.

## Interlocks

The sweep refuses to start, and aborts if any becomes true mid-run, unless:
hardware effects are allowed on this instance, the command port is connected,
**no automation lease is held** (A1 must not be sweeping the drive at the same
time), no protocol is running, and a photodiode level is arriving.

It always restores the pre-sweep drive — on completion, abort, stop press,
disconnect, or a stalled stream. A calibration sweep leaves the bench as it
found it.

## Robustness: strays are dropped, the rest is a warning

The fit runs twice. The first pass finds the period; points whose residual
exceeds **6× the median** absolute residual are then dropped and the fit is
repeated on what is left. The cut is on the median, not the mean or standard
deviation, because those are themselves dragged out by the very points being
looked for. `6 × median` is roughly 4σ for Gaussian noise, so ordinary scatter
survives untouched.

This matters because of how the numbers behave in synthetic stress tests. The
following table is test-model output, not a bench measurement:

| Condition | Residual | Fitted `Vπ` |
|---|---|---|
| clean | 0.0 % | 860 |
| 5 mV noise | 3.1 % | 864 |
| 10 mV drift across the sweep | 3.2 % | 861 |
| 10 mV hysteresis | 5.5 % | 861 |
| **one stray point** | **9.9 %** | **863** |
| amplifier compressing the top of the range | 15.2 % | 1110 ✗ |

A single bad sample inflates the residual fivefold while leaving `Vπ` accurate
to three codes — and it is invisible in the plot. That is why the residual
**warns and never blocks**: blocking on it withholds a good calibration for a
bad reason. A residual that stays high after rejection, with a visibly poor
overlay, is the real signal — and as the last row shows, it comes with a `Vπ`
that is wrong in a way the plot makes obvious.

There is deliberately no absolute minimum voltage. The earlier implementation
rejected every detector span below **10 mV**, while the real Stage-A
photodiode commonly reads only about **0.5–15 mV**. The fit now compares the
between-code sweep span with the median `peak_to_peak_volts` measured inside
the settled CONST windows. A repeatable millivolt-scale lobe is accepted; a
putative lobe no larger than the detector's own typical within-window
excursion is rejected as unresolved. The regression suite includes a 4 mV
transfer that the old threshold always refused.

The fit is **never** applied automatically, and applying re-validates the
resulting drive: a calibration that cannot be armed is rolled back rather than
stored. Warnings surface as `Check:` lines in the status:

| Warning | Meaning |
|---|---|
| residual > 5 % of the span | compare fit and points in the plot before trusting `Vπ` |
| points dropped | a couple is ordinary; a large share means the sweep is the problem |
| hysteresis > 5 % | the cell is drifting, or the settle time is too short |
| clipped points | the extremum they sit on is not where the fit thinks it is |

There is no separate "lobe coverage" gate: `fit_transfer` already refuses a
sweep in which no full lobe fits inside the commandable range, so `Vπ` is always
measured rather than extrapolated by the time a fit exists.

## The transfer-curve view

A `LineSeriesWindow` host view, `Pockels transfer curve`:

- **before any sweep** — the lobe the *configured* `V_null`/`V_peak` claim, on a
  normalised `u` axis, with markers at `V_null` and `V_peak`. This works
  with no hardware attached and is the answer to "what are these two numbers".
  The markers are the settings themselves: both are absolute DAC codes, so the
  plot can be read straight back into the two fields (ADR 016).
- **after a fit** — `measured ↑`, `measured ↓`, the fitted curve, and (while
  they differ) the configured lobe on the fit's own scale, in detector volts.

## Provenance

Applying writes `pockels-<timestamp>.json` into the optional calibration folder
(points, fit, geometry, residual, hysteresis, and the anchor caveat) and sets
`ModulationStateV1.calibration_id`, so a consumer's sidecar can cite which
inversion produced a run's optical depth. Leaving the folder empty applies the
fit without archiving, and says so.

## Dual-instance note

The host renders `settings_schema()` from the **UI mirror**, which never owns
the device link, a lease, a sweep, or a fit. A `SettingKind::Button { enabled }`
may therefore only depend on state that is itself a setting — anything else is
invisible to the instance that draws it and disables the button forever. The
calibration buttons gate on "the operator asked to connect"; every real
interlock is enforced on the worker and reported in the status lines, which the
host does take from the worker.

## Settings

| Key | Meaning |
|---|---|
| `detector_geometry` | which PBS port the photodiode watches (`REJECT PORT` default) |
| `calibrate` | measure the transfer curve; press again to abort |
| `calibrate_apply` | write the reviewed fit into `V_null`/`V_peak` |
| `calibration_dir` | optional archive folder for the calibration record |

`V_null`/`V_peak` remain directly editable as the manual override.

## Verification

- `calibration.rs` unit tests recover a known lobe from **both** ports, across
  a multi-lobe sweep, and with a null at code 0; they check the geometry input
  selects between the two equivalent representations, accept a resolved
  sub-10-mV transfer, and ensure flat/noise-level sweeps, short sweeps, and
  out-of-range lobes are refused.
- An end-to-end test runs the sweep against the mock board, synthesizing the
  light the reject-port detector *would* report for whatever code the board is
  actually holding — ground truth for commanding, settle gating, point
  collection, the fit, and the drive restore.

## Limits

- Analytic `sin²` inversion, not a measured LUT (the knowledge base's eventual
  target); the calibration record stores the points a LUT would need.
- Dark level and the total-power anchor remain separate measurements.
- Static transfer only. A static calibration must never be used to correct
  dynamic roll-off — that would manufacture the Bode curve A1 measures
  (knowledge base "Gotchas").
