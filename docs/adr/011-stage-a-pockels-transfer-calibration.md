# ADR 011 — Measured Pockels transfer calibration in the modulation plugin

- **Status:** Accepted
- **Date:** 2026-07-25
- **Relates to:** ADR 006 (two-plugin split), ADR 008 (optical waveform
  inversion), ADR 010 (amplitude sweep / press-counter idiom),
  [Stage-A Pockels Transfer Calibration](../features/stage-a-pockels-calibration.md)

## Context

ADR 008 made `V_null`/`Vπ` settable and named the **measured LUT** as the
follow-up. In practice they stayed two bare number fields whose tooltip told the
operator to measure them while the software offered no way to do so. Nothing
related a DAC code to an observed photodiode value, so the whole calibrated
drive rested on numbers typed in from a nominal datasheet — exactly what the
knowledge base warns against (`methodology/pockels-waveform-linearisation.md`
§1: "Do not use nominal `Vπ` as the measurement calibration").

## Decision

### 1. The modulation plugin owns the calibration

It already owns `V_null`/`Vπ` and the DAC. It reads photodiode levels **read
only** from the control-snapshot broadcast (the same bus A1 reads for the
measured `a`), so no lease, no service command, no coordinating plugin, and no
PDQ recording are involved. The alternative — a lease-based cross-plugin
protocol like ADR 010's sweep — would have moved the calibration state away from
the parameters it calibrates for no gain.

### 2. `PhotodiodeStreamV1.level` — one additive V1 field

`PhotodiodeLevelV1 { mean_volts, peak_to_peak_volts, sample_count,
end_sample_index, clipped }`, `#[serde(default)]`.

`PhotodiodeOpticalSummaryV1` could not serve: it reports *contrast* not level,
applies the geometry transform (which needs an anchor this reading must not
depend on), and **refuses** on clipping or missing headroom — precisely at
`V_null`, where the reject-port detector is brightest. The level is deliberately
fail-open where the optical summary is fail-closed, and always **raw** detector
volts, never the plugin's RAW/EXCITATION display transform.

`end_sample_index` makes settling *provable*: a point is accepted only from a
window that began after its code was commanded plus a settle margin, on the
device sample clock. No shared wall clock, no sleeps, immune to tick jitter.

### 3. The detector geometry is an input, not an inference

The initial design assumed a free-signed amplitude would let the fit *identify*
the port. It cannot. Since `sin²` is symmetric about its peak,
`(v, p₀, p₁)` and `(v + Vπ, p₀ + p₁, −p₁)` describe the measured curve
*identically* — the data cannot say which extremum is zero excitation. This is a
fact about the optics, so it is asked (`Detector port`, default `REJECT PORT`,
which `setup/optical-path.md` settles by construction) and the fit selects the
matching representation. Guessing would place `V_null` one half-wave-voltage span off and
silently run the drive on the inverted branch.

### 4. One-dimensional harmonic fit, not a nonlinear solve

`sin²(x) = (1 − cos 2x)/2` makes the model a constant plus one sinusoid of
period `2Vπ`, which is linear in its quadrature components. For each candidate
`Vπ`, the phase (hence `V_null`) and both amplitudes come from a 3×3 solve, so
only `Vπ` is searched — a log-spaced scan plus a golden-section refine.

The rejected alternative, seeding the period from the measured extrema, breaks
on the sweeps that matter: at a realistic `Vπ ≈ 860` the DAC range holds ~2.4
lobes and the global extrema can sit whole periods apart.

Where several nulls are valid, the **lowest** in-range one wins: least voltage
across the crystal, most headroom, and predictable for the operator.

### 5. `enabled` is computed from mirrored settings only

`settings_schema()` is rendered by the **UI mirror**, which by construction
never owns the device link, a lease, a running sweep, or a fit — all of that
lives on the live worker. A first cut gated the calibration buttons on
`calibration_blocker()` and `fit.is_some()`, which disabled them *permanently*:
the mirror can never satisfy either. The buttons now gate on the one
prerequisite the mirror does know (the operator asked to connect), and the
authoritative interlocks stay worker-side, reported through the status entries
the host already takes from the worker.

**Rule for this repo:** a `SettingKind::Button { enabled }` may only depend on
state that is itself a setting. Anything else is invisible to the instance that
renders it. The same trap bit the press counters — a baseline folded into the
counter made a fresh worker swallow the operator's first press, so the modulation
plugin now uses A1's `PressLatch` (separate `counter` and `seen`) verbatim.

### 6. The sweep owns the DAC, so `send_modulation` is silent while it runs

`apply_live_plugin_snapshot` writes **every** settings key to the worker on
every sync, and most of this plugin's drive handlers call `send_modulation()`
unconditionally rather than on change. Each sync therefore re-armed the
operator's waveform on top of the code the sweep had just commanded: the board
spent the sweep playing the armed drive, every point measured the same
waveform-averaged level, and the fit correctly reported `NoModulation` on a
bench where the light was plainly modulating.

`send_modulation` now returns early while a sweep is in flight, the same shape
as the existing automation-lease guard — a sweep is simply another owner of the
DAC. Settings changed mid-sweep are withheld rather than rejected, and land on
the board when the sweep finishes: the restore prefers the *current* drive and
falls back to the command captured at sweep start.

### 7. Robust refit, and fit quality warns rather than blocks

The first cut refused to apply a fit whose residual exceeded 2 % of the detector
span. On the bench that gate fired at 20.8 % on a sweep whose plot looked
correct, and withheld a usable calibration.

Measuring the failure modes on a realistic small-signal sweep settled it: 5 mV
of noise gives 3.1 %, drift 3.2 %, hysteresis 5.5 % — but a **single stray
point gives 9.9 % while leaving `Vπ` accurate to three codes**. Residual and
correctness are not the same axis, so a residual threshold is the wrong thing to
block on. (A genuinely wrong fit — an amplifier compressing the top of the
range — gives 15.2 % *and* a `Vπ` off by 250 codes, which the plot shows
plainly.)

Two changes follow. The fit now runs twice, dropping points beyond `6 × median`
absolute residual before refitting — a median cut, because mean and standard
deviation are themselves inflated by the points being sought. And every quality
measure became a warning; the only meaningless case, no full lobe inside the
commandable range, is already refused inside `fit_transfer`, so the separate
coverage gate was dead code and was removed rather than kept.

Applying still re-validates the resulting drive and rolls back if it cannot be
armed. The sweep restores the pre-sweep drive on every exit path, and refuses to
run while a lease or protocol owns the DAC.

### 8. `ModulationStateV1.calibration_id` — one additive V1 field

Set when a measured fit is applied, `None` when the lobe was typed in by hand,
so a consumer's sidecar can cite which inversion produced a run's optical depth.
Previously unrecoverable.

## Consequences

- The reported detector level at the null is a **lower bound** on the
  total-power anchor `I_tot`, not the anchor: on the reject port the residual
  transmitted floor is not separable from it (knowledge base §4.4). The plugin
  labels it as such and derives no maximum achievable `a` from it. Freezing a
  real anchor still needs a transmitted-port power measurement.
- `V_null`/`Vπ` need neither a dark measurement nor an anchor, because the
  fitted offset and amplitude absorb both. That is what keeps this one button
  instead of a protocol.
- Ascending and descending passes are both recorded, so the hysteresis figure
  the knowledge base's acceptance test 1 asks for comes out of the normal run.
- Still an analytic `sin²` inversion, not a measured LUT. The archived record
  stores the points a LUT would need, so ADR 008's follow-up remains open behind
  the same `warp_table` interface.
- A static calibration must never be used to correct dynamic roll-off; doing so
  would manufacture the Bode curve A1 exists to measure.
