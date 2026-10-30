#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MANIFEST_PATH="$SCRIPT_DIR/Cargo.toml"

echo "Running offline Salesforce protocol verification (no cloud records changed)..."
if ! OUTPUT="$(cargo run --manifest-path "$MANIFEST_PATH" -- --local 2>&1)"; then
    printf '%s\n' "$OUTPUT" >&2
    exit 1
fi
echo "$OUTPUT"

for marker in \
    "CREATE_DETECTED ✓" \
    "UPDATE_DETECTED ✓" \
    "DELETE_DETECTED ✓" \
    "FULL_SCAN_REPAIR_DETECTED ✓" \
    "EXAMPLE_COMPLETE ✓"
do
    if ! grep -Fq "$marker" <<<"$OUTPUT"; then
        echo "✗ Missing expected marker: $marker"
        exit 1
    fi
done

echo "✓ Change detection verification passed"
