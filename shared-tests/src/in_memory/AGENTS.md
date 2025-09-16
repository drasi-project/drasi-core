# AGENTS.md: `shared-tests/src/in_memory` Module

## Architectural Intent

This module is the **built-in, reference test runner** for the `shared-tests` suite. It connects the generic test logic from `src/use_cases` with the default `in_memory` storage backend from the `drasi-core` crate.

Its architectural roles are:
1.  **Primary Validation**: To provide the main correctness validation for the `drasi-core` engine's evaluation and path-solving logic.
2.  **Canonical Example**: To serve as the reference implementation for how external storage backend crates should create their own test runners.

## How It Works

This module acts as the "glue" to make the generic tests runnable against the default backend.
1.  It defines `InMemoryQueryConfig`, a concrete implementation of the `QueryTestConfig` trait.
2.  This config struct constructs a `QueryBuilder` explicitly wired to use the `InMemoryElementIndex`.
3.  A series of `#[test]` functions instantiate `InMemoryQueryConfig` and pass it to the corresponding generic test logic from `src/use_cases`.

Running `cargo test` in the `shared-tests` crate executes these functions.
