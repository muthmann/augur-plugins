# Installing Runtime Plugins

AugurRS loads runtime plugins from:

```text
~/.augur/plugins/
```

Do not copy source trees into that directory. The host needs a compiled dynamic library plus a `plugin.toml` manifest.

## Installed Layout

```text
~/.augur/plugins/
  localization/
    plugin.toml
    libaugur_plugin_localization.dylib
```

On Linux the library ends in `.so`. On Windows it ends in `.dll`.

Host-owned built-in tools are part of `augur-gui` and are not installed from this repository.

## Install Without A Toolchain (recommended for bench machines)

CI builds every runtime plugin on each push to `main` and publishes them as a
rolling [`plugins-latest`](https://github.com/muthmann/augur-plugins/releases/tag/plugins-latest)
release, already in the layout above. Installing is then a copy:

```bash
curl -LO https://github.com/muthmann/augur-plugins/releases/download/plugins-latest/augur-plugins-macos-arm64.zip
unzip augur-plugins-macos-arm64.zip -d bundle
mkdir -p ~/.augur/plugins
cp -R bundle/*/ ~/.augur/plugins/
```

Archives exist for `macos-arm64`, `macos-x86_64`, `linux-x86_64` and
`windows-x86_64`. The macOS libraries are per-architecture, not universal, so an
Apple Silicon machine needs `macos-arm64` even though `augur-gui` itself ships
universal — the wrong one fails at load time, not at copy time.

Every archive carries a `BUILD-INFO.txt` naming the `augur-rs` revision and the
`rustc` version it was built with. That is the first thing to check against a
[plugin ABI mismatch](#plugin-abi-mismatch). Verify downloads against
`SHA256SUMS.txt` from the same release.

The sections below cover building from source, which contributors still need.
See [CI Prebuilt Plugin Bundles](./features/ci-prebuilt-plugin-bundles.md) for how
the bundles are produced.

## Build One Plugin

```bash
cargo build -p augur-plugin-localization --release
```

## Build A Plugin Chain

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build \
  -p augur-plugin-evesmlm-candidates \
  -p augur-plugin-evesmlm-fitting \
  -p augur-plugin-evesmlm-postproc \
  --release
```

## Install One Plugin

```bash
mkdir -p ~/.augur/plugins/localization
cp plugins/localization/plugin.toml ~/.augur/plugins/localization/
cp target/release/libaugur_plugin_localization.dylib ~/.augur/plugins/localization/
```

Install each plugin into its own directory under `~/.augur/plugins/<name>/`.

On macOS, a plain `cp` keeps Cargo's build-path dylib identity in the copied file. Rewrite the
installed copy so reloads do not keep resolving back to the build tree:

```bash
install_name_tool -id "@loader_path/libaugur_plugin_localization.dylib" \
  ~/.augur/plugins/localization/libaugur_plugin_localization.dylib
```

## Install All Built Plugins

```bash
./scripts/install-built-plugins.sh --profile release
```

This copies every plugin that already has a built runtime library in `target/release/`.
On macOS it also rewrites each installed dylib id to `@loader_path/<basename>` so Plugin Manager
reloads do not stay pinned to Cargo's original build-path identity.

## Load Or Reload In The GUI

1. Launch `augur-gui`
2. Open **Plugins**
3. Click **Scan for New Plugins**
4. Enable the plugin in the Plugin Manager

After rebuilding a plugin during development, use **Reload** instead of restarting the host.

## Troubleshooting

### “missing field `name`”

Your `plugin.toml` still uses an old manifest format such as:

```toml
[plugin]
name = "..."
```

Update it to the current top-level runtime format used by the plugins in this repository.

### “no .dylib/.so/.dll found”

You copied a source directory instead of the built library. Build the plugin and copy the generated runtime artifact into the installed plugin directory.

### “loading symbol augur_plugin_vtable failed”

The library was built against an older plugin interface or does not export the runtime vtable. Port it to `augur-plugin-api::Plugin` and export it with `export_plugin!`.

### “plugin ABI mismatch”

The installed runtime library is stale relative to the host ABI.

If the library came from a release bundle, compare its `BUILD-INFO.txt` against the
running host first — `augur_rs_sha` says which host revision it was built for, and
`rustc` says which compiler produced it. Plugins are loaded into the host process,
so a compiler mismatch is as much a cause as a stale revision;
[`rust-toolchain.toml`](../rust-toolchain.toml) pins the same version `augur-rs`
does, but only a rustup-managed `cargo` honours it. Check with
`cargo --version` — a Homebrew or distro `cargo` earlier on `PATH` ignores the pin.

1. Rebuild the plugin against the current sibling `augur-rs` checkout.
2. Replace the installed runtime library in `~/.augur/plugins/<name>/`.
3. On macOS, prefer `./scripts/install-built-plugins.sh --profile release` or rewrite the copied dylib id with `install_name_tool -id "@loader_path/<basename>" ...`.

If you overwrote a plugin while `augur-gui` was already running, restart the host once after the ABI bump to clear any previously loaded image from the process.

### The plugin loads but host-owned settings are missing

`GlobalSettings` are published through `augur.global_settings` by newer hosts. If a plugin tolerates `None` there, verify that the installed plugin and the `augur-gui` build come from compatible `augur-rs` / `augur-plugins` revisions.
