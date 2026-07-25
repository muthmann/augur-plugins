# Stage-A Optical Waveform Drive

- **Crate:** `plugins/stage-a-modulation` (`waveform.rs`)
- **Firmware:** `stage-a-controller` — `MOD wave=WARP` (`stimulus_mod::configureWarp`)
- **Status:** Analytic inversion, fed by a measured `V_null`/`Vπ`
  ([Pockels transfer calibration](./stage-a-pockels-calibration.md)); a fully
  measured LUT remains a documented follow-up
- **ADR:** [ADR 008](../adr/008-stage-a-optical-waveform-inversion.md)

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
| `V_null` | DAC code at the excitation minimum (`sin² = 0`) |
| `Vπ` | DAC-code quarter-wave distance from `V_null` to the excitation maximum |
| `a` | requested optical log-modulation depth `ln(I_max/I_min)` |
| `I_k` | operating illumination as a normalised lobe intensity `u_k ∈ (0,1]` |

Get `V_null`/`Vπ` from a two-point check (code giving min light, code giving max
light on one lobe) or from nominal `Vπ ÷ driver volts-per-code`. The drive is
refused (never silently clamped) if `V_null + Vπ` overruns `0..4095`.

### Fixed operating point `I_k`, swept depth `a`

`I_k` is the geometric-mean point the modulation swings around:
`u(t) = u_k·exp[(a/2) sin ωt]` (log) or `u_k·(1 + m sin ωt)` (linear). **Hold
`I_k` fixed and sweep `a`** for one response curve. The drive is refused
(`Saturates`) when the peak `u_k·exp(a/2) > 1` — lower `I_k` or `a`.

`CONST` is the exception because it does not modulate: it maps only `I_k`
through the inverse lobe and ignores `a`. For example, `V_null=1630`,
`Vπ=860` gives DAC `2490` at `I_k=1` and DAC `1685` at `I_k=0.01`.
Periodic modes still require the headroom above. Invalid setting changes are
rejected transactionally, so the UI retains the last applied value instead of
showing a target that the board never received. Photodiode RAW/EXCITATION mode
does not participate in this DAC calculation.

### Drive method and hard ceiling

Under `CALIBRATED`, `V_null`/`Vπ`/`I_k`/`a` define the operating band directly.
Under `MANUAL`, the Power + Min-threshold DAC endpoints are passed through the
forward `sin²` transfer and converted to the target law's effective `(I_k, a)`;
the same inverse-warp implementation then fills that band.

Warp codes are absolute lobe codes and cannot be rescaled without distorting the
target. The plugin therefore **refuses** any drive whose peak exceeds the
always-visible `max_level` hard ceiling. Raise the max limit, or lower the
operating band / `I_k` / `a` / `Vπ`, to fit.

### Modulation reference range

For the current method the plugin reports the resolved DAC lower endpoint,
upper endpoint, constant hold code, and peak-to-peak swing.

### Measured parameters (built) and the measured LUT (still future)

`V_null`/`Vπ` are no longer typed in from a datasheet: the
[Pockels transfer calibration](./stage-a-pockels-calibration.md) sweeps settled
constant DAC codes, reads the photodiode level at each, and fits the lobe those
two parameters describe. The analytic `sin²` inversion above is unchanged — it is
now fed measured parameters.

The fully measured **LUT** remains open: keep the swept `(code → optical level)`
table for one monotonic lobe and invert it directly instead of the analytic
form, dropping in behind the same `warp_table` interface and superseding
`V_null`/`Vπ` entirely. The calibration record already archives the points such
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
are refused. `cargo test -p stage-a-io mod_warp` covers the mock command surface.
The modulation-plugin tests also pin the full-lobe `CONST` values above and
verify that a rejected periodic `I_k` change cannot diverge from the board target.
