#!/usr/bin/env bash
set -euo pipefail

echo "Installing build dependencies..."
sudo apt-get update
sudo apt-get install -y \
    build-essential \
    protobuf-compiler \
    clang \
    libclang-dev \
    autoconf \
    automake \
    libtool \
    flex \
    bison

echo "Development environment setup complete!"
echo "protoc version: $(protoc --version)"
