# AGENTS.md: `shared-tests/src/in_memory` Module

## Architectural Intent
The **reference test runner** for the Drasi engine. It binds the generic test suite (`src/use_cases`) to the default in-memory storage backend.

## Architectural Rules
*   **No Custom Logic**: This module should NOT contain test logic. It should only contain the "glue" code to instantiate the test runner with the in-memory configuration.
*   **Comprehensive Coverage**: Every use case defined in `shared-tests/src/use_cases` should have a corresponding runner in this module to ensure the core engine is fully validated 
      against the reference implementation.

## Implementation Details
*   **`InMemoryQueryConfig`**: Implements `QueryTestConfig`. Its `config_query` method explicitly injects `InMemoryElementIndex` (with archive enabled). It relies on `QueryBuilder` 
      defaults for the `ResultIndex`.
*   **Test Modules**: Each module (e.g., `mod building_comfort`) corresponds to a use case and contains `#[tokio::test]` functions that invoke the shared test logic.