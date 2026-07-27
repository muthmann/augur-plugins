# Stage-A A1 Exact Event Count — the `a₀` depth lock

- **Crate:** `plugins/stage-a-a1` (`augur-plugin-stage-a-a1`)
- **Status:** Built — per-frequency `a₀` lock, one-button event-count point, and
  an unattended frequency ladder that does both at every planned `f`
- **Design:** [ADR 013](../adr/013-stage-a-a1-event-count-depth-lock.md) (the
  lock) and [ADR 014](../adr/014-stage-a-a1-frequency-ladder.md) (the ladder);
  builds on [ADR 010](../adr/010-stage-a-a1-amplitude-sweep.md) (leased
  `SetOpticalDepth`), [ADR 009](../adr/009-stage-a-a1-recording-coordinator.md)
  (the RAW + PDQ + sidecar coordinator) and
  [ADR 012](../adr/012-stage-a-contrast-geometry-is-bench-not-display.md) (the
  geometry the measured `a` is defined in)
- **Relates to:** [Stage-A A1 Analysis](./stage-a-a1.md),
  [Stage-A Pockels Transfer Calibration](./stage-a-pockels-calibration.md),
  [Stage-A Photodiode](./stage-a-photodiode.md)

## Purpose

The minimum-depth workflow sweeps `a` at one frequency to fit `a50`. The
**exact-event-count** workflow is the complement: freeze **one** depth

```math
a_0=\ln\!\left(\frac{I_{\mathrm{exc,max}}}{I_{\mathrm{exc,min}}}\right),
\qquad I_\mathrm{exc}=I_\mathrm{tot}-I_\mathrm{pd}
```

and hold that **photodiode-measured** value constant while the frequency varies,
so event counts per half-cycle are comparable across `f` at equal optical
contrast. `a₀` is a measured log contrast — **never** a DAC-code excursion, and
never the reject-port detector's own contrast (ADR 012).

## Why a lock is needed at all

`ModulationCommandV1::SetOpticalDepth` commands a depth through the *measured*
Pockels inversion (`V_null`, `Vπ`, `u_k` — see the calibration brief). That
inversion is static, so at higher frequencies the drive electronics and crystal
response roll off and the delivered optical depth falls short of the commanded
one. The amplitude sweep (ADR 010) only *waits* for the measured `a`, which
cannot correct a systematic gain error — it would hit the 30 s settle cap and
record at the wrong depth.

The lock closes that loop: it commands, measures, and corrects until the
photodiode reports `a₀`.

## What the measured `a` needs to be worth dividing by

The lock divides by the measured `a`, so a *biased* measurement is not noise —
it is a systematic push on the drive. Two properties of the photodiode estimate
therefore matter more here than anywhere else, and both are enforced:

- **Whole cycles.** `a` is peak-to-peak, so its window has to span at least one
  full modulation cycle. The photodiode sizes its contrast window from the
  phase-0 markers to cover several cycles, and **withholds `a` entirely** below
  one. A fixed 0.82 s window — what it used before — is under one cycle for
  every `f < 1.2 Hz`, exactly where the A1 plateau reference lives, and would
  have under-reported `a` and driven the depth up until it railed. Its length
  and cycle count are published as `window_seconds` / `covered_cycles`.
- **A window that has turned over.** A reading taken sooner than one window
  after a depth change still contains the old depth. The lock's per-trial dwell
  is therefore at least one window (never less than **Sweep settle (s)**), and
  its three readings are spaced by half a window so they are not three views of
  the same samples. The trial value is their **median**; if they spread by more
  than twice the tolerance the operating point is called unsettled and the lock
  aborts rather than latching onto a drifting drive.

If the ladder's lowest frequency needs a longer window than the photodiode's
ring holds, raise its **Cache length**; the refusal says so and names the
seconds needed.

## The workflow, one frequency at a time

Everything up to the references is unchanged and stays the operator's: freeze the
flux point and camera configuration, reuse the same film position, ROI/mask,
optical pedestal, bias set, gates and reference epoch as the minimum-depth
measurement, keep ON and OFF separate, and per frequency record the full-extinction
`I_tot` anchor, the zero-depth background and the high non-saturating pilot (the
existing **Record pilot** / **Record background** buttons; background reuse
across frequencies is not automated, i.e. off by default). Then:

1. Set the frequency `f` in the modulation plugin (yours — the drive is armed
   there, A1 only reads it).
2. Enter **a₀** once for the whole sweep, and press **Find a₀**. A1 leases the
   modulation owner and trims the commanded depth until the photodiode measures
   `a₀` at *this* frequency. Nothing is recorded; the drive is left at the depth
   it found and the result is stored for `f`.
3. Press **Record a₀ point (event-count)**. A1 re-applies the found depth under a
   modulation lease, waits for the measured `a` to hold `a₀`, and records one
   atomic camera RAW + photodiode PDQ + sidecar under one run id.
4. Repeat for the next frequency. Repeating independent blocks (three where
   practical) is yours — every point is one button press.

Steps 1–4 are what **Start frequency sweep** automates; see below.

## The frequency ladder (unattended)

**Start frequency sweep (find a₀ + record per f)** runs the whole ladder on
**one** modulation lease. Per point it retargets the drive's frequency
(`ModulationCommandV1::SetDriveFrequency`), waits for the phase-0 trigger to
actually report the new period, runs the `a₀` lock, and records one atomic
event-count point — then moves on.

| Control | Meaning |
|---|---|
| Sweep min f / max f (Hz) | ends of the ladder, both included |
| Frequency points | how many, **log-spaced** — `\|H(f)\|` is read per decade |
| Frequency order | ascending / descending / alternating / random (seeded) |
| Random order seed | makes the random schedule reproducible; recorded per point |
| Low-f reference every N points | re-visit the lowest frequency every N points |
| Start frequency sweep | run the ladder |
| Stop | aborts the ladder and whichever child is mid-flight |

What it guarantees:

- **One lease for the whole ladder.** The lock and the recording run on the
  ladder's lease instead of taking their own, so the operator's drive settings
  are locked out from the first frequency to the last — the amplitude provably
  cannot move between a lock and the point that replays it. The owner parks the
  operator's frequency *and* depth on the first retarget and hands both back
  when the lease is released.
- **The trigger confirms the frequency, not the firmware.** An ACK says a table
  was accepted; the phase-0 markers say the light is modulating at that rate.
  A point only starts once enough markers at the *new* period agree with the
  commanded frequency.
- **Nothing from the previous frequency survives.** Retained markers and events
  are dropped on every frequency change — the measured period is their mean
  spacing, so keeping them would confirm the new frequency against a mixture.
  **Pilot windows are dropped too**: windows frozen at one period do not
  transfer to another, and scoring a point in the wrong window is a silent
  error. Re-freeze a pilot per frequency if you need pilot-frozen windows.
- **A bad point is skipped, not fatal.** A frequency whose `a₀` is unreachable,
  whose trigger never confirms, or whose recording fails is skipped and named in
  the final summary; the remaining decades are still recorded. The lock table
  keeps the failed attempt.
- **The plan is checked before the drive moves.** The lowest planned frequency
  decides whether the photodiode can measure `a` at all, so it is checked up
  front — not at the ninth point, two hours in.

Every point's sidecar gains a `[frequency_sweep]` section: `min_f`, `max_f`,
`planned_points`, the position in the **executed** order, the order name, the
seed, whether the point is an interleaved reference, and the requested
frequency (`[trigger]` carries what the markers measured).

### What the ladder still does not do

The flux point `I_k`, the camera configuration, ROI/mask, pedestal, bias set,
gates, the `I_tot` anchor, the zero-depth background, the pilot, and repeating
independent blocks stay the operator's. `a₀` itself is an operator input, and
the refractory condition `2·f·a₀/C ≪ 1/τ_refr` is **not** checked — verify it at
your highest planned frequency when you pick `a₀`.

## Controls

| Control | Meaning |
|---|---|
| a₀ (measured log contrast) | the one photodiode-measured depth held across the whole frequency sweep |
| a₀ tolerance (absolute) | convergence band on `|measured a − a₀|`; also the settle band an event-count point must hold before it records (default ±0.02) |
| Find a₀ (lock the drive depth) | closed-loop trim of the commanded depth at the current frequency; records nothing, stores the result, leaves the drive there |
| Record a₀ point (event-count) | re-applies the locked depth under the lease and records one atomic frequency point (`…_ec_f<f>Hz`) |
| Clear a₀ lock table | drops every stored lock and rewrites `a0_locks.json` |
| Stop (abort recording / sweep) | also aborts a running lock |

The **Sweep settle (s)** value in the Recording section is reused as the
per-trial dwell before the lock starts averaging.

## The lock loop

```math
a_\text{cmd} \leftarrow a_\text{cmd}\cdot\frac{a_0}{a_\text{measured}}
```

- Starts from an earlier lock at the same frequency when one exists, otherwise
  from `a₀` itself (the calibrated open-loop guess).
- Converges when `|measured − a₀| ≤ tolerance`; at most **8 trials**, each
  correction capped at ×2/÷2 and clamped to the owner's `0.01..=6.0`.
- Per trial it waits the settle dwell, then averages **three fresh** photodiode
  optical summaries (one per new `service_revision`, so a slow publisher is not
  averaged once per control tick); it evaluates early with fewer readings only if
  the 30 s measurement deadline hits first.
- Ends with the owner's own wording when a commanded depth is **rejected** (lobe
  ceiling, DAC limit) — that is the "`a₀` is unreachable at this operating point,
  lower `a₀` or `I_k`" answer — or reports the drivable limit when the correction
  rails at `0.01`/`6.0`.
- Photodiode clipping above 1 % is called out in the result message and stored
  with the lock: a clipped window makes the measured `a` a truncated estimate.
- Releases the lease with `safe_off = false`, so the drive holds the found depth.

## The lock table

One row per frequency (a re-lock within 1 % of a stored frequency replaces it):
frequency, target `a₀`, commanded `a`, measured `a`, trials, state, locked-at.
Visible as the **A1 a₀ locks** host view and mirrored to
`<output folder>/a0_locks.json`, so the found depths survive a restart and can be
cited offline. A non-converged row is kept for the record but **never** arms a
recording; a stored lock only arms an event-count point when both its frequency
**and** its `a₀` still match the current settings.

## Why recording re-applies the depth

*Record a₀ point* does not simply record at whatever the drive currently is. It
runs the ADR 010 sweep machinery as a one-point sweep of a new kind — lease →
command the locked depth → confirm the measured `a` holds `a₀` → record → release
— which buys three things:

- the depth is **re-asserted**, so an intervening modulation settings sync (which
  re-applies the operator's own `depth a`) cannot silently spoil the point;
- the lease **locks the operator's modulation settings out** for the whole point,
  so *"never change amplitude during the recorded interval"* is enforced rather
  than trusted;
- it stays one button press.

Internally a sweep point is now a pair: the depth the drive is **commanded** to
and the depth it is **expected to measure**. The amplitude sweep sets both equal;
an event-count point deliberately does not, and the difference is the roll-off
the lock absorbed.

## Naming and sidecar

Event-count points use the role suffix `_ec` and carry the **frequency** in the
stem instead of a sweep-point index — one measurement id spans the whole
frequency sweep at the single frozen depth:

- `<id>/<id>_<ts>_ec_f50Hz.raw` (+ the host's own `<stem>.toml`)
- `<id>/<id>_<ts>_ec_f50Hz_pd.pdq` + `_pd.json`
- `<id>/<id>_<ts>_ec_f50Hz_config.toml`

Sub-hertz frequencies keep the decimal as `p` (`f0p5Hz`). The A1 sidecar adds
`sweep.commanded_a` and an `[a0_lock]` section (`target_a`, `commanded_a`,
`measured_a_at_lock`, `frequency_hz_at_lock`, `trials`, `converged`,
`locked_at_utc`); both recorders' own sidecars carry `a0_target`,
`a0_commanded_a`, `a0_lock_measured_a` and `a0_lock_frequency_hz` as metadata.
The measured `a` of the recording itself stays in `[optical]` as for every run.

## Choosing `a₀` (still an operator decision)

No numerical `a₀` is frozen in this repository — the plugin default is a
placeholder. Pick it from the low-frequency scout so that

- the low-frequency response gives **several** events, not the one-event floor;
- the event count is still **proportional** to depth and has not saturated;
- the refractory condition `2 f a₀/C ≪ 1/τ_refr` holds at the **highest**
  frequency (checked once by you when picking `a₀`; the plugin does not test it);
- the same measured `a₀` is **reachable at every frequency** in the sweep — the
  lock reports when it is not, before any data is recorded.

Today's low-frequency `a50` result is a sensible starting point; targeting
several plateau events per pixel per half-cycle is a good scout criterion.

## Tests

`cargo test -p augur-plugin-stage-a-a1` covers the lock converging against a
simulated 60 %-gain bench (and leaving the drive at the found depth with a
`safe_off = false` release), the unreachable-depth case railing at the drive
limit without arming a recording, an owner rejection surfacing verbatim, an
event-count point commanding the **locked** depth rather than `a₀`, the
`_ec_f<f>Hz` stem and `[a0_lock]` sidecar section, file-safe frequency tags, and
the one-row-per-frequency lock table round-tripping through `a0_locks.json`.
