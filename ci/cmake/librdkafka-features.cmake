# CMake toolchain fragment used only to constrain the bundled `librdkafka`
# build performed by `rdkafka-sys` (via the `rdkafka/cmake-build` feature that
# our Kafka plugins enable for `dynamic-plugin` builds).
#
# Why this is needed:
#   `rdkafka-sys` already passes `-DWITH_CURL=0 -DWITH_SSL=0 -DWITH_ZSTD=0`
#   (and friends), but those are *UNINITIALIZED* cache entries (no type). Because
#   `rdkafka-sys` also passes `-DCMAKE_POLICY_VERSION_MINIMUM=3.5`, policy
#   CMP0077 is OLD. In OLD mode `option(WITH_CURL "..." ${default})` *overwrites*
#   an UNINITIALIZED cache entry with the default it computed from
#   `find_package(CURL/ZSTD/OpenSSL)`. On build hosts that have a host
#   `curl`/`zstd`/`openssl` visible to CMake this force-enables the feature even
#   though `-DWITH_...=0` was passed. That then breaks the build: e.g. enabling
#   both SSL and CURL turns on `WITH_OAUTHBEARER_OIDC`, whose sources do
#   `#include <curl/curl.h>` while the target compiler can't find the host
#   headers (`curl/curl.h: No such file or directory`), and it would also add
#   unwanted runtime dependencies to the distributable plugin.
#
# Fix:
#   Force the `WITH_*` options to the desired values as *typed BOOL* cache
#   entries here, before librdkafka's `project()`/`option()` calls run. A typed
#   (non-UNINITIALIZED) cache entry is left untouched by `option()` in both OLD
#   and NEW CMP0077 modes, so the intended minimal static `librdkafka`
#   (zlib + bundled lz4) is built consistently on every platform.
#
#   The `CMAKE_DISABLE_FIND_PACKAGE_*` settings are kept as defense-in-depth to
#   avoid host detection noise, but the forced `WITH_*` values are what actually
#   guarantee the outcome.
#
# This file is wired in via the `CMAKE_TOOLCHAIN_FILE` / target-specific
# `CMAKE_TOOLCHAIN_FILE_<triple>` environment variables that the `cmake` crate
# reads. It only constrains optional features and deliberately does NOT set
# `CMAKE_SYSTEM_NAME`, so it does not turn an otherwise native build into a cross
# build.

# Compiler wiring for cargo-zigbuild (-gnu glibc-pinned) builds.
#
# `cmake-rs` suppresses its own `-DCMAKE_<LANG>_COMPILER` defines whenever a
# toolchain file is supplied, so without this block CMake falls back to the host
# `cc` (gcc) while `cargo-zigbuild` injects clang/zig-style flags such as
# `--target=<triple>` into `CMAKE_C_FLAGS` — which gcc rejects, breaking the
# compiler check. cargo-zigbuild exports `CC_<triple>` / `CXX_<triple>` (triple
# with `-` replaced by `_`) pointing at its `zig cc` wrapper for the target;
# wire those in here so librdkafka compiles with the same zig toolchain (and
# glibc floor) as the rest of the link. Only the target's variable is defined at
# build time, so at most one branch takes effect; on non-zigbuild builds none do
# and CMake's normal detection is used.
foreach(_zig_triple x86_64_unknown_linux_gnu aarch64_unknown_linux_gnu)
  if(DEFINED ENV{CC_${_zig_triple}})
    set(CMAKE_C_COMPILER "$ENV{CC_${_zig_triple}}")
  endif()
  if(DEFINED ENV{CXX_${_zig_triple}})
    set(CMAKE_CXX_COMPILER "$ENV{CXX_${_zig_triple}}")
  endif()
endforeach()

# Keep zlib (matches rdkafka-sys `-DWITH_ZLIB=1`) and the bundled lz4.
set(WITH_ZLIB ON CACHE BOOL "" FORCE)

# Disable optional features that would otherwise pull in host system libraries.
set(WITH_CURL OFF CACHE BOOL "" FORCE)
set(WITH_SSL OFF CACHE BOOL "" FORCE)
set(WITH_SASL OFF CACHE BOOL "" FORCE)
set(WITH_ZSTD OFF CACHE BOOL "" FORCE)
set(ENABLE_LZ4_EXT OFF CACHE BOOL "" FORCE)

# Defense-in-depth: also stop CMake from auto-detecting these host libraries.
set(CMAKE_DISABLE_FIND_PACKAGE_CURL ON CACHE BOOL "" FORCE)
set(CMAKE_DISABLE_FIND_PACKAGE_ZSTD ON CACHE BOOL "" FORCE)
set(CMAKE_DISABLE_FIND_PACKAGE_OpenSSL ON CACHE BOOL "" FORCE)
