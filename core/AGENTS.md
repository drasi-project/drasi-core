# AGENTS.md: `core` Crate

## Architectural Intent
The **abstract heart** of the engine. Defines the core logic and interfaces, strictly decoupled from storage backends and specific query parsers.

## Architectural Rules
*   **Dependency Injection**: All external dependencies (storage, time, parsing) are injected via `core::interface` traits.
*   **No Concrete Storage**: This crate MUST NOT depend on `index-rocksdb`, `index-garnet`, etc.
*   **Statelessness**: Logic components (`evaluation`, `path_solver`) must be stateless; state persistence is delegated to storage backends.

## Core Responsibilities
1.  **Interfaces (`interface`)**: Defines the contracts for `ElementIndex`, `ResultIndex`, `FutureQueue`.
2.  **Models (`models`)**: Defines canonical data structures (`Element`, `SourceChange`).
3.  **Computation (`path_solver`, `evaluation`)**: Implements the graph traversal and expression evaluation logic.
4.  **Orchestration (`query`)**: Provides `QueryBuilder` and the `ContinuousQuery` runtime loop.
5.  **Reference Impl (`in_memory_index`)**: Provides the default, ephemeral storage backend.
