# Stage-A A2 latency automation

A2 applies repeated calibrated optical log-square steps at several fluorescence
pedestals and records the first camera event after each measured optical edge.
The workflow plugin owns no serial port. It leases and orchestrates the permanent
modulation and photodiode owners and the host camera recorder.

The A2 panel asks only for the point protocol. It creates the measurement ID
automatically and uses the data folder selected in the photodiode plugin.

## Acquisition contract

- The TOML contains only the measurement points. Live hardware state is checked
  before a lease is acquired.
- Values an owner already publishes are resolved from the owner, not retyped
  (ADR 040). The applied lobe endpoints and calibration ID come from
  `ModulationStateV1::optical_lobe`, independent of the currently armed mode or
  point. A2 commands every `mean_u` and `depth_a` from the protocol itself. A
  missing applied calibration refuses with an instruction to use **Apply to
  V_null / V_peak** in the modulation plugin. `min_half_us` may be omitted, in
  which case the runner resolves
  `max(5 * pixel_dead_time_us, settling guard)` from sensor telemetry and every
  stepped half period is checked against the resolved floor. All of this happens
  before the camera apply and before either lease.
- `comparator_threshold_dac` defaults to `auto`. `V_50` moves with the operating
  flux, so per distinct `(mean_u, depth_a)` pedestal the runner holds both
  plateaus as constant drives, reads the settled `PhotodiodeLevelV1` at each,
  proves through `end_sample_index` that the averaging window began after the
  drive was acknowledged, and places the threshold at their midpoint via the
  existing `CMP thr=` path. A clipped window, a stream restart, a plateau span
  under 20 mV, an unsettled window or a midpoint outside the threshold DAC's
  0–2500 mV range refuses the point. The ADC (0–3300 mV) and the threshold DAC
  (0–2500 mV) do not share a reference, so the conversion always goes through
  millivolts — an ADC code is never copied across as a threshold code.
- The current complete camera configuration is applied and confirmed before
  either hardware lease. EXT_TRIGGER and sensor telemetry must be on; STC,
  Trail and ERC must be explicitly off. The host restores the pre-run state on
  success, operator stop and failure.
- `PrepareA2` is accepted only when firmware confirms comparator trigger source,
  an armed comparator and `LOG_SQUARE`.
- Dark points record camera RAW and photodiode PDQ for their declared duration
  with modulation forced safe/off. Stepped points additionally require the
  commanded number of both EXT_TRIGGER polarities (tolerance: one edge).
- A pause is only an operator checkpoint for a physical action. Its message
  tells the operator to block or open the optical path, explains what A2 will
  set automatically, and says when to press **Continue**.
- A sidecar records protocol identity/SHA-256, point, commanded pedestal/depth,
  the actual comparator configuration, modulation calibration, photodiode
  placement, splitter fraction, load resistance, reference-set ID, dark
  reference, final file receipts, dynamic sensor values and trigger/load
  evidence. It links the host-owned camera and sensor-monitoring companions
  instead of duplicating their bias/configuration data. The exact protocol
  source is archived once by SHA-256.
- Implausible stepped trigger counts, a partial RAW/PDQ, an exceeded
  pre-qualified recorder safety limit,
  missing sensor dead-time, stale owner reply or expired lease fails closed.
- The plugin contains no scientific fit. Censoring-aware first-event latency and
  jitter are computed offline, ON and OFF separately.

## Current bench topology

The immediate protocol is scoped to `fluorescence_chain`: ATTO647 sample,
fluorescence filter, 50:50 splitter, camera and the sole photodiode in the
emission path. It does not use rejected-port complement geometry or `I_tot`.

## Hardware status

Firmware mode A2, `CMP`, `LOG_SQUARE`, `min_half_us`, comparator source ID 2 and
trigger-source status exist in `stage-a-controller`. The sources build, but the
comparator has not been bench-qualified. H4 loopback, H5 polarity/offset and the
emission optical-edge qualification remain required before a quantitative A2
result is accepted. They are review qualifications, not fields copied into each
point protocol.

The optional PDQ cross-check is not the A2 time base. Production firmware now
mirrors comparator marker frames (`source=2`) through the non-blocking
photodiode stream path, so a PDQ can carry the independent comparator-edge
record. Camera-clock EXT_TRIGGER remains the latency clock of record. Marker
drops are explicit firmware integrity evidence. The A2 sidecar marks that H4/H5
review is required.

Before the first A2 run, record the generic electronics-dark,
blocked-drive-crosstalk and static-light files in the PD plugin. The PD owner
publishes the selected reference-set ID, and A2 stores it automatically. These
files support later noise and timing analysis. They do not replace H4 or H5.
