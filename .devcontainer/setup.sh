#!/usr/bin/env bash
set -euo pipefail

echo "Installing libjq and dependencies..."
sudo apt-get update
sudo apt-get install -y libjq-dev libonig-dev

echo "Configuring Rust build environment..."
mkdir -p ~/.cargo

# Detect the correct library path for the architecture
if [ -x "$(command -v dpkg-architecture)" ]; then
    LIB_PATH="/usr/lib/$(dpkg-architecture -qDEB_HOST_MULTIARCH)"
else
    # Fallback to x86_64 if dpkg-architecture is not available
    LIB_PATH="/usr/lib/x86_64-linux-gnu"
fi

cat > ~/.cargo/config.toml << EOF
[env]
JQ_LIB_DIR = "$LIB_PATH"
EOF

echo "Development environment setup complete!"
echo "JQ_LIB_DIR configured to: $LIB_PATH"
