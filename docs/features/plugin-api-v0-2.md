# Plugin API v0.2

## Summary

The plugin workspace now targets the cleaned-up `augur-plugin-api` v0.2 surface from
`augur-rs`. Every runtime plugin receives an `EventStoreHandle` during `process_frame()`, can use
per-frame or persistent JSON context helpers, and publishes host-rendered outputs exclusively
through `host_views()` plus `host_view_dataset()`.

## Author-Facing Changes

- `Plugin::process_frame()` now takes `event_store: &EventStoreHandle<'_>`.
- `HostContext` now exposes `publish_persistent()` and `get_persistent()` for cross-frame state.
- the old reconstruction-specific compatibility hook is gone; reconstruction-style host outputs
  must use host views.
- Plugin manifests and workspace metadata are bumped to `0.2.0`.

## Migration Checklist

1. Add the `EventStoreHandle` argument to `process_frame()`.
2. Ignore it when the plugin only needs the current frame.
3. Use `event_store.all_events()` / `events_in_range()` when a plugin genuinely needs retained
   history.
4. Keep shared payloads on `HostContext`; use the persistent helpers only for state that should
   survive between frames.
5. Rebuild the plugin against the matching `augur-rs` branch or tag before installing it into
   `~/.augur/plugins/`.

## Verification

```bash
cargo fmt --all
cargo build
cargo test
rg "EventStoreHandle|publish_persistent|get_persistent" plugins plugin-template docs
```
