#!/bin/bash
set -e

echo "=== Testing Change Detection ==="
echo "This will run the example and print live updates."
echo "Expect TrafficSegment and TrafficIncident events over a few minutes."
echo ""

cargo run --release
