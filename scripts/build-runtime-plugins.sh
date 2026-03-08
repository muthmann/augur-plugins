#!/usr/bin/env bash

set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/build-runtime-plugins.sh [--profile <name>] [--locked] [-- <cargo args...>]

Build every runtime-loaded plugin crate in this repository. Plugins without a
`library = ...` entry in `plugin.toml` are treated as non-runtime plugins and
skipped.

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

cmd=(cargo build --manifest-path "${repo_root}/Cargo.toml")
if [[ "${profile}" == "release" ]]; then
    cmd+=(--release)
else
    cmd+=(--profile "${profile}")
fi
if [[ ${locked} -eq 1 ]]; then
    cmd+=(--locked)
fi
for package in "${packages[@]}"; do
    cmd+=(-p "${package}")
done
if [[ ${#cargo_args[@]} -gt 0 ]]; then
    cmd+=("${cargo_args[@]}")
fi

echo "Building ${#packages[@]} runtime plugin(s) with profile ${profile}:"
printf '  %s\n' "${packages[@]}"
echo
"${cmd[@]}"
echo
echo "Built ${#packages[@]} runtime plugin(s)"
echo "Skipped ${skipped_not_runtime} non-runtime plugin(s)"
