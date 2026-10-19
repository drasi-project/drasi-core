# PostgreSQL Source Integration Tests

These integration tests validate logical replication change detection using a real PostgreSQL instance via testcontainers.

## Native plugins

Build all three native PostgreSQL libraries before the required recovery suite:

```sh
make build-native-postgres-plugins
cargo test -p drasi-source-postgres --test native_transactions -- --test-threads=1
```

`DRASI_NATIVE_PLUGIN_DIR` can select another directory of the actual shared
libraries. Missing libraries fail rather than skip. The `libraries` module checks
static/dynamic source, bootstrap and stored-procedure factories in one DrasiLib
graph, TLS, restart, actual-owner rejection, concurrent snapshot/WAL handover,
cancelled initialization, SQL rollback, partial delivery and destination receipts.
The other modules retain their strict decoder, source feedback/backpressure,
snapshot recovery, verified TLS and required child-process crash assertions.

The native bootstrap crate also has a deterministic protocol regression: an
isolated scripted PostgreSQL endpoint supplies a fixed timestamp, and emitted
snapshot rows must use that exact database time sampled before the WAL boundary.
This does not rely on machine or container clock skew.

```sh
cargo test -p drasi-computation-bootstrap-postgres --lib
```

For Podman, set `DOCKER_HOST` to its API socket. All database cases create their
own isolated containers; no existing demo database, volume or configuration is used.

## Legacy per-row source

```bash
cargo test -p drasi-source-postgres --test integration_tests -- --ignored --nocapture
```

These legacy tests are ignored by default because they require a container engine.
