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
    chown -R "${HOST_UID}:${HOST_GID}" /workspace || true
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

rustup_version="1.27.1"
case "${target}" in
  x86_64-unknown-linux-gnu)
    rustup_sha256="6aeece6993e902708983b209d04c0d1dbb14ebb405ddb87def578d41f920f56d"
    ;;
  aarch64-unknown-linux-gnu)
    rustup_sha256="1cffbf51e63e634c746f741de50649bbbcbd9dbe1de363c9ecef64e278dba2b2"
    ;;
  *)
    echo "unsupported Rustup target: ${target}" >&2
    exit 1
    ;;
esac
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  "https://static.rust-lang.org/rustup/archive/${rustup_version}/${target}/rustup-init" \
  --output rustup-init
echo "${rustup_sha256}  rustup-init" | sha256sum --check -
chmod +x rustup-init
./rustup-init -y --profile minimal --default-toolchain "${toolchain}"
rm rustup-init
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
