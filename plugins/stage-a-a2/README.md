# Stage-A A2 latency runner

Runs a point-only TOML protocol through the existing modulation owner,
photodiode owner and host camera recorder. One point creates one camera RAW,
one photodiode PDQ and one A2 JSON sidecar. The operator enters only the points
to record. The runner reads the current hardware state from the owning plugins
and saves it automatically.

The panel asks only for the protocol. The runner creates the measurement ID and
uses the data folder selected in the photodiode plugin.

The plugin does not fit latency. It records both EXT_TRIGGER polarities and the
event-load peak needed by the offline first-event analysis. Dark rows are
explicit fixed-duration acquisitions with the modulation forced safe/off.

The current host camera configuration is applied and confirmed before owner
leases and restored on every terminal path. EXT_TRIGGER and sensor telemetry
must be on. STC, Trail and ERC must be off. The A2 sidecar links the host camera
and sensor-monitoring companions. The exact point protocol is archived by
content hash in the measurement folder.

The applied lobe endpoints and calibration ID come from the modulation owner,
independent of the mode, `mean_u` or `depth_a` currently shown there. A2 sets
each point from the protocol automatically. The operator only applies the
measured transfer curve once. Placement, splitter fraction, load resistance,
reference-set ID, dark reference and stream
state come from the photodiode owner. The step floor is calculated as
`max(5 * pixel_dead_time_us, settling guard)` from sensor telemetry. The runner
also saves its actual comparator settings. An unavailable required value stops
the run before either lease.

`comparator_threshold_dac` defaults to `auto`. `V_50` sits midway between the two
optical plateaus and those move with the operating flux, so the runner measures
it once per distinct `(mean_u, depth_a)` pedestal — holding each plateau as a
constant drive, reading the settled photodiode level, proving via
`end_sample_index` that the window began after the drive was acknowledged, and
refusing the point rather than centring a threshold in noise when the span is too
small.

The longer fluorescence-chain template is
`protocols/a2_fluorescence_chain_followup.toml`.

For the first end-to-end hardware check, use
`protocols/a2_emission_path_technical_smoke.toml`. It records two short dark
brackets around ten slow transitions per polarity. It runs when the live owner
checks pass. It is a commissioning run, not yet a quantitative A2 result. The
sidecar marks that H4/H5 review is still required offline.

When a point has `pause_before = true`, the status text names the required
physical action. Dark points ask the operator to block the optical path. Stepped
points ask the operator to open it and confirm the sample. After **Continue**,
A2 sets modulation, comparator and recording values itself.

Production firmware mirrors comparator marker frames (`source=2`) onto the
non-blocking photodiode stream, so PDQ contains the independent diagnostic edge
record. Camera EXT_TRIGGER remains the latency clock of record. This does not
replace or weaken the mandatory H4 loopback and H5 polarity/offset gates.
