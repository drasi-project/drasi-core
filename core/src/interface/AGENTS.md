# AGENTS.md: `core/src/interface` Module

## Architectural Intent

This is the most important architectural module in the engine. It defines the **abstract contracts** (traits) that decouple the core query logic from its concrete dependencies, particularly storage.

This module embodies the **Interface-Driven Design** and **Pluggable Backends** principles. The traits create stable "seams" in the architecture, allowing different backends (in-memory, RocksDB, mocks) to be plugged in, decorated with caches, and tested independently.

## Architectural Rules

*   **Stability is Paramount**: Changes to these traits are high-impact and must be made with care, as they affect all implementers.
*   **Define Contracts, Not Implementations**: Traits must describe *what* a service does, not *how*. Method documentation must clearly define expected behavior.
*   **Generality**: Interfaces must be general enough to be implemented by various backends (e.g., in-memory, embedded key-value, distributed caches). Avoid tying an interface to a specific technology's capabilities.

## Core Interface Contracts

*   **`ElementIndex`**:
    *   **Intent**: To abstract the storage of the graph's topology and properties.
    *   **Contract**: Implementers must provide atomic, consistent access to graph elements and efficient methods for graph traversal, which are critical for `path_solver` performance.

*   **`ResultIndex`**:
    *   **Intent**: To abstract the storage of query results and the intermediate state required for stateful aggregations (e.g., `sum`, `count`).
    *   **Contract**: Implementers must provide durable and consistent storage for aggregate accumulators.

*   **`FutureQueue`**:
    *   **Intent**: To abstract the scheduling of future work, essential for temporal functions (e.g., `drasi.trueLater`).
    *   **Contract**: Implementers must provide a reliable, time-ordered queue with efficient methods to add, remove (cancel), and peek at the next due item.

*   **`QueryClock`**:
    *   **Intent**: To abstract the concept of "time" for deterministic testing.
    *   **Contract**: Implementers must provide a source for transaction time and real-world time.

*   **`SourceMiddleware`**:
    *   **Intent**: To provide a pluggable mechanism for pre-processing incoming data, decoupling transformation logic from the core engine.
    *   **Contract**: Implementers must be able to accept a `SourceChange` and transform it into zero, one, or many `SourceChange`s.
