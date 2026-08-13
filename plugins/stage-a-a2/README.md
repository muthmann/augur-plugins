# Stage-A A2 latency runner

Runs a fully validated TOML protocol through the existing modulation owner,
photodiode owner and host camera recorder. One row creates one camera RAW, one
photodiode PDQ and one A2 JSON sidecar. The sidecar links the protocol and its
SHA-256, the optical/gate declarations, controller point and finalized receipts;
it does not copy the host-owned camera configuration sidecar.

The plugin does not fit latency. It records both EXT_TRIGGER polarities and the
event-load peak needed by the offline first-event analysis. Dark rows are
explicit fixed-duration acquisitions with the modulation forced safe/off.

The protocol must name a complete host camera profile. It is applied and
confirmed before owner leases and restored on every terminal path. The A2
sidecar links the host camera and sensor-monitoring companions; it does not copy
their camera settings or bias readbacks. The exact protocol text is archived by
content hash in the measurement folder.

The current fluorescence-chain template is
`protocols/a2_fluorescence_chain_followup.toml`. It deliberately contains TBD
bring-up gates and therefore refuses to run until H4, H5, the optical-edge/local-
flux calibrations, comparator threshold per row, lobe endpoints and timing floor
are measured and frozen.

Production firmware mirrors comparator marker frames (`source=2`) onto the
non-blocking photodiode stream, so PDQ contains the independent diagnostic edge
record. Camera EXT_TRIGGER remains the latency clock of record. This does not
replace or weaken the mandatory H4 loopback and H5 polarity/offset gates.
