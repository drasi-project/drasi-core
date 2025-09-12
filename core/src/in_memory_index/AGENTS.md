# AGENTS.md: `core/src/in_memory_index` Module

## Architectural Intent
The **reference, in-memory implementation** of storage traits (`ElementIndex`, `FutureQueue`, `ResultIndex`).
*   **Goal:** Maximum read performance for non-persistent workloads.
*   **Scope:** Zero-configuration. strictly no external infrastructure dependencies (e.g., no Redis/RocksDB).

## Rules
*   **Thread Safety:** MUST use `tokio::sync::RwLock` (not `Mutex`) for all shared state to ensure concurrent read access.
*   **Correctness:** Strict adherence to `core::interface` contracts is prioritized over memory efficiency.
*   **Dependencies:** Use only standard Rust libraries and those already defined in `core/Cargo.toml`.

## Patterns
*   **State Management:** Uses `Arc<RwLock<T>>` for granular locking of distinct stores.
*   **Indexing Strategy:** High write-amplification (multiple secondary indexes) is accepted to guarantee O(1) graph traversal and lookups.
*   **Trait Composition:** `ResultIndex` is composed of multiple specialized traits (`AccumulatorIndex`, `LazySortedSetStore`) rather than monolithic logic.
*   **Temporal Storage:** Optional archival support uses `BTreeMap` for efficient time-range queries.
