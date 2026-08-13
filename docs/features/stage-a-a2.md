# Stage-A A2 latency automation

A2 applies repeated calibrated optical log-square steps at several fluorescence
pedestals and records the first camera event after each measured optical edge.
The workflow plugin owns no serial port. It leases and orchestrates the permanent
modulation and photodiode owners and the host camera recorder.

## Acquisition contract

- The full TOML is parsed and all hardware/optical gates are checked before a
  lease is acquired.
- A named, complete camera profile is applied and confirmed before either
  hardware lease. EXT_TRIGGER and sensor telemetry must be on; STC, Trail and
  ERC must be explicitly off. The host restores the pre-run state on success,
  operator stop and failure.
- `PrepareA2` is accepted only when firmware confirms comparator trigger source,
  an armed comparator and `LOG_SQUARE`.
- Dark points record camera RAW and photodiode PDQ for their declared duration
  with modulation forced safe/off. Stepped points additionally require the
  commanded number of both EXT_TRIGGER polarities (tolerance: one edge).
- A sidecar records protocol identity/SHA-256, row, commanded pedestal/depth,
  comparator configuration, optical placement, final file receipts, dynamic
  sensor values and trigger/load evidence. It links the host-owned camera and
  sensor-monitoring companions instead of duplicating their bias/configuration
  data. The exact protocol source is archived once by SHA-256.
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
emission optical-edge qualification remain mandatory protocol gates.

The optional PDQ cross-check is not the A2 time base. Production firmware now
mirrors comparator marker frames (`source=2`) through the non-blocking
photodiode stream path, so a PDQ can carry the independent comparator-edge
record. Camera-clock EXT_TRIGGER remains the latency clock of record. Marker
drops are explicit firmware integrity evidence; H4 loopback and H5
polarity/offset calibration remain mandatory and are cited in the protocol.
