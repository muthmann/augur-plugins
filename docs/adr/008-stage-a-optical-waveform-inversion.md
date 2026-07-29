# ADR 008 — Optical waveform inversion for the Stage-A modulator

- **Status:** Accepted
- **Date:** 2026-07-20
- **Relates to:** ADR 006 (two-plugin split), `stage-a-controller` waveform drive

## Context

The Pockels/PBS amplitude modulator has a `sin²` voltage→transmission transfer.
A pure DAC sine (`DAC_SINE`) therefore produces a distorted optical waveform,
and a 50 % bias only approximately linearises the small signal. The A1
measurement wants a clean optical target — ideally a **log-intensity** sine,
because the event camera responds to changes in `ln I`.

Producing that target requires driving the DAC with the *inverse* of the `sin²`
lobe, `V(u) = V_null + (2Vπ/π)·arcsin√u`, which is not a sinusoid. The existing
firmware only synthesises a pure sine from a fixed 256-entry table scaled between
`min`/`level`, so it cannot emit the warped shape as-is. The command line is also
capped at 192 bytes, too small to upload a 256-code table inline.

## Decision

1. **Own the inversion in the modulation plugin.** `waveform.rs` computes a
   256-entry DAC warp table from an `OpticalTarget` (`LogSine`/`LinearSine`), the
   requested depth `a`, and a `LobeInversion { v_null_dac, v_pi_dac }`. It refuses
   (never clamps) a drive whose codes leave `0..4095`.
2. **Keep the inversion parameters settable.** `V_null` and `Vπ` are entered in
   DAC codes; no measurement rig is required to start. The scientifically clean
   **measured LUT** (sweep constant codes, log the photodiode, freeze the table)
   is a documented follow-up that drops in behind the same `warp_table` interface.
3. **Send parameters, not the table, over the wire.** The compact
   `MOD wave=WARP freq_mhz=… target=… a_milli=… v_null=… v_pi=…` command fits the
   192-byte limit; the firmware rebuilds the identical table with the same formula
   (`stimulus_mod::normalisedIntensity` + `dacForU`) and plays it back through a
   `warpIsr`. A chunked table-upload command is the future path for the measured
   LUT, which cannot be parameterised.
4. **Preserve `DAC_SINE`.** The pure DAC sine (firmware `SINE`) is unchanged and
   remains the default for non-optical work.
5. **Separate drive from measurement.** The requested `a` is only a drive target.
   The realised optical depth is always the photodiode-measured `a`
   (rejected-complement corrected in the `stage-a-io` estimator), never the
   commanded value.
6. **Keep drive method orthogonal to waveform mode (2026-07-23 amendment).**
   `MANUAL` defines a DAC band from Power + Min threshold; `CALIBRATED` derives
   one from `V_null`, `Vπ`, normalized `u`, and `a`. All five waveform modes remain
   available with both methods. Manual optical modes pass their DAC endpoints
   through the forward `sin²` transfer to derive `(u, a)`, then reuse the same
   inversion path. The separate `max_level` setting is the hard ceiling for
   every drive; it is no longer merely the upper bound of the Power slider.
7. **Treat constant hold separately from modulation headroom (2026-07-23
   amendment).** `CONST` maps `u` directly through the inverse lobe and ignores
   `a`; periodic modes retain target-specific headroom. A rejected
   calibrated setting is rolled back so displayed settings always describe the
   command that can actually be sent.
8. **Separate normalized transfer coordinate from physical flux and preserve
   its mean (2026-07-28 amendment).** The UI setting is the normalized
   cycle mean `ū`; it is never called the physical A1 flux `I_k`. Log-sine
   generation derives `u_g=ū/I_0(a/2)` before sending the existing WARP
   parameters. The internal value is quantized to the existing `u_k_milli`
   wire field, and the acknowledged state publishes both requested and resolved
   means, so sweeping `a` removes the analytic mean shift and makes the small
   quantization residual explicit. Finite `I_floor`
   still makes physical contrast smaller than the requested floor-subtracted
   contrast. A1 therefore records a separate physical `flux_point_id`,
   measured photodiode `a` remains authoritative, and the measured finite-floor
   LUT remains required when the analytic residual exceeds the error budget.
   `ModulationStateV1.optical_drive` publishes requested and resolved `ū`,
   internal `u_g`/`u_c`, requested `a`, target and lobe codes as an additive V1
   provenance field.

## Consequences

- The plugin, the `stage-a-io` mock, and the firmware share one small parameter
  contract and one formula; the inversion math is duplicated in Rust and C++ but
  covered by the Rust round-trip tests (`sin²(warp) ≈ target`).
- Method changes only the operating-band source; mode remains a pure waveform
  choice. The UI can therefore hide inactive parameters without filtering modes.
- Real optical output on hardware depends on firmware that supports the `WARP`
  command; until flashed, the mode is exercisable only against the in-process
  mock and the unit tests.
- The measured-LUT upgrade and the eventual `EXT_TRIGGER` camera marker (see the
  A1 analysis brief) remain the two open scientific accuracy items.
