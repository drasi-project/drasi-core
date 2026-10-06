#!/usr/bin/env bash
set -euo pipefail

target="${1:?target triple is required}"
toolchain="$(awk -F'"' '/^channel = / { print $2; exit }' rust-toolchain.toml)"
if [[ -z "${toolchain}" ]]; then
  echo "rust-toolchain.toml does not declare a toolchain channel" >&2
  exit 1
fi

restore_workspace_ownership() {
  if [[ -n "${HOST_UID:-}" && -n "${HOST_GID:-}" ]]; then
    chown -R "${HOST_UID}:${HOST_GID}" target
  fi
}
trap restore_workspace_ownership EXIT

dnf install -y \
  autoconf \
  automake \
  clang-devel \
  cmake \
  gcc \
  gcc-c++ \
  jq-devel \
  libtool \
  make \
  perl \
  pkgconf-pkg-config \
  protobuf-compiler \
  protobuf-devel

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --profile minimal --default-toolchain "${toolchain}"
export PATH="${HOME}/.cargo/bin:${PATH}"

rustup target add "${target}"

jq_library="$(rpm -ql jq-devel | awk '/\/libjq\.so$/ { print; exit }')"
if [[ -z "${jq_library}" ]]; then
  echo "jq-devel did not install libjq.so" >&2
  exit 1
fi
export JQ_LIB_DIR
JQ_LIB_DIR="$(dirname "${jq_library}")"

toolchain_env="CMAKE_TOOLCHAIN_FILE_${target//-/_}"
export "${toolchain_env}=${PWD}/ci/cmake/librdkafka-features.cmake"

cargo run -p xtask -- build-plugins --release --target "${target}"
