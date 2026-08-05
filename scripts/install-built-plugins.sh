#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/install-built-plugins.sh [--profile <name>] [--dest <dir>]

Copy every runtime plugin that has already been built into the AugurRS plugin
directory layout. Plugins without a built dynamic library are skipped. Plugins
without a `library = ...` entry in `plugin.toml` are treated as non-runtime
plugins and skipped as well.

Options:
  --profile <name>  Cargo profile to copy from (default: release)
  --dest <dir>      Destination plugin directory (default: ~/.augur/plugins)
  -h, --help        Show this help text
EOF
}

profile="release"
dest_dir="${HOME}/.augur/plugins"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)
            profile="${2:-}"
            shift 2
            ;;
        --dest)
            dest_dir="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

if [[ -z "$profile" || -z "$dest_dir" ]]; then
    echo "Both --profile and --dest require values." >&2
    exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
target_dir="${repo_root}/target/${profile}"

if [[ ! -d "${target_dir}" ]]; then
    echo "Build directory not found: ${target_dir}" >&2
    echo "Run cargo build first, for example: cargo build --release" >&2
    exit 1
fi

library_extension() {
    case "$(uname -s)" in
        Darwin) echo "dylib" ;;
        Linux) echo "so" ;;
        MINGW*|MSYS*|CYGWIN*) echo "dll" ;;
        *)
            echo "Unsupported OS: $(uname -s)" >&2
            exit 1
            ;;
    esac
}

find_library_path() {
    local library_base="$1"
    local extension="$2"
    local candidates=()

    if [[ "${library_base}" == *".${extension}" ]]; then
        candidates+=("${target_dir}/${library_base}")
    else
        candidates+=("${target_dir}/${library_base}.${extension}")
    fi

    if [[ "${library_base}" != lib* ]]; then
        candidates+=("${target_dir}/lib${library_base}.${extension}")
    fi

    local candidate
    for candidate in "${candidates[@]}"; do
        if [[ -f "${candidate}" ]]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
    done

    return 1
}

rewrite_macos_install_name() {
    local installed_library_path="$1"

    if [[ "$(uname -s)" != "Darwin" ]]; then
        return 0
    fi

    if ! command -v install_name_tool >/dev/null 2>&1; then
        echo "warning: install_name_tool not found; leaving ${installed_library_path} with Cargo's build-path dylib id" >&2
        return 0
    fi

    install_name_tool -id "@loader_path/$(basename "${installed_library_path}")" "${installed_library_path}"
}

library_extension="$(library_extension)"
mkdir -p "${dest_dir}"

installed=0
skipped_not_runtime=0
skipped_not_built=0

for plugin_dir in "${repo_root}"/plugins/*; do
    [[ -d "${plugin_dir}" ]] || continue

    plugin_id="$(basename "${plugin_dir}")"
    manifest_path="${plugin_dir}/plugin.toml"
    if [[ ! -f "${manifest_path}" ]]; then
        continue
    fi

    library_base="$(sed -n 's/^library = "\(.*\)"$/\1/p' "${manifest_path}" | head -n 1)"
    if [[ -z "${library_base}" ]]; then
        echo "Skipping ${plugin_id}: no runtime library declared in plugin.toml"
        skipped_not_runtime=$((skipped_not_runtime + 1))
        continue
    fi

    if ! library_path="$(find_library_path "${library_base}" "${library_extension}")"; then
        echo "Skipping ${plugin_id}: no built .${library_extension} found in ${target_dir}"
        skipped_not_built=$((skipped_not_built + 1))
        continue
    fi

    install_dir="${dest_dir}/${plugin_id}"
    mkdir -p "${install_dir}"
    cp "${manifest_path}" "${install_dir}/plugin.toml"
    installed_library_path="${install_dir}/$(basename "${library_path}")"
    cp "${library_path}" "${installed_library_path}"
    rewrite_macos_install_name "${installed_library_path}"
    # Operator-facing example files a plugin ships alongside its library (A1's
    # recording protocols). The bench has the installed folder, not the repo,
    # so an example the settings panel points at has to travel with the plugin.
    if [[ -d "${plugin_dir}/protocols" ]]; then
        rm -rf "${install_dir}/protocols"
        cp -R "${plugin_dir}/protocols" "${install_dir}/protocols"
    fi
    echo "Installed ${plugin_id} -> ${install_dir}"
    installed=$((installed + 1))
done

echo
echo "Installed ${installed} runtime plugin(s) into ${dest_dir}"
echo "Skipped ${skipped_not_runtime} non-runtime plugin(s)"
echo "Skipped ${skipped_not_built} plugin(s) without a built library in ${target_dir}"
if [[ ${skipped_not_built} -gt 0 ]]; then
    echo "Hint: run ./scripts/build-runtime-plugins.sh --profile ${profile} before installing."
fi
