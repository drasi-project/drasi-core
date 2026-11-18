# Native SQL Server CDC source

`drasi-computation-source-mssql` streams SQL Server change data capture into a
ComputationGraph (native ABI 5.0). It exports one factory,
`drasi.mssql/changes`, at implementation and configuration version `1`: one
envelope per row change for a fixed set of consumers, each with its own
committed cursor and replay.

```sh
cargo build -p drasi-computation-source-mssql --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: the CDC polling model, cursor and binding,
  grouping, commit times, value mapping, snapshot handover and owned markers,
  pause, stop, reconnect, erase, replay retention, management, security and
  differences from the legacy source.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges,
  the secret and update boundaries; server prerequisites and permissions;
  encryption and authentication; erase, failures, operations and migration
  from the previous nested configuration.

Minimal configuration (flat; `password` is the only secret):

```json
{"stream": "orders/out", "host": "sql.internal", "database": "shop",
 "user": "drasi_cdc", "password": {"secret": "mssql-password"},
 "tables": ["dbo.Orders"]}
```

Initial data comes from the paired providers in
[`drasi-computation-bootstrap-mssql`](../../bootstrappers/mssql), which take
the same settings.

## Qualification

**Live SQL Server qualification is explicitly deferred by the user (CW-78).**
The source is verified by unit tests against an in-memory implementation of
its database seam (paging, cursors, grouping, retention, binding, pause,
stop, reconnect, erase and update planning) and by database-free Host checks
of the static and dynamic libraries. The live suite in
`components/host-sdk/tests/native_mssql_components.rs` compiles and is kept
current for a future authorized run; see
`components/host-sdk/tests/fixtures/native_mssql/README.md`.

```sh
cargo test -p drasi-computation-source-mssql
cargo test -p drasi-mssql-common --features native,sql
```
