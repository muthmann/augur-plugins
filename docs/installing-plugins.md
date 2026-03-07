# Installing Runtime Plugins

AugurRS loads plugins at runtime from:

```text
~/.augur/plugins/
```

Do not copy source trees into that directory and expect them to load. The GUI needs a compiled dynamic library plus a `plugin.toml`.

## Installed Layout

```text
~/.augur/plugins/
  hotpixel/
    plugin.toml
    libaugur_plugin_hotpixel.dylib
```

## Build a Plugin

```bash
cargo build -p augur-plugin-hotpixel --release
```

For the eveSMLM chain:

```bash
CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build \
  -p augur-plugin-evesmlm-candidates \
  -p augur-plugin-evesmlm-fitting \
  -p augur-plugin-evesmlm-postproc \
  --release
```

## Install It

```bash
mkdir -p ~/.augur/plugins/hotpixel
cp plugins/hotpixel/plugin.toml ~/.augur/plugins/hotpixel/
cp target/release/libaugur_plugin_hotpixel.dylib ~/.augur/plugins/hotpixel/
```

On Linux, copy the `.so`. On Windows, copy the `.dll`.

Install each plugin into its own directory, for example:

```bash
mkdir -p ~/.augur/plugins/evesmlm-fitting
cp plugins/evesmlm-fitting/plugin.toml ~/.augur/plugins/evesmlm-fitting/
cp target/release/libaugur_plugin_evesmlm_fitting.dylib ~/.augur/plugins/evesmlm-fitting/
```

## Load It in the GUI

1. Launch `augur-gui`
2. Open **Plugins**
3. Click **Scan for New Plugins**
4. Enable the plugin from **Analysis** or in the Plugin Manager

## Reload During Development

After rebuilding a plugin, use the Plugin Manager **Reload** button instead of restarting the host.

## Troubleshooting

### “missing field `name`”

Your `plugin.toml` still uses the old legacy catalog format such as:

```toml
[plugin]
name = "..."
```

Update it to the new runtime format with top-level fields. All maintained runtime plugins in this repository already use that format.

### “no .dylib/.so/.dll found”

You copied a source folder instead of the built library. Build the plugin in `--release` mode and copy the generated dynamic library into the installed plugin directory.

### “loading symbol augur_plugin_vtable failed”

The library still uses the old compile-time plugin API. Port it to `augur-plugin-api::Plugin` and export it with `export_plugin!`.
