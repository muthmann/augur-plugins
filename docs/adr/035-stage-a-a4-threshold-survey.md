# ADR 035: A Threshold Point Is Only Real If The Sensor Confirms It

## Status

Accepted (2026-08-08), implemented in `plugins/stage-a-a4`.

## Context

Stage-A A4 measures the IMX636's contrast threshold: hold the optical condition
still, step `diff_on`/`diff_off` through a list, record a RAW file at each, and
read the event rate against the threshold setting afterwards. It is the one
Stage-A measurement whose independent variable is a **camera bias**.

Three things make that harder than "set a slider and press record".

1. **The requested value is not the measured one.** The settings panel shows an
   *offset* around a per-unit factory trim. The quantity the physics depends on
   is the absolute 8-bit code in the bias register. They differ by a trim that
   varies between sensors, and the offset is clamped into the register on the
   way in.
2. **Nothing else may move.** `fo`, `hpf`, `refr`, the ROI and the pixel mask
   all change the event rate. So do the STC and Trail filters, which discard
   events *before* they are streamed — the quantity being counted.
3. **The bench drifts.** A survey runs for hours. Die temperature and
   illumination move under it, and whether that invalidated a given point is
   not something the runner can decide.

A4 also could not exist at all until the plugin interface could change a bias.
That half was originally augur-rs ADR 036, a verb written for A4 and two fields
wide, so point 2 above was enforced by the wire. augur-rs ADR 037 replaced it
with a generic camera-configuration session, on the grounds that the host must
carry no plugin- or experiment-specific command. The decision below is
unchanged by that; what changed is where point 2 is enforced. A4 now opens a
run by having the host confirm the configuration the bench is on, and builds
every point by cloning that snapshot and setting only `diff_on` and `diff_off`.
A test asserts the equality field by field.

## Decision

**Every point is confirmed against the sensor's own readback before it is
recorded.** A4 sends the two offsets, then checks that the absolute codes the
sensor reports are `factory_default + offset`, and that the reading confirming
them is fresh. A point whose codes disagree, or whose confirming reading is
missing or stale, is **skipped** — it would not be measuring what the protocol
says it measures, and recording it anyway produces a file that is wrong in a
way nobody can detect later. Every sidecar carries the confirmed absolute
codes, the factory trim, and the age of the reading.

Consequently a survey **refuses to start without a bias readback at all**.
Without one the method's central claim is uncheckable, and a run that cannot be
checked should not pretend to have run.

**The freeze on everything else is structural.** A4 uses a host command that
has no field for `fo`, `hpf`, `refr`, the ROI or the mask, so it cannot disturb
them even by mistake. That is stronger than a rule the plugin has to follow.
The filters are a hard refusal, checked both by A4 before the run and by the
host on every command.

**A settle is not over until the sensor has been read again.** Waiting out
`settle_s` proves only that time passed. Requiring a monitoring sample newer
than the settle is what makes the point's recorded start conditions belong to
the point rather than to the state before the bias change.

**Bench-stability limits are flags, not gates.** `max_temperature_drift_c`,
`max_illumination_drift_percent` and `max_event_rate` mark a point and are
carried into its sidecar and the run summary; the recording is kept and the
survey continues. Whether a 2 °C drift invalidated a threshold point is a
judgement to make later with the file in hand, and a runner that discarded the
point would have destroyed the evidence for making it.

A limit whose quantity could **not be measured** is flagged rather than passed.
Otherwise a camera with no temperature readback silently reports every point as
within a drift limit nobody ever checked — the worst of the three outcomes,
because it looks like a verified result.

**File completeness is a gate.** Size, hash, duration and a clean finalize are
all checked. A `RecordingPartial`, an empty file, a missing hash, or a
recording materially shorter than requested is never counted as recorded,
whatever the host called the outcome. The file is kept and the sidecar says
why.

**The bench is put back.** The offsets the survey found are captured before
anything moves and re-applied on completion, on Stop, and on any abort. The run
does not close until that restore is answered, so a survey never disappears
while the sensor is still on its last threshold. They are also remembered after
the run for a manual `Restore biases`, which is the recovery path for a run
that could not restore them itself.

**Failed points get sidecars too.** The record of a failed point is the reason
the survey has a hole in it.

## Consequences

An overnight threshold survey is one button press, and every point on disk can
prove which codes were live on the die while it was written.

The cost is that a bench without a monitoring block cannot run A4 at all —
deliberately, since on such a bench the measurement would be unverifiable. A
survey on a drifting bench still completes, and the drift is visible per point
rather than being resolved by the runner.

## References

- augur-rs ADR 037: host-owned camera profiles and generic plugin configuration
  sessions (supersedes the A4-specific `apply_biases` verb of augur-rs ADR 036)
- ADR 022: Stage-A A1 sensor conditions on every run (absent, never `0`)
- ADR 027: Stage-A A1 declarative protocols (the protocol shape A4 follows)
- ADR 028: the sensor readout travels with the measurement, column-wise
- ADR 031: shared code crosses plugin boundaries through a vtable-free crate
- `docs/features/stage-a-a4.md`
