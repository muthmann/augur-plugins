# ADR 016 — The Pockels lobe is two observed codes, not a code and a distance

- **Status:** Accepted
- **Date:** 2026-07-28
- **Relates to:** ADR 008 (optical waveform inversion), ADR 011 (measured
  Pockels transfer calibration),
  [Stage-A Optical Waveform Drive](../features/stage-a-optical-waveform.md),
  [Stage-A Pockels Transfer Calibration](../features/stage-a-pockels-calibration.md)

## Context

ADR 008 made the lobe settable as `V_null` (a DAC **code**) plus `Vπ` (a DAC
**distance** from it). On the bench on 2026-07-28 the drive behaved backwards:
excitation was brightest at normalized lobe coordinate `u = 0.5` and returned
to the null at both `u = 0` and `u = 1`, with the reject-port photodiode reading its maximum at
both ends.

Nothing was wrong with the arithmetic. Host (`waveform.rs`), firmware
(`stage-a-controller/src/stimulus_mod.cpp`) and the knowledge base
(`methodology/pockels-waveform-linearisation.md` §3) all implement the same
inverse, `V(u) = V_null + (2Vπ/π)·arcsin(√u)`, with maximum light at
`V_null + Vπ`.

What was wrong was the question the settings pane asked. `V_null (DAC code at
min light)` and `Vπ (DAC codes, null → max light)` render one above the other,
both labelled in DAC codes, and only the second is a distance. The operator
entered the **code** where the light was brightest. With a true null `N`, a true
peak `P` and `Vπ` set to `P`, the realised light is

```
I(u) = sin²( (P/(P−N)) · arcsin(√u) )
```

which peaks at `u = sin²((π/2)(1 − N/P))` — one half when `N ≈ P/2` — and
falls back to the null at `u = 1`. That reproduces the observed curve exactly,
including the symmetry, and is asserted as a regression witness in
`waveform::tests::the_brightest_code_typed_as_v_pi_is_what_used_to_peak_at_half`.

A distance is not an observable. Sweeping the DAC yields two *codes* — where the
light is dimmest and where it is brightest — and the pane asked the operator to
subtract them in their head, silently, with no way for the software to check the
result. The failure is silent by construction: any positive `Vπ` produces a
valid-looking drive, so the mistake only shows up as light that does the wrong
thing.

## Decision

### 1. The two settings are both absolute codes

`v_null_dac` (code at minimum light) and `v_peak_dac` (code at maximum light).
`Vπ = |V_peak − V_null|` is derived, never typed. Both fields are read straight
off a sweep or off the transfer-curve plot, so there is nothing to subtract and
nothing to confuse.

The mis-entry that caused this ADR cannot be expressed in the new form: the code
of the brightest point **is** what `V_peak` asks for.

### 2. `LobeInversion::resolve` is the single place a pair becomes a lobe

It returns the ascending lobe the drive inverts, or an error. Two cases beyond
the obvious one:

- **A pair measured running downward** (`V_peak < V_null`) is now expressible,
  where before it simply could not be entered — `Vπ` was constrained positive
  and the inverse only ever climbs from `V_null`. `sin²` repeats every `2Vπ`, so
  the branch one full period below the observed null rises into the very maximum
  that was measured; that branch is used and the status pane says so, because
  the codes driven are not the ones that were typed.
- **A degenerate or unreachable pair** is refused with the measurement to redo,
  rather than accepted into a drive that cannot be armed.

### 3. The lobe is resolved against the DAC, the ceiling checks emitted codes

Where the crystal nulls and peaks is a fact about the bench, so `resolve` bounds
the lobe by the DAC range (`0..=4095`) and not by the operator's `max_level`
safety ceiling. Resolving against the ceiling would have refused a perfectly
drivable `MANUAL` band merely because the lobe it is interpreted against extends
past the ceiling.

What the ceiling constrains is the codes actually emitted. The four floor/ceiling
guards in `dac_band` collapse to one — the floor cannot be breached now that
every emitted code lies between two in-range endpoints — and it names the
settings that still exist: *"the modulation peak needs DAC code 2600, above the
max limit 2400; raise the max limit or lower u / a"*.

### 4. The wire format and the fit are unchanged

`MOD wave=WARP … v_null=… v_pi=…` still carries the quarter wave, because that
is what the firmware rebuilds the table from; the derived distance goes on the
wire. `fit_transfer` still reports `v_pi_dac`, because a fitted period *is* a
distance — the endpoint form is about what an operator types, not about how the
model is expressed internally. Applying a fit writes `V_peak = V_null + Vπ`.

### 5. `v_pi_dac` remains settable, and only settable

It is absent from `settings_schema` but still accepted by `set_setting`, where
it is converted to `V_peak = V_null + Vπ`. Stored configs keep loading; nothing
new can be authored against the form that caused the mix-up.

### 6. The status pane states where normalized `u` lands

`Lobe: Vπ = 860 codes — u 0 → 1630 (min light), 0.5 → 2060, 1 → 2490 (max
light)`. The parameters are only meaningful as the codes they produce, and a
wrong endpoint is visible there without running a sweep or looking at the light.

This `u` is the dimensionless, floor-subtracted lobe coordinate. It is not the
physical A1 flux point `I_k`, which remains separately identified and measured.

## Consequences

- `u = 1` holds exactly at the measured maximum, by construction rather than
  by arithmetic that has to come out right.
- Drive rejection narrows to one honest case: the max-limit ceiling cutting the
  requested `u`/`a` short. The DAC floor can no longer be breached at all.
- A descending branch is expressible for the first time.
- **Breaking:** `v_pi_dac` no longer appears in the settings schema. Stored
  values still load through the compatibility path above, but a saved value that
  was *wrong* in the old sense (a code entered as a distance) migrates to an
  equally wrong `V_peak` — the bench pair must be re-entered once, or a
  calibration sweep re-applied.
- This does not make the calibration self-checking. Nothing yet compares the
  applied lobe against the light; the measured sweep of ADR 011 remains the way
  to establish the two codes, and this ADR only makes hand-entering them
  unambiguous.
