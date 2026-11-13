#!/usr/bin/env bash

set -euo pipefail

if [[ $# -eq 0 ]]; then
    case "$(uname -s)" in
        Linux*) prefix="lib"; ext="so" ;;
        Darwin*) prefix="lib"; ext="dylib" ;;
        MINGW* | MSYS* | CYGWIN*) prefix=""; ext="dll" ;;
        *) echo "Unsupported platform: $(uname -s)" >&2; exit 1 ;;
    esac
elif [[ $# -eq 2 ]]; then
    prefix="$1"
    ext="$2"
else
    echo "Usage: $0 [library-prefix library-extension]" >&2
    exit 2
fi

metadata="$(cargo metadata --locked --no-deps --format-version 1)"
target_dir="$(printf '%s' "$metadata" | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"].replace("\\", "/"))')"
native_plugins="$(printf '%s' "$metadata" | python3 -c '
import json,sys
packages = json.load(sys.stdin)["packages"]
names = sorted(p["name"] for p in packages
               if (p.get("metadata") or {}).get("drasi-plugin", {}).get("abi-family") == "computation")
if not names:
    sys.exit("No native plugins found in workspace metadata")
print("\n".join(names))
')"
build_dir="$target_dir/debug"
plugin_dir="$build_dir/plugins"
mkdir -p "$plugin_dir/examples"

plugins=(
    drasi-source-mock
    drasi-source-http
    drasi-reaction-http
    drasi-reaction-log
    drasi-reaction-sse
    drasi-reaction-snapshot-test
    drasi-identity-test
    drasi-bootstrap-scriptfile
    drasi-secret-store-file
)
while IFS= read -r plugin; do
    plugins+=("$plugin")
done <<< "$native_plugins"

stage() {
    local source="$1" destination="$2"
    if [[ ! -s "$source" ]]; then
        echo "Expected plugin not found or empty: $source" >&2
        exit 1
    fi
    cp "$source" "$destination"
    echo "Verified $destination"
}

# Stage each library immediately: later Cargo invocations can rebuild the same
# output filename without its dynamic-plugin feature.
for plugin in "${plugins[@]}"; do
    if [[ "$plugin" == "drasi-reaction-snapshot-test" ]]; then
        cargo build --locked --lib -p "$plugin"
    else
        cargo build --locked --lib -p "$plugin" --features "$plugin/dynamic-plugin"
    fi
    artifact="${prefix}${plugin//-/_}.${ext}"
    stage "$build_dir/$artifact" "$plugin_dir/$artifact"
done

cargo build --locked -p drasi-host-sdk --examples
for fixture in native_recovery native_consumer_progress_metadata \
    native_bootstrap_coordination_metadata native_bootstrap_coordination_partial \
    native_activation_credentials native_custom_query native_scheduling; do
    artifact="${prefix}${fixture}.${ext}"
    stage "$build_dir/examples/$artifact" "$plugin_dir/examples/$artifact"
done
cargo build --locked -p drasi-instance-host --example instance_host_fixture \
    --features drasi-instance-host/dynamic-plugin
artifact="${prefix}instance_host_fixture.${ext}"
stage "$build_dir/examples/$artifact" "$plugin_dir/examples/$artifact"

# Retired aggregate plugins must not be discovered alongside their replacements.
rm -f "$plugin_dir/${prefix}drasi_computation_standard.${ext}" \
    "$plugin_dir/${prefix}drasi_computation_network.${ext}"
