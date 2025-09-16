# AGENTS.md: `index-garnet` Crate

## Architectural Intent

This crate provides a **concrete, persistent storage backend** for the `drasi-core` engine using a Garnet or Redis-compatible server. It translates the abstract storage interfaces (`ElementIndex`, `ResultIndex`, `FutureQueue`) into operations against a remote key-value store.

The primary design goal is to **efficiently map Drasi's conceptual data models onto Redis's highly-optimized data structures**. This enables durable, distributed state for continuous queries.

## Architectural Rules

*   **Adherence to Interfaces**: This crate must be a correct and complete implementation of the `core::interface` traits.
*   **Client Abstraction**: Direct interaction with the `redis-rs` client library must be encapsulated.
*   **Efficient Data Mapping**: The choice of Redis data structure for each piece of state is critical for performance and must be benchmarked.

## Key Design Decisions & Data Mapping Strategy

*   **Serialization with Protocol Buffers (`prost`)**: Protobufs were chosen to serialize Rust structs for storage. This was decided for performance, cross-language compatibility, and robust schema evolution. The `storage_models` module defines these schemas.
*   **Elements as Redis Hashes**: An `Element` is stored in a Redis Hash. This is more memory-efficient than using top-level keys for each property and allows for retrieval of an entire element in a single `HGETALL` command.
*   **Secondary Indexes as Redis Sets**: To enable efficient graph traversal, secondary indexes are implemented using Redis Sets. A key representing a connection point on a node maps to a Set of connected element references.
*   **`FutureQueue` as a Redis Sorted Set**: A Redis Sorted Set is used to implement the time-ordered `FutureQueue`. The timestamp is the score, allowing for efficient retrieval of due items with `ZPOPMIN`.
*   **Command Pipelining**: To reduce network latency, multiple Redis commands are batched into a single pipeline, especially for multi-part write operations.
