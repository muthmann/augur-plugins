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

The installed cell is the Excelitas **LM 0202**, part `84502049000`: four
KD*P crystals, 3×3 mm aperture, 400–850 nm, 5 W, nominal half-wave voltage
`210 V ±10 %` at 633 nm. The catalog value is a hardware plausibility check,
not a drive calibration; the plugin uses the measured DAC-domain
`V_null`/`Vπ`.

## Targets

- **`OPTICAL_LOG_SINE`** (recommended A1 input):
  `ln u = ln u_g + (a/2)\sin\omega t`.
- **`OPTICAL_LINEAR_SINE`**:
  `u = u_c(1 + m\sin\omega t)`, `m = \tanh(a/2)`.

Both operate around an explicit operating point and are refused if their optical
maximum exceeds the lobe ceiling. Their headroom tests use their own target law;
the linear target is not tested against the log-sine endpoints. `DAC_SINE`
remains the pure-DAC sine.

## Inversion parameters (settable — you do not need a rig to start)

| Setting | Meaning |
|---|---|
| `V_null` | DAC code at the excitation minimum (`sin² = 0`) |
| `Vπ` | DAC-code **half-wave-voltage span** from `V_null` to the excitation maximum |
| `a` | requested peak-to-trough natural-log contrast `ln(I_max/I_min)` |
| `ū` | requested dimensionless floor-subtracted **cycle mean** in `(0,1]` |

Get `V_null`/`Vπ` from a two-point check (code giving min light, code giving max
light on one lobe) or from nominal `Vπ ÷ driver volts-per-code`. The drive is
refused (never silently clamped) if `V_null + Vπ` overruns `0..4095`.

### `u` is not the physical flux point `I_k`

The modulation setting `ū` is a normalized cycle mean, not photons per pixel
per second. For a log target the plugin derives the geometric pedestal
`u_g=ū/I_0(a/2)` before generating/sending the WARP parameters; for a linear
target the arithmetic centre is already `u_c=ū`. The physical A1 quantity
`I_k` is the cycle-mean local excitation flux after all optics and
sample/spatial mapping. A1 therefore records a separate, required
`flux_point_id`; it never derives absolute `I_k` from `ū`.

For the log target, `u(t)=u_g exp[(a/2)sin ωt]` and
`⟨u⟩=u_g I_0(a/2)=ū`. The implemented Bessel normalization therefore holds
the normalized—and, for a stable affine floor/span, physical—cycle mean while
`a` is swept, to the existing `u_k_milli` wire resolution. The acknowledged
state and A1 sidecar publish both the requested mean and the resolved mean after
that milli-unit quantization, plus the internal `u_g`. The independent flux
calibration still supplies the absolute local `I_k` and verifies it on the
bench.

`CONST` maps only `u`
through the inverse lobe and ignores `a`. For example, `V_null=1630`,
`Vπ=860` gives DAC `2490` at `u=1` and DAC `1685` at `u=0.01`.
Periodic modes still require the headroom above. Invalid setting changes are
rejected transactionally, so the UI retains the last applied value instead of
showing a target that the board never received. Photodiode RAW/EXCITATION mode
does not participate in this DAC calculation.

### Drive method and hard ceiling

Under `CALIBRATED`, `V_null`/`Vπ`/`ū`/`a` define the operating band directly.
Under `MANUAL`, the Power + Min-threshold DAC endpoints are passed through the
forward `sin²` transfer and converted to the target law's effective `(u, a)`;
the same inverse-warp implementation then fills that band.

Warp codes are absolute lobe codes and cannot be rescaled without distorting the
target. The plugin therefore **refuses** any drive whose peak exceeds the
always-visible `max_level` hard ceiling. Raise the max limit, or lower the
operating band / `ū` / `a` / `Vπ`, to fit.

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

This matters when the floor is finite. The implemented drive makes `u` obey the
requested target, so with `I=I_floor+(I_ceil-I_floor)u` the realised physical
contrast is

```math
a_\mathrm{phys} =
\ln\frac{I_\mathrm{floor}+S u_g e^{a/2}}
        {I_\mathrm{floor}+S u_g e^{-a/2}},
\qquad S=I_\mathrm{ceil}-I_\mathrm{floor}.
```

It equals the requested `a` only for zero floor (or if contrast is explicitly
defined on floor-subtracted intensity). The measured photodiode `a` is therefore
the authority, and quantitative A1 acquisition requires the residual/floor
validation or the measured-LUT extension.

## Wire form (firmware line limit)

The command line is capped at 192 bytes, too small for a 256-code table, so the
plugin computes and validates the warp table locally (for the operator preview
and range guard) but sends the compact **parameters**:

```
MOD wave=WARP freq_mhz=<f> target=<LOG_SINE|LINEAR_SINE> a_milli=<a·1000> u_k_milli=<internal pedestal/centre·1000> v_null=<code> v_pi=<code>
```

For log-sine, the plugin first converts the UI's `ū` to
`u_g=ū/I_0(a/2)` and transmits that backward-compatible `u_k_milli` field.
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
verify that a rejected periodic `ū` change cannot diverge from the board
target, that Bessel normalization preserves the log-sine cycle mean, and that
linear-sine headroom uses the linear target law. The resolved optical-drive
provenance test also checks the exact milli-unit value sent to the controller
and the corresponding resolved cycle mean.
