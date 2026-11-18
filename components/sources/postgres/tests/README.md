# PostgreSQL Source Integration Tests

These integration tests validate logical replication change detection using a real PostgreSQL instance via testcontainers.

The live qualification suite of the native PostgreSQL plugins lives with the
native source crate:
[`components/computation-plugins/sources/postgres/tests`](../../../computation-plugins/sources/postgres/tests).

## Legacy per-row source

```bash
cargo test -p drasi-source-postgres --test integration_tests -- --ignored --nocapture
```

These legacy tests are ignored by default because they require a container engine.
