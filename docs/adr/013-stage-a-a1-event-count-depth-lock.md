# ADR 013 — Stage-A A1 exact-event-count depth lock (`a₀`)

- **Status:** accepted (2026-07-25)
- **Relates to:** ADR 009 (recording coordinator), ADR 010 (amplitude sweep via
  leased `SetOpticalDepth`), ADR 011 (measured Pockels transfer calibration),
  ADR 012 (the contrast geometry the measured `a` comes from),
  [Stage-A A1 Exact Event Count](../features/stage-a-a1-event-count.md)

## Context

The minimum-depth workflow sweeps the depth `a` at one frequency and fits `a50`.
The **exact-event-count** workflow is the complement: freeze **one** depth

```math
a_0=\ln\!\left(\frac{I_{\mathrm{exc,max}}}{I_{\mathrm{exc,min}}}\right),
\qquad I_\mathrm{exc}=I_\mathrm{tot}-I_\mathrm{pd}
```

and hold **that measured value** constant while the frequency varies, so the
event count per half-cycle is compared across `f` at equal optical contrast.

`a₀` is defined on the **photodiode-measured** log contrast, never on a DAC
excursion. That is exactly where the existing sweep path stops short: ADR 010
drives `SetOpticalDepth { depth_a_milli }` **open-loop**, trusting the measured
Pockels inversion (ADR 011) to turn a commanded depth into an optical one, and
then only *waits* for the measured `a` to arrive. The inversion is static, so as
`f` rises the drive electronics and the crystal response roll off and the
delivered depth falls short of the commanded one. Waiting cannot fix a
systematic gain error: the sweep would hit its 30 s settle cap and record a
point at the wrong depth (with the measured value honestly in the sidecar, but
the run wasted).

A second, subtler problem: ADR 010 already notes that "the operator's own
`depth a` re-applies on the next modulation settings sync after release". A
depth found in one operator action and recorded in a *later* one can therefore be
silently overwritten between the two.

## Decision

**1. A closed-loop `a₀` lock in A1, separate from recording.** A *Find a₀*
button runs a small state machine — `AcquiringLease → (per trial) SettingDepth →
Measuring → …release` — that iterates

```math
a_\text{cmd} \leftarrow a_\text{cmd}\cdot\frac{a_0}{a_\text{measured}}
```

until the photodiode-measured `a` is within an absolute tolerance of `a₀`
(default ±0.02), at most 8 trials, each correction capped at ×2/÷2 and clamped
to the owner's `0.01..=6.0`. The delivered depth is proportional to the commanded
one to first order, so this converges in two or three trials while absorbing
whatever roll-off the frequency introduces. It reuses the ADR 010 contract
command unchanged — no new modulation command, no optical math outside its owner.

The lock **records nothing** and releases the lease with `safe_off = false`, so
the drive stays exactly where the lock left it.

Measurement hygiene: readings are only taken after the operator's settle dwell
has passed, and one reading per **fresh** photodiode `service_revision` (three
per trial), so a slow publisher is not averaged once per control tick. A
measured `a ≤ 0`, a missing optical summary, or an owner rejection ends the lock
with the owner's own wording — a refused depth *is* the "`a₀` unreachable at this
operating point" answer.

**2. The result is data, not a transient.** Each finished lock is stored as one
row per frequency — `frequency_hz`, `target_a`, `commanded_a`, `measured_a`,
`trials`, `converged`, clip fractions, timestamp — replacing any earlier row
within 1 % of the same frequency, shown in an `a₀ lock table` host view, and
mirrored to `a0_locks.json` in the output folder so the found depths survive a
restart and can be cited offline. Non-converged attempts are kept for the record
but never arm a recording.

**3. Recording replays the locked depth under the lease.** *Record a₀ point*
does **not** simply record at whatever the drive currently is. It runs the ADR 010
sweep machinery as a **one-point sweep of a new kind**: lease → command the
locked `a_cmd` → confirm the measured `a` holds `a₀` within the lock tolerance →
record through the unchanged coordinator → release. This gives three things at
once: the depth is re-asserted (immune to an intervening settings sync), the
lease locks the operator's modulation settings out for the whole point, so
"never change amplitude during the recorded interval" is enforced rather than
trusted, and the point is one button press.

To express this, a sweep point became a pair — what the drive is **commanded**
to, and the depth it is **expected to measure**. The amplitude sweep sets both
equal (it trusts the calibration); an event-count point deliberately does not,
and the difference *is* the absorbed roll-off.

**4. Naming and provenance.** Event-count points take the role suffix `_ec` and
carry their **frequency** in the stem (`…_ec_f50Hz`, `…_ec_f0p5Hz`) instead of a
sweep-point index, because one measurement id spans the whole frequency sweep at
the single frozen depth. The sidecar gains `sweep.commanded_a` and an
`[a0_lock]` section (target, commanded, measured-at-lock, frequency-at-lock,
trials, converged, locked-at), and both recorders' own sidecars carry the same
values as string metadata.

**5. What stays the operator's.** The flux point, camera configuration, ROI/mask,
pedestal, bias set, gates, reference epoch, the frequency itself, the
`I_tot` anchor, the zero-depth background and the pilot (already separate
buttons), the randomised frequency order, the interleaved low-frequency
reference, and the repeated blocks. A1 adds exactly two buttons per frequency —
*Find a₀* and *Record a₀ point* — because the protocol's ordering and
randomisation decisions are scientific, not mechanical.

## Consequences

- A1's scoped hardware reach is unchanged in kind (still only the armed drive's
  depth, still only while leased) but now closed-loop: it reads the photodiode
  to decide what to command.
- The recorded amplitude is provably the measured `a₀`, not a calibrated guess,
  at every frequency — including frequencies where the static Pockels inversion
  is no longer accurate.
- `a₀` itself is **not** frozen numerically in this repository: it is an operator
  input, to be chosen from the low-frequency scout (several events per
  pixel-half-cycle, still proportional, refractory-safe at the top frequency).
  The plugin default is a placeholder.
- The refractory condition `2 f a₀/C ≪ 1/τ_refr` is *not* checked in the plugin;
  it is a choice made once when `a₀` is picked, and stays with the operator.
- Re-locking after changing the flux point, the calibration or `a₀` is required:
  a stored lock is only armed for a matching frequency **and** a matching `a₀`,
  and *Clear a₀ lock table* exists for the rest.
