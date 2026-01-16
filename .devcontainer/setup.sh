#!/usr/bin/env bash
set -euo pipefail

echo "Installing libjq and dependencies..."
sudo apt-get update
sudo apt-get install -y libjq-dev libonig-dev

echo "Configuring Rust build environment..."

# Detect the correct library path for the architecture
if [ -x "$(command -v dpkg-architecture)" ]; then
    LIB_PATH="/usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
else
    # Fallback to x86_64 if dpkg-architecture is not available
    LIB_PATH="/usr/lib/x86_64-linux-gnu"
fi

# Set JQ_LIB_DIR environment variable persistently
echo "export JQ_LIB_DIR=\"$LIB_PATH\"" >> ~/.bashrc
echo "export JQ_LIB_DIR=\"$LIB_PATH\"" >> ~/.zshrc 2>/dev/null || true
echo "export JQ_LIB_DIR=\"$LIB_PATH\"" >> ~/.profile

# Also set it for the current session
export JQ_LIB_DIR="$LIB_PATH"

echo "Development environment setup complete!"
echo "JQ_LIB_DIR configured to: $LIB_PATH"
