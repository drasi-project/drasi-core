#!/bin/bash
set -e

echo "Installing libjq and dependencies..."
sudo apt-get update
sudo apt-get install -y libjq-dev libonig-dev

echo "Configuring Rust build environment..."
mkdir -p ~/.cargo
cat > ~/.cargo/config.toml << 'EOF'
[build]
JQ_LIB_DIR = "/usr/lib/x86_64-linux-gnu"
EOF

echo "Development environment setup complete!"
