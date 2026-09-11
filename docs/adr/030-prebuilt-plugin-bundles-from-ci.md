# ADR 030 — Prebuilt plugin bundles are produced by CI, not by the bench

**Status:** accepted
**Date:** 2026-08-04
**Feature brief:** [CI Prebuilt Plugin Bundles](../features/ci-prebuilt-plugin-bundles.md)

## Context

A runtime plugin is a `cdylib` plus a `plugin.toml`. Getting one onto a machine
required a Rust toolchain, a sibling `augur-rs` checkout, and `cargo`, because
this workspace depends on the host by path. That made the measurement PC a
development machine by necessity: every plugin fix had to be compiled where it
was used.

Two properties of the plugin model make "just compile it there" worse than it
looks. Plugins are dlopened into the host process, so the compiler that builds a
plugin and the compiler that builds `augur-gui` have to agree — and this
repository pinned no toolchain at all while `augur-rs` pinned `1.95.0`. And the
installed folder is not just a library: A1 ships operator-facing `protocols/`
examples, and macOS copies need their dylib id rewritten or Plugin Manager
reloads resolve back into Cargo's build tree.

## Decision

CI builds the runtime plugins on every pull request and every push to `main`, for
macOS arm64, macOS x86_64, Linux x86_64 and Windows x86_64, and publishes the
result as a folder that is copied verbatim into `~/.augur/plugins/`.

Three things follow from that, and they are the actual decision:

1. **This repository pins the host's toolchain.** `rust-toolchain.toml` carries
   the same `1.95.0` as `augur-rs`, and the workflow reads the channel out of
   that file instead of naming a version in YAML. A bundle built by a different
   compiler than the host is not a bundle, it is a load failure waiting to
   happen, and the pin is the only thing that makes that guarantee checkable.

2. **CI runs the repository's own build and install scripts.** It does not
   reimplement plugin discovery, library naming, the `protocols/` copy or the
   macOS install-name rewrite in YAML. The scripts are the single definition of
   what an installed plugin is; CI is one more caller of them, with
   `--dest dist/<bundle>` instead of `~/.augur/plugins`.

3. **`main` publishes a rolling release, not just artifacts.** Workflow artifacts
   need a GitHub login and expire after 90 days. The bench is the consumer, and
   it should be able to `curl` a URL. The tag `plugins-latest` is deleted and
   recreated on every push to `main`, so its assets can never be a mixture of two
   builds.

Every bundle carries a `BUILD-INFO.txt` recording the `augur-plugins` commit, the
`augur-rs` ref and SHA, and the exact `rustc` version.

## Consequences

- The measurement PC needs no toolchain, no checkout, and no `cargo`.
- An ABI-mismatch report from the bench is now answerable: the provenance file
  says which host revision and compiler the installed library came from.
- Local builds in this repository move from whatever `rustc` is on `PATH` to the
  pinned `1.95.0` — but only for people whose `cargo` is the rustup shim. A
  Homebrew `cargo` earlier on `PATH` ignores `rust-toolchain.toml` entirely and
  will keep producing plugins for a compiler the host does not use.
- The macOS bundles are per-architecture while `augur-gui` ships universal, so
  the download page has one more choice on it than the host's does.
- `main` gains a permanent release tag. The repository had no releases before, so
  `releases/latest` now resolves to `plugins-latest`; a future versioned release
  scheme would have to account for that.

## Alternatives considered

**Publish only workflow artifacts.** Simplest, and rejected: it puts a GitHub
login between the bench and a fix, and the artifact disappears after 90 days.

**Build against the newest `augur-rs` release tag, or against `main`.** Both were
rejected by fact rather than by preference: `augur-rs` `main` does not carry the
`TableSchema`, host-view or dataset-descriptor API these plugins already use, so
either choice is a guaranteed red build. The default host ref is therefore the
open host branch that does carry it, and `BUILD-INFO.txt` records the exact ref
and SHA behind every library so the coupling stays visible. This is temporary by
construction: the default moves to `main` in the same commit that the host API
lands there.

**Reimplement the install layout in the workflow.** Would have avoided calling
shell scripts from YAML, at the cost of a second, silently divergent definition
of what an installed plugin contains. The `protocols/` folder and the macOS
install-name rewrite were both added to the script after the fact; a YAML copy
would have missed both.

**`lipo` the two macOS builds into universal libraries.** Attractive, since the
host is universal, but `install-built-plugins.sh` reads `target/<profile>` only
and a cross-build lands in `target/<triple>/<profile>`. Deferred rather than
special-cased in CI, since it belongs in the script if it is worth doing.
