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

## Install All Built Plugins

```bash
./scripts/install-built-plugins.sh --profile release
```

This copies every plugin that already has a built runtime library in `target/release/`.

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

### The plugin loads but host-owned settings are missing

`GlobalSettings` are published through `augur.global_settings` by newer hosts. If a plugin tolerates `None` there, verify that the installed plugin and the `augur-gui` build come from compatible `augur-rs` / `augur-plugins` revisions.
