#!/bin/bash
# Setup dependencies for the Cloudflare Radar dashboard example

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

if [[ -z "${CF_RADAR_TOKEN:-}" ]]; then
    echo "CF_RADAR_TOKEN is not set."
    echo "Export your Cloudflare Radar API token before running this script."
    exit 1
fi

if ! command -v cargo > /dev/null 2>&1; then
    echo "cargo is not available in PATH."
    exit 1
fi

echo "Building Cloudflare Radar dashboard example..."
cargo build

echo "Setup complete."
