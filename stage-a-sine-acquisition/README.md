# Shared A1/A3 acquisition

Non-runtime library for the sine-protocol coordinator and the existing A1 pure
analysis helpers. `StageAA1Plugin` and `StageAA3Plugin` select the experiment at
compile time. A1 keeps its public re-exports and behavior; A3 selects its own
identity and reduced offline-acquisition UI. This crate exports no plugin vtable.
The two runtime entry crates each export one vtable, so they do not link another
runtime plugin library. See [ADR 047](../docs/adr/047-shared-a1-a3-acquisition.md).
