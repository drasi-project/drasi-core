#!/bin/bash
# Run the Cloudflare Radar dashboard example

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ -z "${CF_RADAR_TOKEN:-}" ]]; then
    echo "CF_RADAR_TOKEN is not set."
    echo "Export your Cloudflare Radar API token before running this script."
    exit 1
fi

if lsof -ti:8080 > /dev/null 2>&1; then
    echo "Warning: Port 8080 is already in use."
    echo "Kill the existing process? (y/n)"
    read -r response
    if [[ "$response" =~ ^[Yy]$ ]]; then
        lsof -ti:8080 | xargs kill -9 2>/dev/null || true
        sleep 1
        echo "Port 8080 cleared."
    else
        echo "Exiting. Please free port 8080 and try again."
        exit 1
    fi
fi

echo "Starting Cloudflare Radar dashboard example..."
cargo run
