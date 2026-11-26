# Native MySQL sources

`drasi-computation-source-mysql` streams MySQL row-based binary logs into a
ComputationGraph (native ABI 0.2). Seven factories, each at implementation and
configuration version `1`:

| Factory | Output |
|---|---|
| `drasi.mysql/changes` | Untracked row changes |
| `drasi.mysql/untracked-transactions` | Untracked whole transactions |
| `drasi.mysql/transactions` | Whole transactions for one atomic persistent query |
| `drasi.mysql/shared-transactions` | Whole transactions for a fixed set of queries |
| `drasi.mysql/shared-changes` | Row changes for a fixed set of queries |
| `drasi.mysql/positional-changes` | Untracked `@N` rows (MySQL 5.7 / limited metadata) |
| `drasi.mysql/shared-positional-changes` | Positional rows for a fixed set of queries |

Each factory declares its progress contract: the replaying factories (and
shared rows with `persistent: true`) require durable progress, which the
graph checks when it binds the progress resource; the untracked factories
declare volatile progress and resume at `startFrom` after a restart.

```sh
cargo build -p drasi-computation-source-mysql --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: capture, element identity and types, cursors and
  binding, snapshot handover (FTWRL), positional profile, retention checks, DDL
  policy, pause, reconnection, lifecycle, erase, management, security and
  differences from the legacy source.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges,
  secrets and update boundaries; server prerequisites; failures and runbook;
  migration from the previous `settings` configuration.

Minimal configuration:

```json
{"stream": "orders", "database": "shop", "user": "drasi",
 "password": {"secret": "mysql-password"}, "tables": ["orders"]}
```

Initial data comes from the paired providers in
[`drasi-computation-bootstrap-mysql`](../../bootstrappers/mysql), which take the
same flat settings. MariaDB is not supported.

## Qualification

`make test-native-mysql` builds the three MySQL libraries and runs the Host
suite (`native_mysql_components`) with Podman, static and dynamic, on both Tokio
flavours. The crate's own live suite (`tests/native_live.rs`, Podman, ignored by
default) drives the shared engine directly through test-only adapters (erase,
pause across server timeouts, reconnect after a killed session, DDL policy,
large transactions, extended types, process exits, coordinated snapshots and
shared consumer replay) and builds its graph cases from this plugin's factories:

```sh
cargo test --locked -p drasi-computation-source-mysql --test native_live -- --ignored --test-threads=1
```
