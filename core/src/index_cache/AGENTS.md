# AGENTS.md: `core/src/index_cache` Module

## Architectural Intent

This module provides a **transparent, in-memory caching layer** for any storage backend, implementing the **Decorator pattern**.

Its primary design goal is **performance**. It reduces read latency for slower persistent backends (like `index-rocksdb`) by serving frequent requests from an in-memory LRU cache. It is "transparent" as the query engine is unaware of its existence, interacting with the decorator as it would with the real index.

## Architectural Rules

*   **Transparency**: Caching decorators MUST perfectly implement the `interface` traits. Their behavior must be indistinguishable from a non-cached index.
*   **Consistency**: The cache MUST NOT serve stale data. This is enforced with a **write-through** strategy: all writes are passed to the underlying store, and the in-memory cache is updated or invalidated only after the persistent write succeeds.
*   **Composability**: Decorators are designed to be composed (e.g., a `RocksDBElementIndex` can be wrapped with a `CachedElementIndex`).

## Key Design Decisions

*   **Decorator Pattern**: Each caching struct holds an `Arc<dyn ...>` to the "inner" index it decorates, which is the foundation of its composability.
*   **Write-Through Caching**: This strategy is used to prioritize data consistency and durability over write speed, preventing data loss on crash.
*   **LRU Eviction**: A Least Recently Used (LRU) policy is used to keep frequently accessed data hot in memory while managing memory usage.
*   **Specialized `FutureQueue` Caching**: `ShadowedFutureQueue` is a specialized implementation that only caches the *head* of the queue. This is an optimization for the frequent `peek_due_time` operation, avoiding the overhead of keeping the entire queue synchronized in memory.
