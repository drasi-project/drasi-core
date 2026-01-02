# AGENTS.md: `core` Crate

## Architectural Intent

This crate is the abstract heart of the `drasi-core` engine. Its purpose is to define the core logic and interfaces of the continuous query engine while remaining completely decoupled from any specific storage backend or query language parser.

The key design principle is **inversion of control** (dependency injection), achieved through Rust's `trait` system. The engine's components operate on the abstract interfaces defined in `core::interface`, not on concrete implementations.

## Core Responsibilities

1.  **Define Interfaces (`interface`)**: Establish the contracts for all pluggable components, especially storage (`ElementIndex`, `ResultIndex`, `FutureQueue`). This is the most critical architectural boundary.
2.  **Define Data Models (`models`)**: Provide the canonical data structures (`Element`, `SourceChange`) that flow through the engine.
3.  **Provide the Processing Pipeline**: Contain the logic for the sequential stages of query execution (`middleware`, `path_solver`, `evaluation`).
4.  **Orchestrate the Runtime (`query`)**: Provide the public API (`QueryBuilder`) and the `ContinuousQuery` struct that manages execution flow.

## Architectural Rules

*   **No Concrete Storage Dependencies**: This crate MUST NOT have a direct dependency on `index-rocksdb`, `index-garnet`, or any other concrete storage implementation. All storage interactions must be mediated through the traits in `core::interface`.
*   **Genericity**: Components must be written to operate on the abstract interfaces to allow for flexible composition (e.g., wrapping a persistent index with a caching decorator).
*   **Stateless Logic**: Core logic components (e.g., `ExpressionEvaluator`) should be stateless. State should be managed by the storage backends.
*   **Unidirectional Data Flow**: Data flow from `SourceChange` to result diff is strictly unidirectional through the pipeline stages.

## Module Responsibilities

*   **`interface`**: **The most important module.** Defines the architectural seams (traits) of the engine, enabling the pluggable backend system.
*   **`evaluation`**: The computational core. Executes query functions, filters, and aggregations.
*   **`query`**: The public entry point and runtime orchestrator.
*   **`in_memory_index`**: The default, reference implementation of the storage interfaces for testing and non-persistent scenarios.