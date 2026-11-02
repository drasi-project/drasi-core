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
    exit 1
fi

plugins=(
    drasi-source-mock
    drasi-source-http
    drasi-reaction-log
    drasi-reaction-sse
    drasi-reaction-snapshot-test
    drasi-identity-test
    drasi-bootstrap-scriptfile
)

build_dir="${CARGO_TARGET_DIR:-target}/debug"
plugin_dir="target/debug/plugins"
mkdir -p "$plugin_dir"

for plugin in "${plugins[@]}"; do
    if [[ "$plugin" == "drasi-reaction-snapshot-test" ]]; then
        cargo build --lib -p "$plugin"
    else
        cargo build --lib -p "$plugin" --features "$plugin/dynamic-plugin"
    fi

    name="${plugin//-/_}"
    artifact="${prefix}${name}.${ext}"
    if [[ ! -f "$build_dir/$artifact" ]]; then
        echo "Expected plugin not found: $build_dir/$artifact" >&2
        exit 1
    fi
    cp "$build_dir/$artifact" "$plugin_dir/$artifact"
    echo "Verified $plugin_dir/$artifact"
done
