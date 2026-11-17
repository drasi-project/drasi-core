# Native PostgreSQL stored-procedure sink

`drasi-computation-sink-storedproc-postgres` exports one native
ComputationGraph factory, `drasi.postgres/storedproc` (implementation `1`,
configuration version `1`, native ABI 4.0, consumer extension). Its `in` port
accepts standard graph-change envelopes from any source, transformer or query
([sink input contract](../../SINKS.md)). It runs inside the hosting graph,
statically or as a separately loaded library, and does not depend on
`drasi-lib`, the query engine or the legacy SDK.

It is an external consumer for the host `DeliveryRunner`: each operation's
procedure call commits in one PostgreSQL transaction together with a
per-consumer destination cursor row, so retries after a lost connection, an
unknown commit outcome or a crash do not repeat committed effects. Procedures
that `COMMIT` or `ROLLBACK` themselves are refused. With
`onPermanentFailure: skip`, an undeliverable operation is recorded in a
dead-letter table atomically with the cursor advance. The protocol is shared
with the MySQL and SQL Server sinks
([storedproc-common](../../common/storedproc/README.md)). Command templates are parameter-only
Handlebars: values are always bound as `$n` parameters.

```sh
cargo build -p drasi-computation-sink-storedproc-postgres --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: architecture, delivery profile with exact
  duplicate and loss windows, the format-4 cursor protocol and exactly-once
  conditions, retry and classification, lifecycle per hook, management,
  security model and differences from the legacy reaction.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges,
  secrecy and update boundaries, examples, the template reference, procedures
  and grants (including a pre-created progress table for least-privilege
  roles), Erase, an operations runbook and the legacy mapping.

Minimal configuration (supply `password` as a secret reference):

```json
{"host": "db.example", "database": "app", "user": "drasi_sink",
 "defaultTemplate": {"added": {"template": "CALL add_user({{param after.id}}, {{param after.name}})"}}}
```

## Qualification

```sh
cargo test -p drasi-computation-sink-storedproc-postgres
# the shared protocol table, commit classification and lost-COMMIT cases:
cargo test -p drasi-computation-sink-storedproc-postgres --test live -- --ignored --test-threads=1
cargo build --locked --features dynamic-plugin -p drasi-computation-source-postgres \
  -p drasi-computation-bootstrap-postgres -p drasi-computation-sink-storedproc-postgres
# copy the three libraries into target/debug/plugins/, then:
cargo test --locked -p drasi-source-postgres --test native_transactions -- --ignored \
  --test-threads=1 storedproc
```

The live cases (Podman/Docker) cover static and dynamic end-to-end delivery,
parameter safety, TLS, restart, partial batches, destination deduplication,
retry classification, cancellation, Erase, per-owner stream limits, reconnect
after `pg_terminate_backend`, recovery without restart, a least-privilege role
with a pre-created table and template-injection rejection.
