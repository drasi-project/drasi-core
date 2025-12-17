#!/bin/bash
# Run the DrasiLib Constructor Example

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Starting DrasiLib Constructor Example..."
echo ""

cd "$SCRIPT_DIR"

# Check if port 9000 is in use
if lsof -ti:9000 > /dev/null 2>&1; then
    echo "Warning: Port 9000 is already in use."
    echo "Kill the existing process? (y/n)"
    read -r response
    if [[ "$response" =~ ^[Yy]$ ]]; then
        lsof -ti:9000 | xargs kill -9 2>/dev/null || true
        sleep 1
        echo "Port 9000 cleared."
    else
        echo "Exiting. Please free port 9000 and try again."
        exit 1
    fi
fi

# Run the example
cargo run
