# Shared A2/A5 step acquisition

Non-runtime library for the optical step-latency protocol recorder.
`StageAA2Plugin` is the A2 runtime and the transport layer that A5 embeds.
This crate exports no plugin vtable. The two runtime entry crates
(`plugins/stage-a-a2`, `plugins/stage-a-a5`) each export one vtable, so
neither links another runtime plugin library. See
[ADR 050](../docs/adr/050-shared-a2-a5-step-acquisition.md).
