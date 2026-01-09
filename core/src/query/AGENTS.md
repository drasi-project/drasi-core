# AGENTS.md: `core/src/query` Module

## Architectural Intent
The **runtime orchestration layer** for the engine. It binds together the disjointed components (solver, evaluator, storage) into a cohesive, running query instance.

## Architectural Rules
*   **Builder-First Construction**: `ContinuousQuery` is complex to instantiate. Users MUST use `QueryBuilder` to ensure all components are correctly wired and defaults are applied.
*   **Dependency Injection**: The builder serves as the composition root, allowing external implementations of `interface` traits (like storage backends) to be injected.
*   **Change Processing Logic**: While it delegates graph traversal and expression evaluation, `ContinuousQuery` IS RESPONSIBLE for:
    1.  **Delta Calculation**: Computing "Before" vs "After" states for updates/deletes.
    2.  **Concurrency Control**: Managing locking and state transitions for incoming changes.

## Core Components
*   **`QueryBuilder`**: Implements the Builder pattern to construct `ContinuousQuery` instances, handling the registration of built-in functions and default storage backends.
*   **`ContinuousQuery`**: The primary runtime object. It receives `SourceChange` events, orchestrates them through the middleware -> solver -> evaluator pipeline, and manages the 
      lifecycle of the query.
*   **`AutoFutureQueueConsumer`**: A utility background task that polls the `FutureQueue` and feeds due events back into the query to drive temporal functions.
