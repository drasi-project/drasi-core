#!/usr/bin/env bash
set -euo pipefail

version="29.3"
prefix="${1:-/opt/protoc}"

# protoc runs on the build host, not the Rust compilation target.
case "$(uname -m)" in
  x86_64)
    arch="x86_64"
    sha256="3e866620c5be27664f3d2fa2d656b5f3e09b5152b42f1bedbf427b333e90021a"
    ;;
  aarch64 | arm64)
    arch="aarch_64"
    sha256="6427349140e01f06e049e707a58709a4f221ae73ab9a0425bc4a00c8d0e1ab32"
    ;;
  *)
    echo "unsupported protoc host architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

workdir="$(mktemp -d)"
trap 'rm -rf "${workdir}"' EXIT

curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  "https://github.com/protocolbuffers/protobuf/releases/download/v${version}/protoc-${version}-linux-${arch}.zip" \
  --output "${workdir}/protoc.zip"
echo "${sha256}  ${workdir}/protoc.zip" | sha256sum --check -
unzip -q -o "${workdir}/protoc.zip" -d "${prefix}"
"${prefix}/bin/protoc" --version
