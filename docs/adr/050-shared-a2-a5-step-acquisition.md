# ADR 050: Share A2 and A5 step acquisition

Date: 2026-09-11
Status: Accepted

## Problem

A5 embeds the A2 recorder as its transport layer. It did so by depending on
the A2 *plugin* crate with a `plugin-entrypoint` feature turned off, and the
build script re-enabled that feature for the standalone A2 build.

That does not survive Cargo's per-package builds. `build-runtime-plugins.sh`
builds one package at a time. Building A5 compiles A2 again as a dependency,
now without the entrypoint, and Cargo uplifts that copy of the A2 dynamic
library over the standalone one it built a step earlier. Nothing fails at
build time. On Windows the installed `augur_plugin_stage_a_a2.dll` has no
`augur_plugin_vtable` and the host reports `GetProcAddress failed`; macOS was
only unaffected by the order in which the two builds landed. A `.def` export
list cannot repair this: the copy that A5 produces has no symbol to export.

This is the case ADR 031 forbids — a plugin crate depending on another plugin
crate — hidden behind a feature flag.

## Decision

Move the A2 recorder (`protocol`, `resume`, `runtime`) to the non-runtime
`stage-a-step-acquisition` crate, the same shape ADR 047 gave A1 and A3. It
exports no vtable. `plugins/stage-a-a2` re-exports the crate and exports the
A2 vtable; `plugins/stage-a-a5` wraps `StageAA2Plugin` from the crate and
exports the A5 vtable. Neither plugin crate depends on the other. The feature
flag, the build-script special case and the Windows `.def` workaround are
removed.

The A2 protocol files and manifest stay in `plugins/stage-a-a2/`; the library
includes them by path, as the sine crate does for A1.

## Consequences

Each dynamic library is built once and has exactly one vtable, independent of
build order. The Windows export check in CI stays as the regression guard for
the whole bundle. A2's Rust exports (`stage_a_step_acquisition::*` through
`augur_plugin_stage_a_a2`), routing ID, schema and on-disk artefacts are
unchanged; A5's behaviour is unchanged. A2's tests now run under
`stage-a-step-acquisition` and CI tests that crate alongside the entry point.
