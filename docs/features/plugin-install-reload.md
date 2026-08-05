# Plugin Install And Reload

## Goal

Keep locally installed runtime plugins reloadable on macOS even when they are built from an in-flight sibling `augur-rs` checkout.

## Problem

Cargo's macOS `cdylib` outputs keep an absolute `LC_ID_DYLIB` that points back into the build tree, for example:

```text
/path/to/augur-plugins/target/release/deps/libaugur_plugin_localization.dylib
```

That identity is harmless when the library stays in `target/`, but it becomes a footgun once the plugin is copied into `~/.augur/plugins/<name>/`. The host scans the installed copy, yet dyld can still treat the plugin as the build-tree image identity during later loads or reloads.

In practice that makes plugin updates look stale: the Plugin Manager can keep reporting an older ABI or older code path even though the copied file in `~/.augur/plugins/` was rebuilt.

## Repo-Level Fix

- `scripts/install-built-plugins.sh` still copies each built runtime plugin into the standard `~/.augur/plugins/<name>/` layout.
- On macOS, the script now rewrites the copied library's `LC_ID_DYLIB` to `@loader_path/<basename>` with `install_name_tool`.
- That keeps the installed artifact self-identified by its installed location instead of Cargo's build-path identity, which makes rescans/reloads behave like the user expects.

## Authoring Guidance

- Prefer `./scripts/install-built-plugins.sh --profile release` over manual `cp` steps when installing local plugins on macOS.
- If you do copy a plugin by hand on macOS, rewrite the installed dylib id after copying:

```bash
install_name_tool -id "@loader_path/libaugur_plugin_my_plugin.dylib" \
  ~/.augur/plugins/my-plugin/libaugur_plugin_my_plugin.dylib
```

- After an ABI bump in `augur-plugin-api`, rebuild the plugin and replace the installed runtime library before using **Scan for New Plugins** or **Reload** in `augur-gui`.

## Verification

- The installed runtime libraries continue to hash-match the built release artifacts apart from the macOS dylib id rewrite.
- `otool -D ~/.augur/plugins/<name>/libaugur_plugin_<name>.dylib` now reports `@loader_path/...` instead of an absolute path into `target/release/deps/`.
