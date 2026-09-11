# ADR 031 — Plugins share a types crate, never each other

**Status:** accepted
**Date:** 2026-08-04
**Supersedes:** the "shared types are exported from the producing plugin's crate" convention
**Feature brief:** [eveSMLM Pipeline](../features/evesmlm.md)

## Context

The eveSMLM plugins form a chain: fitting consumes what candidates publishes,
post-processing consumes what fitting publishes. The repository convention was
that shared types are exported from the producing plugin's crate, so
`augur-plugin-evesmlm-fitting` depended on `augur-plugin-evesmlm-candidates`, and
`augur-plugin-evesmlm-postproc` depended on fitting.

Plugin crates are built as `crate-type = ["cdylib", "rlib"]`, and each one
invokes `export_plugin!`, which emits `#[no_mangle] augur_plugin_vtable`. A
plugin that depends on another plugin therefore links that plugin's rlib — and
its vtable symbol — into its own `cdylib`.

The Apple linker tolerates the duplicate. `rust-lld` and MSVC's `link.exe` do
not:

```
rust-lld: error: duplicate symbol: augur_plugin_vtable
LNK2005: augur_plugin_vtable already defined … fatal error LNK1169
```

This was invisible for as long as the only build machine was a Mac. It surfaced
the first time CI built the repository on Linux and Windows (ADR 030): macOS
arm64 produced a complete bundle while both other platforms failed to link.
Every non-macOS user was locked out of the eveSMLM chain, and Stage-A users were
locked out of a Windows bundle entirely, because the build is all-or-nothing.

## Decision

**A plugin crate may not depend on another plugin crate.** Everything that
crosses a plugin boundary lives in a plain library crate that exports no vtable.

For eveSMLM that crate is `evesmlm-types`, holding the wire contract
(`EveEvent`, `EveCluster`, `EveCandidates`, `EveLocalization`,
`EveLocalizationResults`, `FitMethod`, the `CTX_*` channel names) and the
current-localization dataset surface that both fitting and post-processing
publish (`current_localizations_registry_for_results`,
`current_localizations_dataset`, `localization_row_id`,
`to_localization_results`, the `CURRENT_LOCALIZATIONS_*` ids).

Plugin-private types stay in their plugin: the candidate tracker's
`TrackedCluster` moved back out of the shared crate into
`plugins/evesmlm-candidates/src/tracking.rs`. The test is whether another plugin
names the type, not whether it happens to sit next to one that does.

Each plugin keeps re-exporting the shared names it used to own, so downstream
`use augur_plugin_evesmlm_fitting::EveLocalization` keeps compiling.

## Consequences

- The eveSMLM chain links on Linux and Windows, so CI can produce bundles for all
  four platforms rather than macOS only.
- Every plugin `cdylib` exports exactly one `augur_plugin_vtable`, which is what
  the host's loader assumes in the first place.
- `stage-a-plugin-contract` was already built this way for the Stage-A owner
  plugins (ADR 005/006). This generalizes that pattern instead of treating it as
  a Stage-A peculiarity.
- The repository convention in `CLAUDE.md` and `CONTRIBUTING.md` — "shared types
  between plugins should be exported from the producing plugin's crate" — is
  wrong as stated and is replaced by this ADR.
- One more crate per plugin family. That is the cost of the rule, and it is
  smaller than the cost of a platform-specific link failure that only shows up
  on a machine nobody builds on.

## The rule covers dev-dependencies (2026-09-07)

A `[dev-dependencies]` edge onto a plugin crate links that plugin's vtable into
the *test* binary, and it fails exactly the same way. The modulation and
photodiode owners each had one onto the A1 plugin, to validate the shipped A1
protocols against their own limits. Nothing caught it because the workflow only
built `cdylib`s; the first job that compiled a test target on Linux and Windows
failed to link.

The A1 recording protocol parser therefore lives in `stage-a-plugin-contract`
(`protocol`), re-exported by the A1 crate so `crate::protocol::…` and
`augur_plugin_stage_a_a1::protocol::…` both keep working. The build workflow
now runs the Stage-A tests on every platform, so the next such edge fails in CI
rather than on the bench.

## Alternatives considered

**Feature-gate `export_plugin!` and have dependents disable it.** Would keep the
plugin-to-plugin dependency. Rejected: cargo unifies features across a workspace
build, so the `cdylib` target and the same crate consumed as an rlib dependency
resolve to one feature set — the vtable would be on for both, or off for both.

**Duplicate the shared type definitions in each plugin.** No new crate, and no
shared contract either: the two copies would drift, and the published JSON is
exactly what must not drift.

**Build the eveSMLM plugins only on macOS.** Considered because the bench PC that
needed a Windows bundle runs Stage-A, not eveSMLM. Rejected: it encodes a
linker accident as a platform policy, and it leaves the bug in place for the
next plugin family that chains.
