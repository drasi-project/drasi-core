#!/bin/bash
set -e

echo "=== HERE Traffic Source Quickstart ==="
./setup.sh

echo ""
echo "Starting HERE Traffic example..."
echo "Press Ctrl+C to stop."
echo ""

cargo run --release
