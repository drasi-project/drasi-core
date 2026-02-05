# Running tests

## Types of tests

We apply the [testing pyramid](https://martinfowler.com/articles/practical-test-pyramid.html) to divide our tests into groups for each feature.

- Unit tests: exercise functions and types directly
- Integration tests: exercise features working with dependencies

## Unit tests

Unit tests live within the crate of each component and can be run with the following command:

```sh
cargo test
```

We require unit tests to be added for new code as well as when making fixes or refactors in existing code. As a basic rule, ideally every PR contains some additions or changes to tests.

Unit tests should run with only the [basic prerequisites](../contributing-code-prerequisites/) installed. Do not add external dependencies needed for unit tests, prefer integration tests in those cases.

## Integration tests

Integration tests exercise features working with real dependencies like databases and storage systems. All integration tests run automatically when you execute `cargo test` from the workspace root, both locally (if dependencies are available) and in CI.

### Core Storage Integration Tests

The [shared-tests](../../../../shared-tests/) directory contains a suite of scenario tests that spin up continuous queries, push changes through them, and assert the results.  

Running `cargo test` in this directory will run these tests against the in-memory storage implementation.

These scenarios are also shared by the Garnet/Redis and RocksDB storage implementations:

- **Garnet/Redis tests** (`components/indexes/garnet/tests/`): Tests run against a real Garnet/Redis instance. By default, it will try to use the connection string of `redis://127.0.0.1:6379`, but this can be overridden by setting the `REDIS_URL` environment variable.

- **RocksDB tests** (`components/indexes/rocksdb/tests/`): Tests run against a real RocksDB, which is embedded as an in-process library. Running the tests will create a `test-data` directory where the RocksDB files will be stored.

### Database Reaction Integration Tests

Components that interact with external databases include integration tests using [testcontainers](https://github.com/testcontainers/testcontainers-rs) to automatically manage database instances:

- **PostgreSQL** (`components/reactions/storedproc-postgres/tests/`): Tests stored procedure reactions against a real PostgreSQL database
- **MySQL** (`components/reactions/storedproc-mysql/tests/`): Tests stored procedure reactions against a real MySQL database  
- **MS SQL Server** (`components/reactions/storedproc-mssql/tests/`): Tests stored procedure reactions against Azure SQL Edge

These tests automatically:
1. Pull the required Docker images (first run only)
2. Start database containers with random ports
3. Run tests against real database instances
4. Clean up containers when tests complete

**Requirements:**
- Docker must be installed and running
- Sufficient permissions to start containers
- Internet access to pull images (first run)

**Note for Apple Silicon developers:** MS SQL Server tests are automatically skipped on ARM64 (`aarch64`) due to Azure SQL Edge platform limitations.

### Other Integration Tests

- **SSE Reaction** (`components/reactions/sse/tests/`): Integration tests for Server-Sent Events reactions with full DrasiLib setup
- **Application Source**: Tests use the in-memory Application source for end-to-end scenarios

### Running Integration Tests

**Run all tests (unit + integration):**
```sh
cargo test
```

**Run only integration tests for a specific component:**
```sh
cargo test -p drasi-reaction-storedproc-postgres
cargo test -p drasi-index-garnet
```

**Run with verbose output to see container lifecycle:**
```sh
RUST_LOG=debug cargo test
```

### In Continuous Integration

The [Run Tests workflow](../../../../.github/workflows/test.yml) executes all integration tests automatically:
- Redis service container is provided for Garnet/Redis tests
- Docker is available for testcontainers-based database tests
- All integration tests run on every pull request and push to main

