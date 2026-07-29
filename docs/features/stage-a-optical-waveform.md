# Stage-A Optical Waveform Drive

- **Crate:** `plugins/stage-a-modulation` (`waveform.rs`)
- **Firmware:** `stage-a-controller` — `MOD wave=WARP` (`stimulus_mod::configureWarp`)
- **Status:** Analytic inversion, fed by a measured `V_null`/`V_peak` pair
  ([Pockels transfer calibration](./stage-a-pockels-calibration.md)); a fully
  measured LUT remains a documented follow-up
- **ADR:** [ADR 008](../adr/008-stage-a-optical-waveform-inversion.md),
  [ADR 016](../adr/016-stage-a-lobe-endpoints-not-a-distance.md) (the lobe is two
  observed codes, not a code and a distance)

## Why

The Pockels/PBS amplitude modulator has a `sin²` transfer, so a pure DAC sine
does **not** produce a sinusoidal *optical* target. On one monotonic lobe:

```math
I(V) = I_\text{floor} + (I_\text{ceil}-I_\text{floor})\,\sin^2[\alpha (V - V_\text{null})],
\qquad \alpha = \frac{\pi}{2 V_\pi}.
```

To hit a chosen optical target the DAC must be pre-warped by inverting it:

```math
u(t) = \frac{I_d(t)-I_\text{floor}}{I_\text{ceil}-I_\text{floor}},\qquad
V(u) = V_\text{null} + \frac{2 V_\pi}{\pi}\,\arcsin\!\sqrt{u}.
```

## Targets

- **`OPTICAL_LOG_SINE`** (recommended A1 input): `ln I_d = ln I_g + (a/2)\sin\omega t`.
  The event camera responds to changes in `ln I`, so this is the clean input.
- **`OPTICAL_LINEAR_SINE`**: `I_d = I_c(1 + m\sin\omega t)`, `m = \tanh(a/2)`.

Both operate around an explicit operating point and are refused if their optical
maximum exceeds the lobe ceiling. `DAC_SINE` remains the pure-DAC sine.

## Inversion parameters (settable — you do not need a rig to start)

| Setting | Meaning |
|---|---|
| `V_null` | DAC code at the excitation **minimum** (`sin² = 0`) |
| `V_peak` | DAC code at the excitation **maximum**, on the same lobe |
| `a` | requested optical log-modulation depth `ln(I_max/I_min)` |
| `I_k` | operating illumination as a normalised lobe intensity `u_k ∈ (0,1]` |

Both endpoints are **absolute codes you observe** — sweep the DAC and read off
where the light is dimmest and where it is brightest. The quarter wave
`Vπ = |V_peak − V_null|` is derived, never typed: an earlier form asked for
`Vπ` as a *distance* directly beneath `V_null` as a code, and entering the
brightest code there puts maximum light at `I_k ≈ 0.5` with a null back at
`I_k = 1` (ADR 016). `I_k = 1` now holds exactly at `V_peak` by construction.

The pair is resolved against the **DAC** range, not the `max_level` ceiling —
where the crystal nulls and peaks is a fact about the bench. A ceiling that cuts
the lobe short is reported against the code it actually blocks (*"the modulation
peak needs DAC code 2600, above the max limit 2400"*), not against the lobe.

A pair entered running downward in code (`V_peak < V_null`) is accepted: the
transfer repeats every `2Vπ`, so the drive uses the ascending branch one period
below, which rises into the same measured maximum, and says so in the status
pane. A degenerate pair, or one whose lobe fits nowhere inside `0..max_level`,
is refused rather than armed.

The status pane spells the resolved lobe out in codes —
`Lobe: Vπ = 860 codes — I_k 0 → 1630 (min light), 0.5 → 2060, 1 → 2490 (max
light)` — so a wrong endpoint is visible without measuring anything.

### Fixed operating point `I_k`, swept depth `a`

`I_k` is the geometric-mean point the modulation swings around:
`u(t) = u_k·exp[(a/2) sin ωt]` (log) or `u_k·(1 + m sin ωt)` (linear). **Hold
`I_k` fixed and sweep `a`** for one response curve. The drive is refused
(`Saturates`) when the peak `u_k·exp(a/2) > 1` — lower `I_k` or `a`.

`CONST` is the exception because it does not modulate: it maps only `I_k`
through the inverse lobe and ignores `a`. For example, `V_null=1630`,
`V_peak=2490` gives DAC `2490` at `I_k=1` and DAC `1685` at `I_k=0.01`.
Periodic modes still require the headroom above. Invalid setting changes are
rejected transactionally, so the UI retains the last applied value instead of
showing a target that the board never received. Photodiode RAW/EXCITATION mode
does not participate in this DAC calculation.

### Drive method and hard ceiling

Under `CALIBRATED`, `V_null`/`V_peak`/`I_k`/`a` define the operating band directly.
Under `MANUAL`, the Power + Min-threshold DAC endpoints are passed through the
forward `sin²` transfer and converted to the target law's effective `(I_k, a)`;
the same inverse-warp implementation then fills that band.

Warp codes are absolute lobe codes and cannot be rescaled without distorting the
target. The plugin therefore **refuses** any drive whose peak exceeds the
always-visible `max_level` hard ceiling. Raise the max limit, or lower the
operating band / `I_k` / `a`, to fit. Under `CALIBRATED` the DAC *floor* can no
longer be breached — every emitted code lies between the two endpoints — so only
the ceiling is ever reported.

### Modulation reference range

For the current method the plugin reports the resolved DAC lower endpoint,
upper endpoint, constant hold code, and peak-to-peak swing.

### Measured parameters (built) and the measured LUT (still future)

`V_null`/`V_peak` are no longer typed in from a datasheet: the
[Pockels transfer calibration](./stage-a-pockels-calibration.md) sweeps settled
constant DAC codes, reads the photodiode level at each, and fits the lobe those
two parameters describe. The analytic `sin²` inversion above is unchanged — it is
now fed measured parameters.

The fully measured **LUT** remains open: keep the swept `(code → optical level)`
table for one monotonic lobe and invert it directly instead of the analytic
form, dropping in behind the same `warp_table` interface and superseding
`V_null`/`V_peak` entirely. The calibration record already archives the points such
a table would need.

## Wire form (firmware line limit)

The command line is capped at 192 bytes, too small for a 256-code table, so the
plugin computes and validates the warp table locally (for the operator preview
and range guard) but sends the compact **parameters**:

```
MOD wave=WARP freq_mhz=<f> target=<LOG_SINE|LINEAR_SINE> a_milli=<a·1000> u_k_milli=<u_k·1000> v_null=<code> v_pi=<code>
```

The firmware rebuilds the identical 256-entry DAC table with the same formula
(`stimulus_mod::normalisedIntensity` + `dacForU`) and plays it back at the drive
frequency. A chunked **table upload** command is the natural extension for the
measured LUT.

## Relationship to the measured `a`

The requested `a` here is a *drive* target. The realised optical depth is always
the photodiode-measured `a` from the [photodiode plugin](./stage-a-photodiode.md)
(estimator geometry, rejected-complement corrected), never the commanded value.

## Tests

`cargo test -p augur-plugin-stage-a-modulation waveform` verifies both targets
stay in the DAC range, that feeding the warp table back through the `sin²` lobe
recovers the intended optical intensity, that the recovered log-contrast matches
the requested `a`, that a manual DAC band round-trips through
`OpticalDrive::from_dac_band`, and that invalid depth/inversion and lobe overruns
are refused. It also pins the endpoint form: that `I_k` rises monotonically to
the measured maximum for any observed pair, that a pair measured downward folds
onto the branch into the same peak, and — as a regression witness for the bench
report of 2026-07-28 — that entering the brightest *code* where the quarter-wave
*distance* belongs is what peaked the light at `I_k = 0.5`.
`cargo test -p stage-a-io mod_warp` covers the mock command surface.
The modulation-plugin tests also pin the full-lobe `CONST` values above and
verify that a rejected periodic `I_k` change cannot diverge from the board target.
