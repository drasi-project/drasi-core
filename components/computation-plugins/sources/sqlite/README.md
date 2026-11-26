# Native SQLite source

`drasi-computation-source-sqlite` is a native ComputationGraph source (ABI
0.2.0) that owns a SQLite connection. Writes arrive through its REST API (or a
Rust `SqliteHandle` when the crate is linked), run under an authorizer and are
captured in the same transaction. Four factories, each at implementation and
configuration version `1`:

| Factory | Output | Recovery |
|---|---|---|
| `drasi.sqlite/changes` | Graph changes per commit | Volatile |
| `drasi.sqlite/transactions` | Complete source transactions | Volatile |
| `drasi.sqlite/replay-transactions` | Complete transactions from a source-owned journal | One persistent query |
| `drasi.sqlite/shared-transactions` | The same journal for a fixed consumer set | Fixed persistent consumers |

```sh
cargo build -p drasi-computation-source-sqlite --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: modes, journal tables and their ownership,
  authorizer, `initSql` and `/api/sql`, pause, two-phase stop, erase,
  management, REST security, WAL mode, the DDL restriction, external changes
  and differences from the legacy source.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges,
  secrets and update boundaries; REST routes and status codes; examples;
  erase; operations runbook; migration from the previous nested
  configuration.

Minimal configuration:

```json
{"stream": "sqlite/out", "path": "data/app.db", "tables": ["items"], "restApi": true}
```

The REST listener is off by default and binds loopback; a non-loopback
address needs `authToken` (a secret) or `allowUnauthenticated: true`.

## Host bindings

`NativePlugin::from_definition(drasi_computation_source_sqlite::plugin()?)` and
the built dynamic library expose the same four factories; match them by
implementation identity. `changes` and `transactions` take no progress
resource. `replay-transactions` requires the query's actual
`QuerySourceProgressResource` at the Checkpoint slot `source_progress`;
`shared-transactions` requires the actual `ConsumerProgressSet` at
`consumer_progress`. Construction validates configuration only: it opens no
database and starts no worker.

For coordinated initialization set `coordinatedSnapshot: true`. Keep the
source's ordinary `source_progress` or `consumer_progress` binding and add one
paired bootstrap provider node from
[`drasi-computation-bootstrap-sqlite`](../../bootstrappers/sqlite)
(`drasi.sqlite/coordinated-bootstrap` or `drasi.sqlite/shared-bootstrap`).
Bind each query's actual progress under the provider's `source_progress` slot
and connect its `snapshot` output to the source's `bootstrap` input. The source
creates its own coordinator from its actual progress owners; there is no
coordinator resource to declare. Connect the source's `snapshot` output to each
query's `bootstrap` input; the source relays each query's session from the
provider and adds its handover boundary. The paired factory accepts only
`source` and `stream`. See the
[Host contracts](../../../host-sdk/README.md#native-source-bootstrap-coordination).
For memory-backed queries, `drasi.sqlite/file-bootstrap` reads the file
directly; keep writers quiescent until the queries finish loading.

`DrasiLib::get_source_schema` and `get_graph_schema` read the captured table
schema through the source's schema reader while it runs.

## Qualification

```sh
make test-native-sqlite
# or, without rebuilding every test plugin:
cargo build --locked --lib -p drasi-computation-source-sqlite --features drasi-computation-source-sqlite/dynamic-plugin
cargo build --locked --lib -p drasi-computation-bootstrap-sqlite --features drasi-computation-bootstrap-sqlite/dynamic-plugin
# copy both libraries into <target>/debug/plugins, then:
cargo test --locked -p drasi-computation-source-sqlite --lib
cargo test --locked -p drasi-host-sdk --test native_sqlite_components -- --test-threads=1
```

The crate tests run the SDK conformance kit (lifecycle, pause gating,
cancelled start, erase twice, configuration round trip, assess matrix, in-place
and restart apply) on both Tokio flavours, plus the stop-deadlock scenario,
Drain timeout and Abort, pause of REST and handle writes, `initSql`,
`/api/sql` authentication and authorizer, REST limits and paging, and token
rotation. The Host suite runs the static definition and the built library on
both flavours, including persistent replay, erase twice and reuse of the file,
and security, pause and in-memory checks through the loaded library.
