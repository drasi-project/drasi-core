# AGENTS.md: `shared-tests` Crate

## Architectural Intent

This crate's purpose is twofold:
1.  **Define a generic, backend-agnostic test suite** for the `drasi-core` engine, located in `src/use_cases`.
2.  **Provide a built-in test runner** that validates the core logic against the default **in-memory** storage backend, located in `src/in_memory`.

The core architectural principle is to **decouple test logic from storage implementation**, allowing the same test suite to run against any backend to ensure behavioral consistency.

## Architectural Rules

*   **Generic Test Logic**: New tests in `src/use_cases` must be generic, parameterized by the `QueryTestConfig` trait, and must not depend on any concrete storage implementation.
*   **Concrete Test Runners**: Each storage backend is responsible for running the generic test suite against itself. The `in_memory` module provides the reference implementation for this pattern.

## Usage and Testing

*   **Internal Validation**: `cargo test` within this crate executes the `src/in_memory` runner. This is the primary CI validation for the core engine's logic.
*   **External Validation**: Storage crates (e.g., `index-rocksdb`) use this crate as a `dev-dependency`. They create their own test runner in their `tests/` directory that implements `QueryTestConfig` for their backend and calls the generic test functions from `src/use_cases`. This validates the correctness of the storage implementation.