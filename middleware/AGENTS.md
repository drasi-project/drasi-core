# AGENTS.md: `middleware` Crate

## Architectural Intent

This crate provides a **standard library of concrete, reusable data transformation components**. It contains implementations for common, declaratively configurable pre-processing tasks.

Its architectural role is to **extend the engine's capabilities** with a toolbox of transformations (e.g., `unwind`, `promote`). It is the primary implementer of the middleware framework defined in `drasi-core`.

## Architectural Rules

*   **Implement `core` Interfaces**: Every middleware MUST implement the `SourceMiddleware` and `SourceMiddlewareFactory` traits from `drasi-core`.
*   **Single Responsibility**: Each middleware module must have a single, well-defined purpose.
*   **Stateless and Declarative**: Middleware must be stateless where possible, with all configuration passed in declaratively.
*   **Robust Error Handling**: Middleware must handle errors gracefully according to the configured `on_error` policy (`skip` or `fail`).

## Relationship to `drasi-core`

This crate provides the "plugins" for the "plugin framework" in `core::middleware`.
1.  `drasi-core` defines the `trait`s and the `SourceMiddlewarePipeline` orchestrator.
2.  This `middleware` crate provides the concrete structs (e.g., `UnwindMiddleware`) that implement those traits.
3.  At runtime, the `drasi-core` engine uses the factories from this crate to build the execution pipeline.
