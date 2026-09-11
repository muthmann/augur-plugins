#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-runtime-plugins.sh [--profile <name>] [--locked] [-- <cargo args...>]

Build every runtime-loaded plugin crate in this repository. Plugins without a
`library = ...` entry in `plugin.toml` are treated as non-runtime plugins and
skipped.

If `AUGUR_RS_PATH` is set, or a sibling `../camerSDK` / `../augur-rs` checkout
exists, the build is patched to use that local AugurRS repo for `augur-core`
and `augur-plugin-api`. This is useful when plugins depend on unreleased host
API changes.

Options:
  --profile <name>  Cargo profile to use (default: release)
  --locked          Pass --locked to cargo build
  --                Forward the remaining arguments to cargo build
  -h, --help        Show this help text
EOF
}

profile="release"
locked=0
cargo_args=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --profile)
            profile="${2:-}"
            shift 2
            ;;
        --locked)
            locked=1
            shift
            ;;
        --)
            shift
            cargo_args=("$@")
            break
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

if [[ -z "${profile}" ]]; then
    echo "--profile requires a value." >&2
    exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"

find_local_augur_rs_repo() {
    local candidate

    if [[ -n "${AUGUR_RS_PATH:-}" ]]; then
        if [[ -d "${AUGUR_RS_PATH}/.git" ]]; then
            printf '%s\n' "${AUGUR_RS_PATH}"
            return 0
        fi
        echo "AUGUR_RS_PATH does not point to a git checkout: ${AUGUR_RS_PATH}" >&2
        exit 1
    fi

    for candidate in "${repo_root}/../camerSDK" "${repo_root}/../augur-rs"; do
        if [[ -d "${candidate}/.git" ]]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
    done

    return 1
}

packages=()
skipped_not_runtime=0

for plugin_dir in "${repo_root}"/plugins/*; do
    [[ -d "${plugin_dir}" ]] || continue

    manifest_path="${plugin_dir}/plugin.toml"
    cargo_toml="${plugin_dir}/Cargo.toml"
    [[ -f "${manifest_path}" && -f "${cargo_toml}" ]] || continue

    library_base="$(sed -n 's/^library = "\(.*\)"$/\1/p' "${manifest_path}" | head -n 1)"
    if [[ -z "${library_base}" ]]; then
        echo "Skipping $(basename "${plugin_dir}"): no runtime library declared in plugin.toml"
        skipped_not_runtime=$((skipped_not_runtime + 1))
        continue
    fi

    package_name="$(sed -n 's/^name = "\(.*\)"$/\1/p' "${cargo_toml}" | head -n 1)"
    if [[ -z "${package_name}" ]]; then
        echo "Could not read package name from ${cargo_toml}" >&2
        exit 1
    fi
    packages+=("${package_name}")
done

if [[ ${#packages[@]} -eq 0 ]]; then
    echo "No runtime plugin packages found." >&2
    exit 1
fi

echo "Building ${#packages[@]} runtime plugin(s) with profile ${profile}:"
printf '  %s\n' "${packages[@]}"
common_args=(--manifest-path "${repo_root}/Cargo.toml")
if [[ "${profile}" == "release" ]]; then
    common_args+=(--release)
else
    common_args+=(--profile "${profile}")
fi
if [[ ${locked} -eq 1 ]]; then
    common_args+=(--locked)
fi

if local_augur_rs_repo="$(find_local_augur_rs_repo)"; then
    local_augur_rs_url="file://${local_augur_rs_repo}"
    common_args+=(
        --config
        "patch.\"https://github.com/muthmann/augur-rs.git\".augur-core.git=\"${local_augur_rs_url}\""
        --config
        "patch.\"https://github.com/muthmann/augur-rs.git\".augur-plugin-api.git=\"${local_augur_rs_url}\""
    )
fi

if [[ -n "${local_augur_rs_repo:-}" ]]; then
    echo "Using local AugurRS checkout: ${local_augur_rs_repo}"
fi
echo
for package in "${packages[@]}"; do
    cmd=(cargo build "${common_args[@]}" -p "${package}")
    # A5 embeds A2 as a library with its default features disabled so that A5
    # exports the one vtable belonging to the A5 runtime.  The standalone A2
    # cdylib must explicitly re-enable its own entrypoint.
    if [[ "${package}" == "augur-plugin-stage-a-a2" ]]; then
        cmd+=(--features plugin-entrypoint)
    fi
    if [[ ${#cargo_args[@]} -gt 0 ]]; then
        cmd+=("${cargo_args[@]}")
    fi
    echo "Building ${package}"
    "${cmd[@]}"
done
echo
echo "Built ${#packages[@]} runtime plugin(s)"
echo "Skipped ${skipped_not_runtime} non-runtime plugin(s)"
