# AGENTS.md: `core/src/query` Module

## Architectural Intent

This module is the **primary public API and runtime orchestrator** for the engine. It provides a user-friendly interface for configuring and running a continuous query, while hiding internal complexity.

The key architectural patterns are:
1.  **Builder Pattern (`QueryBuilder`)**: Provides a fluent and flexible API for constructing a query, managing complex configuration and dependencies.
2.  **Facade Pattern (`ContinuousQuery`)**: The `ContinuousQuery` struct acts as a facade, coordinating all internal components and exposing a single `process_source_change` method.

## Architectural Rules

*   **User-Friendly API**: The `QueryBuilder` is the main entry point and its API must be clear and guide the user toward a correct configuration.
*   **Dependency Injection Point**: The builder is the primary mechanism for dependency injection, allowing users to provide their own `interface` trait implementations.
*   **Orchestration, Not Logic**: The `ContinuousQuery` struct must only contain orchestration logic (calling pipeline stages in order). Core computational logic is delegated to other components.

## Core Component Intent

*   **`QueryBuilder`**: Solves the complex construction problem for a `ContinuousQuery`, avoiding a complex constructor and providing sensible defaults (e.g., `in_memory_index`).
*   **`ContinuousQuery`**: Encapsulates the state and logic of a live query instance. It is the primary runtime object for processing data.
*   **`AutoFutureQueueConsumer`**: Provides a simple, "fire-and-forget" background task to handle temporal functions by consuming the `FutureQueue` and feeding events back into the query. This encapsulates the non-trivial polling logic.
