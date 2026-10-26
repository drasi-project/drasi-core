# RocksDB index backend

This backend uses `rocksdb` 0.24 and native RocksDB 10.4.2. The upgrade from
`rocksdb` 0.22 / native RocksDB 8.10.0 includes upstream GCC 15 support, avoiding
missing standard integer declarations when building with modern Linux compilers,
including the Ubuntu 26.04 GitHub Actions image. Builds use the default C++
compiler; no forced include flags or patches to Cargo registry sources are needed.
The Rust bindings require Rust 1.85 or newer.

## Persistent data and rollback

Before upgrading, stop all users of the database and back up each query's entire
database directory. Keep the backup until the upgraded application has been
validated.

The existing index and persistence tests cover Drasi behavior on the current
engine. Cross-version compatibility with databases written by native RocksDB
8.10.0 is not covered by regression tests.

In-place downgrade after opening or writing a database with the new engine is
**not supported**. This is a conservative support policy, not a claim that every
new file is unreadable by the old engine. To roll back, restore the pre-upgrade
backup and the previous application version together. Do not point the old
application at the upgraded database.

No Drasi key/value codec or explicit point-lookup, cache, and memory-budget
settings are intentionally changed by this dependency upgrade. Native engine
defaults can still change.

## Validation

```sh
c++ --version
cargo test -p drasi-index-rocksdb --features plugin-descriptor
cargo clippy -p drasi-index-rocksdb --all-targets --features plugin-descriptor -- -D warnings
```

The focused CI matrix tests Ubuntu 26.04 x86_64, Linux arm64, and macOS using
their default C++ compilers. The downstream server must also update its lockfile
to the newly published `drasi-index-rocksdb` release and rerun its locked
dependency validation and release rehearsal.
