/*
 * Intentionally empty stub for <curl/curl.h>.
 *
 * librdkafka's src/rdkafka_conf.c includes <curl/curl.h> under
 *   #ifdef WITH_OAUTHBEARER_OIDC
 * but its generated config.h defines WITH_OAUTHBEARER_OIDC via
 * `#cmakedefine01`, which ALWAYS emits `#define WITH_OAUTHBEARER_OIDC 0/1`.
 * `#ifdef` on an always-defined macro is always true, so the curl header is
 * included unconditionally even when curl/OIDC support is disabled
 * (WITH_CURL=OFF / WITH_OAUTHBEARER_OIDC=0).
 *
 * On the GitHub runners a native gcc build finds the real host <curl/curl.h>
 * (libcurl-dev is preinstalled), but the cargo-zigbuild zig sysroot does not
 * ship curl headers, so the build fails with "curl/curl.h: file not found".
 *
 * Because WITH_OAUTHBEARER_OIDC is 0 in our minimal static librdkafka build,
 * no curl types or functions are actually referenced or linked, so this empty
 * stub is sufficient to satisfy the stray include without pulling libcurl in
 * (which would also break the pinned glibc floor).
 *
 * Wired onto the compiler include path by ci/cmake/librdkafka-features.cmake.
 */
