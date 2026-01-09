# AGENTS.md: `index-rocksdb` Crate

## Architectural Intent
A **high-performance, embedded persistent storage backend** using RocksDB. It enables durable, serverless state management by mapping Drasi's storage interfaces to RocksDB's Key-Value model.

## Architectural Rules
*   **Async/Sync Bridge**: RocksDB API is synchronous. All interaction MUST be wrapped in `tokio::task::spawn_blocking` to avoid blocking the async runtime.
*   **Transactional Integrity**: Multi-key updates (e.g., Element + Indexes) MUST be wrapped in a RocksDB Transaction to ensure atomicity.
*   **Data Partitioning**: Data types MUST be segregated into distinct **Column Families** for performance tuning and isolation.

## Data Mapping Strategy
*   **Serialization**: Uses **Protocol Buffers (`prost`)** for efficient, schema-aware serialization of storage structs (`StoredElement`, `StoredValue`).
*   **Column Families**: Each index type (ElementIndex, FutureQueue, ResultIndex) maintains its own set of Column Families in isolated databases for data partitioning and performance tuning.
    *   `slots`: Stores `BitSet` of slot affinities.
    *   `inbound`/`outbound`: Stores graph edge indexes for traversal.
    *   `futures`: Stores the time-ordered future queue.
*   **Concurrency**: Uses `OptimisticTransactionDB` to manage concurrent updates without heavy locking.