#!/bin/bash
# Run the A2A Hello World DrasiLib example.
# Start samples/python/agents/helloworld on port 9999 first.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if lsof -ti:9000 >/dev/null 2>&1; then
    echo "Port 9000 is already in use. Free it and retry."
    exit 1
fi

cargo run
