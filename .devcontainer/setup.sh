#!/usr/bin/env bash
set -euo pipefail

echo "Installing build dependencies..."
sudo apt-get update
sudo apt-get install -y build-essential libjq-dev libonig-dev protobuf-compiler

echo "Development environment setup complete!"
echo "Installed packages:"
echo "  - build-essential (gcc, g++, make)"
echo "  - libjq-dev, libonig-dev"
echo "  - protobuf-compiler (protoc version: $(protoc --version))"
echo ""
echo "JQ_LIB_DIR is configured via devcontainer.json remoteEnv"
