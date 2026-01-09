# AGENTS.md: `core/src/middleware` Module

## Architectural Intent
A **framework for pluggable data transformation pipelines**. It decouples "data preparation" (parsing, unwinding, reshaping) from the core query engine.

## Architectural Rules
*   **Separation of Concerns**: This module contains only the *framework* (interfaces, pipeline orchestration). Concrete middleware implementations (e.g., `JsonParser`) MUST reside in 
      separate modules/crates to keep the core lightweight.
*   **One-to-Many Transformation**: The pipeline architecture explicitly supports `1 -> N` expansion (e.g., unwinding an array into multiple events).
*   **Configuration-Driven**: Middleware pipelines are instantiated dynamically at runtime based on configuration data, not static code binding.

## Core Patterns
*   **Pipeline Pattern**: `SourceMiddlewarePipeline` orchestrates a linear sequence of transformations. Each step feeds into the next, managing the complexity of `Vec<SourceChange>` 
      flattening.
*   **Factory Registry**: `MiddlewareTypeRegistry` maps string identifiers (e.g., `"unwind"`) to `SourceMiddlewareFactory` instances. This allows new middleware types to be registered without modifying the orchestration logic.