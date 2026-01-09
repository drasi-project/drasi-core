# AGENTS.md: `shared-tests` Crate

## Architectural Intent
A **reusable compliance test suite** for the Drasi engine. It defines the canonical behavior for queries and ensures that all storage backends (Memory, RocksDB, Redis) behave 
      identically.

## Architectural Rules
*   **Decoupling**: Test logic MUST NOT depend on concrete storage implementations. It interacts with the engine solely through the `QueryBuilder`.
*   **Parameterized Execution**: Tests are parameterized by the `QueryTestConfig` trait, which allows the caller (the test runner) to inject the specific storage backend configuration.

## Test Categories
1.  **Use Case Scenarios (`src/use_cases`)**: End-to-end tests validating complex query logic (e.g., "incident alert", "linear regression").
2.  **Targeted Functionality**:
    *   **`src/index`**: Validates specific index behaviors (e.g., `FutureQueue` ordering).
    *   **`src/temporal_retrieval`**: Validates time-travel query capabilities (`ElementArchiveIndex`).
    *   **`src/sequence_counter`**: Validates logical clock sequencing.

## Implementation Details
*   **`src/in_memory`**: The internal test runner that executes the suite against the default in-memory backend during `cargo test`.
*   **`QueryTestConfig`**: The trait contract for configuring the `QueryBuilder` with a specific backend.
