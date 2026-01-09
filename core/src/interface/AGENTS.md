# AGENTS.md: `core/src/interface` Module

## Architectural Intent
This module defines the **abstract contracts** (traits) that decouple the core engine from its dependencies. It embodies **Interface-Driven Design**, creating stable seams for pluggable backends (storage, timing, middleware).

## Architectural Rules
*   **Stability**: These traits are the "spine" of the system. Changes here are high-impact breaking changes.
*   **Abstraction**: Traits must define *capabilities*, not implementation details. They must be implementable by diverse backends (in-memory, RocksDB, Redis).
*   **Async First**: All I/O-bound operations must be `async` to support high-throughput, non-blocking runtimes.

## Core Contracts

### Storage Abstractions
*   **`ElementIndex`**: Abstract access to the graph structure (Nodes & Relations).
    *   *Constraint*: Must support efficient O(1) lookups for graph traversal (e.g., `get_slot_elements_by_inbound`).
*   **`ResultIndex`**: Abstract storage for query results and aggregation state.
    *   *Composition*: Composed of `AccumulatorIndex` (for aggregation values) and `ResultSequenceCounter` (for logical time).
*   **`ElementArchiveIndex`**: **Optional** capability for retrieving historical versions of elements, enabling temporal queries.
*   **`FutureQueue`**: Abstract scheduling for time-based operations.
    *   *Constraint*: Must support strict ordering by `due_time`.

### Extension Points
*   **`SourceMiddleware`**: Contract for data transformation plugins.
    *   *Pattern*: A pipeline stage that transforms one `SourceChange` into many.
*   **`SourceMiddlewareFactory`**: The "Builder" contract used by the registry to instantiate middleware from configuration.

### System Services
*   **`QueryClock`**: Abstracts "time" (transactional & real-world) to ensure deterministic execution and testing.
