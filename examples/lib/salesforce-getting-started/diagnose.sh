#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST_PATH="$SCRIPT_DIR/Cargo.toml"

echo "Rust toolchain:"
rustc --version
cargo --version

if [ "$#" -eq 0 ]; then
    echo "Usage: $0 --org YOUR_SALESFORCE_ALIAS"
    echo "Offline diagnostics: $0 --local"
    exit 2
fi

exec cargo run --manifest-path "$MANIFEST_PATH" -- "$@" --diagnose
