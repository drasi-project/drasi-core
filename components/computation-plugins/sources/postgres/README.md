# Native PostgreSQL source

`drasi-computation-source-postgres` streams a PostgreSQL logical-replication
publication into a ComputationGraph (native ABI 4.0). It provides four
factories, each at implementation and configuration version `1`:

| Factory | Output |
|---|---|
| `drasi/postgres-transactions` | Whole committed transactions, for one atomic persistent query |
| `drasi.postgres/changes` | One envelope per row change, for one query |
| `drasi.postgres/shared-changes` | Row changes, for a fixed set of queries |
| `drasi.postgres/shared-transactions` | Whole commits, for a fixed set of atomic persistent queries |

```sh
cargo build -p drasi-computation-source-postgres --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: modes, cursors and feedback, the source
  binding, slot ownership, reconnection, lifecycle, management, security and
  differences from the legacy source.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults,
  ranges, secrets and update boundaries; server prerequisites; erase and
  failure runbook; migration from the previous configuration.

Minimal configuration (a managed slot, created by the source on first start):

```json
{"stream": "orders", "database": "shop", "user": "drasi",
 "password": {"secret": "postgres-password"}, "publication": "drasi_orders"}
```

Initial data comes from the paired providers in
[`drasi-computation-bootstrap-postgres`](../../bootstrappers/postgres), which
take the same settings.

## Qualification

```sh
make build-native-postgres-plugins
cargo test -p drasi-source-postgres --test native_transactions -- --ignored --test-threads=1
```

The live suite needs a Docker-compatible runtime (Podman works). It runs the
source, bootstrap and stored-procedure components through both the static
definitions and the built libraries; set `DRASI_NATIVE_PLUGIN_DIR` for a
non-default library directory. A missing library fails the suite rather than
skipping it.
