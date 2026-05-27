#!/bin/bash
# Run the DrasiLib Dashboard Example

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "Starting DrasiLib Dashboard Example..."
echo ""

cd "$SCRIPT_DIR"

# Check if ports are in use
for PORT in 3000 9000; do
    if lsof -ti:$PORT > /dev/null 2>&1; then
        echo "Warning: Port $PORT is already in use."
        echo "Kill the existing process? (y/n)"
        read -r response
        if [[ "$response" =~ ^[Yy]$ ]]; then
            lsof -ti:$PORT | xargs kill -9 2>/dev/null || true
            sleep 1
            echo "Port $PORT cleared."
        else
            echo "Exiting. Please free port $PORT and try again."
            exit 1
        fi
    fi
done

# Run the example
cargo run
