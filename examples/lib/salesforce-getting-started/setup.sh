#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST_PATH="$SCRIPT_DIR/Cargo.toml"

echo "Building Salesforce getting-started example..."
echo "The first build includes RocksDB and can take several minutes."
exec cargo build --manifest-path "$MANIFEST_PATH"
