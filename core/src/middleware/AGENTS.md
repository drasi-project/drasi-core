# AGENTS.md: `core/src/middleware` Module

## Architectural Intent

This module provides a **declarative and extensible framework for pre-processing incoming data**. It enables the creation of reusable data transformation pipelines for `SourceChange` events.

The key architectural goal is to **decouple data transformation logic from the core query evaluation logic**. This module provides a dedicated layer for data preparation tasks (e.g., parsing JSON, unwinding arrays), keeping the query engine focused on evaluation.

## Architectural Rules

*   **Declarative First**: Middleware usage must be configurable declaratively (e.g., via YAML). The framework is designed to translate this configuration into a runtime pipeline.
*   **Composability**: Middleware components should be small, single-purpose, and composable into a pipeline.
*   **Extensibility**: The framework is built around the `MiddlewareTypeRegistry`. New middleware is added by implementing the `SourceMiddleware` and `SourceMiddlewareFactory` traits and registering the factory.

## Key Design Decisions

*   **Registry and Factory Pattern**: A `MiddlewareTypeRegistry` (Service Locator) maps string identifiers (e.g., "unwind") to `SourceMiddlewareFactory` instances. This allows the framework to be extended at runtime without changing the orchestration logic.
*   **Pipeline Orchestration**: The `SourceMiddlewarePipeline` orchestrates the ordered execution of a middleware sequence. It is designed to handle one-to-many transformations (e.g., one `SourceChange` unwinding to a `Vec<SourceChange>`).
*   **Framework/Implementation Separation**: This module provides the framework. Concrete implementations (e.g., `unwind`, `promote`) reside in the separate `middleware` crate.
