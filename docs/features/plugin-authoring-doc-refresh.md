# Plugin Authoring Docs Refresh

## Summary

This pass updates the repository docs to match the current AugurRS runtime plugin interface documented in `augur-rs`.

The refresh removes stale compile-time registry language, points authors at the upstream runtime-only guide, documents `PluginCapabilities` and host-published `GlobalSettings`, updates scale-sensitive plugin docs to reflect host-driven calibration, and records the move of shared localization payloads into `augur-plugin-types`.

## What Changed

- repo overview docs now treat the upstream authoring guide as the canonical host/runtime contract
- local docs now describe runtime packaging, execution phases, retained event history, host views, and `CTX_GLOBAL_SETTINGS`
- installation examples now use maintained runtime plugins from this repository instead of removed or host-owned tools
- scale-sensitive plugin READMEs now explain that host `GlobalSettings` drive the effective pixel scale, with a hidden fallback kept only for older hosts
- `AGENTS.md` is reduced to an index that points at the canonical references instead of duplicating outdated design prose

## Scope

Updated areas include:

- `README.md`
- `CONTRIBUTING.md`
- `docs/plugin-api.md`
- `docs/installing-plugins.md`
- `docs/architecture.md`
- feature briefs and ADRs that referenced the older interface
- plugin and template READMEs that describe scale-sensitive behavior

## Verification

```bash
rg -n "AnalysisPlugin|PluginContext|create_all_plugins|plugins/hotpixel|roi-grid|flat v0.2|compile-time plugin API" \
  AGENTS.md README.md CONTRIBUTING.md docs plugin-template plugins/*/README.md

rg -n "GlobalSettings|CTX_GLOBAL_SETTINGS|nm_per_pixel|host_view_dataset_generation|retained_event_history" \
  README.md CONTRIBUTING.md docs plugin-template plugins/*/README.md
```
