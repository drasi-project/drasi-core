# CMake toolchain fragment used only to constrain the bundled `librdkafka`
# build performed by `rdkafka-sys` (via the `rdkafka/cmake-build` feature that
# our Kafka plugins enable for `dynamic-plugin` builds).
#
# Why this is needed:
#   `rdkafka-sys` already passes `-DWITH_CURL=0 -DWITH_SSL=0 -DWITH_ZSTD=0`,
#   but `librdkafka`'s CMakeLists.txt declares those as `option()` *after*
#   running `find_package(CURL/ZSTD/OpenSSL)` to compute their defaults. Because
#   `rdkafka-sys` also passes `-DCMAKE_POLICY_VERSION_MINIMUM=3.5`, policy
#   CMP0077 is OLD, so `option()` ignores the `-D...=0` cache values and instead
#   honors whatever `find_package` auto-detected. On build hosts that happen to
#   have a host `curl`/`zstd`/`openssl` visible to CMake this force-enables the
#   feature, which then breaks cross/arch-targeted builds (the host headers are
#   found but the target compiler can't use them — e.g. `curl/curl.h: No such
#   file or directory`) and would otherwise add unwanted runtime dependencies to
#   the distributable plugin.
#
# Forcing `find_package` for these optional libraries to fail makes the computed
# defaults OFF, so the intended minimal static `librdkafka` (zlib + bundled lz4)
# is built consistently on every platform.
#
# This file is wired in via the `CMAKE_TOOLCHAIN_FILE` / target-specific
# `CMAKE_TOOLCHAIN_FILE_<triple>` environment variables that the `cmake` crate
# reads. It only disables optional `find_package` lookups and deliberately does
# NOT set the compiler or `CMAKE_SYSTEM_NAME`, so it does not turn an otherwise
# native build into a cross build.
set(CMAKE_DISABLE_FIND_PACKAGE_CURL ON CACHE BOOL "" FORCE)
set(CMAKE_DISABLE_FIND_PACKAGE_ZSTD ON CACHE BOOL "" FORCE)
set(CMAKE_DISABLE_FIND_PACKAGE_OpenSSL ON CACHE BOOL "" FORCE)
