# CI Prebuilt Plugin Bundles

**Status:** built
**Workflow:** [`.github/workflows/build-plugins.yml`](../../.github/workflows/build-plugins.yml)
**ADR:** [030 — Prebuilt plugin bundles are produced by CI](../adr/030-prebuilt-plugin-bundles-from-ci.md)

## Problem

Installing a plugin used to require a Rust toolchain, a sibling `augur-rs`
checkout, and a working `cargo`. That is a reasonable ask of a contributor and an
unreasonable ask of the bench machine that actually runs the experiment. A
measurement PC should not need a development environment just to pick up a fixed
plugin.

## What it does

Every pull request and every push to `main` builds all runtime plugins on four
platforms and stages them in the exact layout `~/.augur/plugins/` expects:

```text
augur-plugins-macos-arm64/
  BUILD-INFO.txt
  stage-a-a1/
    plugin.toml
    libaugur_plugin_stage_a_a1.dylib
    protocols/
      example.csv
      example.toml
  stage-a-modulation/
  stage-a-photodiode/
  localization/
  …
```

Installing is then a copy — no build step, no toolchain.

| Bundle | Runner | Library |
|---|---|---|
| `macos-arm64` | `macos-latest` | `.dylib` |
| `macos-x86_64` | `macos-13` | `.dylib` |
| `linux-x86_64` | `ubuntu-latest` | `.so` |
| `windows-x86_64` | `windows-latest` | `.dll` |

Pull requests publish the bundles as workflow artifacts. Pushes to `main`
additionally publish a rolling GitHub Release tagged `plugins-latest`, one zip per
platform plus `SHA256SUMS.txt`. The release exists because artifacts require a
GitHub login and expire; a release asset can be fetched from the bench with
`curl` and no account.

`workflow_dispatch` takes an `augur_rs_ref` input for building a bundle against a
different host branch or tag — but note that GitHub only offers `workflow_dispatch`
for workflows present on the default branch, so until this lands on `main` the
`AUGUR_RS_REF` default below is the only way to retarget the host revision.

## Why it is shaped this way

**Two sibling checkouts, not one.** The workspace depends on the host by path
(`augur-core = { path = "../augur-rs/augur-core" }`), so the job checks
`augur-plugins` and `augur-rs` out next to each other under the workspace root
and builds from the former. A single-repo checkout cannot resolve the dependency
at all.

**`augur-rs/.git` is deleted right after checkout.** `build-runtime-plugins.sh`
adds `--config patch."…augur-rs.git"…` flags whenever it finds a sibling
`augur-rs` *git checkout*. With path dependencies that patch matches nothing —
cargo reports `Patch … was not used in the crate graph` and exits 0 — but it
still costs a git fetch of the checkout. Removing `.git` makes the script's
detection fail, and the path dependencies are used directly.

**The toolchain is pinned and read from the file.** Plugins are `cdylib`s the
host `dlopen`s into its own process, so they must be built by the same compiler
as `augur-gui`. [`rust-toolchain.toml`](../../rust-toolchain.toml) pins the same
`1.95.0` as `augur-rs`, and the workflow parses the channel out of that file
rather than repeating the version — CI cannot drift from the pin.

**Linux system dependencies are the plugins' own, not the host's.** The job
installs `pkg-config` and `libudev-dev`, which is what `serialport` (used by
`stage-a-modulation` and `stage-a-photodiode`) needs. Reusing
`augur-rs/.github/scripts/install-linux-deps.sh` was tried first and reverted: it
pulls the whole GUI stack that no plugin links, and it does not exist on every
`augur-rs` revision this job can be pointed at, so the Linux build failed on the
value of `augur_rs_ref` rather than on anything in this repository.

**Warnings are not errors here.** `actions-rust-lang/setup-rust-toolchain`
injects `RUSTFLAGS="-D warnings"` by default. This job ships artifacts, so it
sets `rustflags: ""` — a dead-code warning in one plugin must not deny the bench
a bundle for all of them. Lint gating belongs in its own job.

**The build goes through the repo's own two scripts.** `build-runtime-plugins.sh`
and `install-built-plugins.sh` already know which crates are runtime plugins,
which library name each `plugin.toml` declares, that A1's `protocols/` folder has
to travel with the plugin, and that macOS copies need their dylib id rewritten to
`@loader_path/<basename>`. Re-implementing any of that in YAML would be a second
source of truth. CI runs the same commands a developer runs, only with
`--dest dist/<bundle>`.

On Windows, the staged DLLs are then checked for the required
`augur_plugin_vtable` export before the bundle can be uploaded. A2 is built with
its standalone `plugin-entrypoint` feature explicitly enabled; A5 keeps A2's
entrypoint disabled when it embeds the A2 implementation.

**Archiving happens once, in the release job.** The build matrix uploads raw
folders; the Ubuntu release job zips them. `zip` is not available in the Windows
runner's bash by default, so packaging on each runner would have needed a
per-platform branch for no benefit.

## Provenance

Each bundle carries `BUILD-INFO.txt`:

```text
bundle:        macos-arm64
built_at:      2026-08-04T19:38:11Z
augur_plugins: 30e677c…
augur_rs_ref:  main
augur_rs_sha:  d43652a…
rustc:         rustc 1.95.0 (…)
```

That is what turns an "ABI mismatch" report from the bench into an answerable
question: it records exactly which host revision and which compiler the installed
library was built against.

## Installing a bundle

1. Download the archive for the platform from the
   [`plugins-latest` release](https://github.com/muthmann/augur-plugins/releases/tag/plugins-latest)
2. Unpack it
3. Copy the plugin folders inside into `~/.augur/plugins/`
4. In `augur-gui`: **Plugins** → **Scan for New Plugins** → enable

See [Installing Runtime Plugins](../installing-plugins.md) for the full
installed layout and troubleshooting.

## Which host revision the bundles are built against

`AUGUR_RS_REF` currently defaults to the `augur-rs` branch
`fix/gui-layout-and-alignment` (open host PR #36), **not** to `main`.

That is not a preference, it is the state of the two repositories. These plugins
reference `TableSchema` fields (`layer_id`, `semantic_label`, `provenance`,
`column_display`, `row_id_column`, `time_column`, `coordinate_space_3d`),
`HostViewKind::Scatter3dFromTable`, `HostDatasetDescriptor.relations` /
`.display` and `HostViewRegistry.actions` — none of which exist on `augur-rs`
`main`, which still has a two-field `TableSchema`. Defaulting to `main` would be
a guaranteed red build that never hands the bench a bundle.

The first CI run is what surfaced this: the repository as checked in cannot be
built by anyone who does not already have an unmerged host branch on disk.

**Move the default back to `main` in the same commit that the host API lands
there.** Until then, `BUILD-INFO.txt` is the thing that keeps this honest — it
records the exact host ref and SHA behind every library, so an installed plugin
can always be traced to the host revision it matches.

## Limitations

- The macOS bundles are single-architecture, not universal. `augur-gui` ships as
  a universal binary, so an Intel Mac needs `macos-x86_64` and an Apple Silicon
  Mac needs `macos-arm64`; picking the wrong one fails at load, not at copy.
- `macos-13` is GitHub's last x86_64 macOS runner image. When it is retired, the
  Intel bundle needs a cross-build (`--target x86_64-apple-darwin`), which the
  install script does not currently look for — it only reads `target/<profile>`.
- The bundles are unsigned. macOS Gatekeeper does not quarantine libraries loaded
  by `dlopen` from a user directory, so this has not needed handling, but a
  downloaded archive may still need `xattr -d com.apple.quarantine` if Safari
  attached the flag.
- `Cargo.lock` is gitignored, so builds are not `--locked`. A dependency
  publishing a broken semver-compatible release can turn CI red without a commit
  in either repository.
