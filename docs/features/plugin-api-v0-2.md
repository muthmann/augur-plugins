# Plugin Runtime Migration Notes

## Summary

This brief originally tracked the first cleanup that moved this repository onto the runtime
`augur-plugin-api` surface. It now serves as a historical migration note plus a pointer to the
additional interface changes that matter today.

The live authoring contract is the upstream guide in `augur-rs`.

## Original Runtime Migration

The first runtime pass introduced:

- `Plugin::process_frame(..., event_store: &EventStoreHandle<'_>)`
- declarative settings and status rows instead of plugin-owned `egui`
- generic host views instead of reconstruction-specific hooks
- runtime packaging as `plugin.toml` plus a compiled `cdylib`

## Interface Additions That Matter Now

Authors updating older docs or plugins also need to account for:

- `PluginCapabilities { retained_event_history: true }` as the opt-in for host-retained history
- `GlobalSettings` on `CTX_GLOBAL_SETTINGS` for host-owned pixel scale, geometry, acquisition time, and EventStore budget
- optional `host_view_dataset_generation()` so the host can invalidate cached datasets only when the provider reports a newer generation
- the fact that host-owned built-in tools are no longer described as repository plugins

## Migration Checklist

1. Replace `AnalysisPlugin` with `Plugin`.
2. Replace typed `PluginContext` exchange with string-keyed `HostContext`.
3. Move direct plugin UI code to declarative settings and status entries.
4. Replace special-case host rendering hooks with `host_views()` plus `host_view_dataset()`.
5. Use `capabilities()` when the plugin needs retained event history.
6. Prefer `GlobalSettings` over duplicating host-owned calibration defaults.
7. Rebuild against a matching `augur-rs` checkout before installing into `~/.augur/plugins/`.

## Verification

```bash
rg -n "EventStoreHandle|PluginCapabilities|retained_event_history|GlobalSettings|CTX_GLOBAL_SETTINGS|host_view_dataset_generation" \
  plugins plugin-template docs
```
