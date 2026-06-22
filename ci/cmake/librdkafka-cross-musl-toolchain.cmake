# Cross-compilation toolchain fragment for building the bundled `librdkafka`
# (via rdkafka-sys's `cmake-build`) for the musl targets under `cross`.
#
# Why this exists
# ----------------
# rdkafka-sys passes `-DWITH_CURL=0 -DWITH_SSL=0 -DWITH_ZSTD=0` because the
# corresponding rdkafka Cargo features are off, and it therefore does NOT emit
# link flags for libcurl/openssl/zstd. But librdkafka's CMakeLists.txt computes
# those `WITH_*` defaults from `find_package(...)`, and under CMP0077 OLD
# (rdkafka-sys also passes `-DCMAKE_POLICY_VERSION_MINIMUM=3.5`) `option()`
# OVERRIDES the uninitialized `-DWITH_*=0` with whatever it detected in the build
# image. In the musl cross images curl/openssl are discoverable, so WITH_CURL and
# WITH_SSL flip ON, enabling WITH_OAUTHBEARER_OIDC whose sources do
# `#include <curl/curl.h>`. That fails the build (there is no musl curl header),
# and even if it compiled, rdkafka-sys would not link libcurl/openssl.
#
# We cannot suppress that detection with a plain environment variable, and merely
# pointing `CMAKE_TOOLCHAIN_FILE` at a feature-only fragment makes the `cmake`
# crate skip setting the cross compiler (it assumes the toolchain file defines it),
# which then breaks the build differently. So this toolchain file does BOTH:
#   1. defines the musl cross compiler (so cmake builds for the right target), and
#   2. disables find_package for curl/openssl/zstd so the WITH_*=0 intent is
#      honored, yielding a minimal static librdkafka (zlib + bundled lz4) with no
#      curl/ssl symbols and therefore no undefined symbols at plugin link time.
#
# The musl triple is provided via the LIBRDKAFKA_MUSL_TRIPLE environment variable
# set in the corresponding cross Dockerfile (e.g. `x86_64-linux-musl`).

set(_musl_triple "$ENV{LIBRDKAFKA_MUSL_TRIPLE}")
if(NOT _musl_triple)
  message(FATAL_ERROR "LIBRDKAFKA_MUSL_TRIPLE environment variable must be set")
endif()

set(CMAKE_SYSTEM_NAME Linux)
if(_musl_triple MATCHES "^aarch64")
  set(CMAKE_SYSTEM_PROCESSOR aarch64)
else()
  set(CMAKE_SYSTEM_PROCESSOR x86_64)
endif()

# These resolve via PATH (the musl toolchain is installed under /usr/local/bin).
set(CMAKE_C_COMPILER "${_musl_triple}-gcc")
set(CMAKE_CXX_COMPILER "${_musl_triple}-g++")
set(CMAKE_ASM_COMPILER "${_musl_triple}-gcc")

# Honor rdkafka-sys's WITH_CURL=0 / WITH_SSL=0 / WITH_ZSTD=0 by preventing
# librdkafka from auto-detecting (and thus force-enabling) these optional
# features. ZLIB is intentionally left enabled (rdkafka-sys passes WITH_ZLIB=1
# and provides libz via CMAKE_PREFIX_PATH).
set(CMAKE_DISABLE_FIND_PACKAGE_CURL ON)
set(CMAKE_DISABLE_FIND_PACKAGE_OpenSSL ON)
set(CMAKE_DISABLE_FIND_PACKAGE_ZSTD ON)
