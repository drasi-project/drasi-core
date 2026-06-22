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
# image. In the musl cross images openssl (built from source under
# /usr/local/musl-ssl) is discoverable, so WITH_SSL flips ON; if curl were also
# found WITH_CURL would too, enabling WITH_OAUTHBEARER_OIDC and pulling curl/ssl
# symbols into the static archive that rdkafka-sys never links -> undefined
# symbols at final plugin link.
#
# We cannot suppress that detection with a plain environment variable, and merely
# pointing `CMAKE_TOOLCHAIN_FILE` at a feature-only fragment makes the `cmake`
# crate skip setting the cross compiler (it assumes the toolchain file defines it),
# which then breaks the build differently. So this toolchain file does BOTH:
#   1. defines the musl cross compiler (so cmake builds for the right target), and
#   2. disables find_package for curl/openssl/zstd so the WITH_*=0 intent is
#      honored, yielding a minimal static librdkafka (zlib + bundled lz4) with
#      WITH_OAUTHBEARER_OIDC=0 and no curl/ssl symbols (no undefined symbols at
#      plugin link time).
#
# IMPORTANT (separate upstream bug -> still need curl HEADERS, not the library):
# librdkafka 2.12.1's packaging/cmake/config.h.in declares
# `#cmakedefine01 WITH_OAUTHBEARER_OIDC`, which ALWAYS emits a `#define`
# (`... 0` or `... 1`). But src/rdkafka_conf.c guards its curl include with
# `#ifdef WITH_OAUTHBEARER_OIDC` (should be `#if`). So `#include <curl/curl.h>`
# is compiled UNCONDITIONALLY in every cmake build, even with WITH_*=0. Since
# WITH_OAUTHBEARER_OIDC=0 means no curl FUNCTIONS are referenced (OIDC sources
# aren't compiled and the curl call sites are `#if`-guarded), we only need
# curl/curl.h to be present on the include path -- NOT libcurl. The musl
# Dockerfiles therefore copy the curl headers into the musl sysroot's include
# dir. This is why the native linux-gnu build "works": its curl headers are
# already present.
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
