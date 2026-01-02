# AGENTS.md: `core/src/in_memory_index` Module

## Architectural Intent

This module provides a **complete, high-performance, in-memory implementation** of the storage `interface` traits. It serves as the default, "batteries-included" backend for the engine.

Its primary design goals are:
1.  **Performance**: To be the fastest possible backend for non-persistent scenarios.
2.  **Reference Implementation**: To serve as the canonical, correct implementation of the storage interfaces, acting as a baseline for testing and benchmarking.
3.  **Simplicity**: To provide a zero-configuration backend for ease of use in testing and development.

## Architectural Rules

*   **Correctness**: This implementation must perfectly adhere to the contracts defined in `core::interface`.
*   **Thread Safety**: All access to shared data structures MUST be protected by appropriate locking mechanisms (e.g., `RwLock`) to ensure safe concurrent access.
*   **No External Dependencies**: This module must only rely on the parent `core` crate and standard Rust libraries.

## Key Design Decisions

*   **`Arc<RwLock<T>>`**: This pattern is used for all shared data stores to enable safe, concurrent, multi-threaded access. `RwLock` is preferred to allow parallel reads.
*   **Secondary Indexes for Performance**: The `InMemoryElementIndex` maintains multiple `HashMap`s (e.g., `element_by_slot_inbound`). This trade-off (increased memory usage and write amplification) is made to achieve O(1) lookups for critical path-matching operations.
*   **Separated Stores**: The implementations for `ElementIndex`, `FutureQueue`, and `ResultIndex` are in separate structs to align with the interface segregation principle and improve maintainability. A single monolithic store was intentionally avoided.
