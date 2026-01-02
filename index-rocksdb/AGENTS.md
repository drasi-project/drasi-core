# AGENTS.md: `index-rocksdb` Crate

## Architectural Intent

This crate provides a **concrete, high-performance, embedded, persistent storage backend** for the `drasi-core` engine using RocksDB. It translates the abstract storage interfaces (`ElementIndex`, `ResultIndex`, `FutureQueue`) into operations against a local, on-disk key-value store.

The primary design goal is to **leverage RocksDB's specific features, especially Column Families, to efficiently organize state**. This provides a durable, serverless persistence option that does not require a separate database server.

## Architectural Rules

*   **Adherence to Interfaces**: This crate must be a correct and complete implementation of the `core::interface` traits.
*   **Blocking Operations Off Main Thread**: RocksDB's API is synchronous. All calls to the `rocksdb` crate MUST be wrapped in `tokio::task::spawn_blocking` to prevent stalling the async runtime.
*   **Atomicity via Transactions**: Multi-part write operations (e.g., updating an element and its secondary indexes) must be wrapped in a RocksDB transaction.

## Key Design Decisions & Data Mapping Strategy

*   **Serialization with `bincode`**: `bincode` was chosen for its high performance in Rust-to-Rust scenarios. As an embedded store, cross-language compatibility was not a primary concern.
*   **Data Partitioning with Column Families**: This is the most critical design decision. Instead of key prefixes, RocksDB's **Column Families** are used to partition different data types (elements, results, futures, indexes). This is the idiomatic approach in RocksDB, allowing for separate tuning and faster iteration over specific data types.
*   **Key Schema**: Within each Column Family, a specific key schema is used, typically composed of serialized identifiers for consistent ordering and efficient lookups.
*   **Optimistic Transactions**: To ensure atomicity for multi-part writes, RocksDB's optimistic transaction database is used. This is crucial for maintaining a consistent state.
