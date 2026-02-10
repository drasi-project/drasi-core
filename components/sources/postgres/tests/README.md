# PostgreSQL Source Integration Tests

These integration tests validate logical replication change detection using a real PostgreSQL instance via testcontainers.

## Running tests

```bash
cargo test -p drasi-source-postgres --test integration_tests -- --ignored --nocapture
```

The tests are ignored by default because they require Docker.
