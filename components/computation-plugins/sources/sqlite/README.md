# Native SQLite source

Independent native source library using ComputationGraph ABI `1.0.0`.
All factories use implementation/configuration version `1` and reuse the same
bounded SQLite worker, SQL handles, capture/journal and REST routes as the
host-linked native Rust source.

| Factory | Output | Recovery |
|---|---|---|
| `drasi.sqlite/changes` | `GraphChangeCodec` | No source journal or persistent recovery claim |
| `drasi.sqlite/transactions` | `SourceTransactionCodec` | Atomic source groups without restart replay |
| `drasi.sqlite/replay-transactions` | `SourceTransactionCodec` | Actual single persistent query owner |
| `drasi.sqlite/shared-transactions` | `SourceTransactionCodec` | Actual fixed persistent consumer set |

Configuration contains `stream`, `settings` (`SqliteConfig`) and optional
`rest_api`. Persistent factories also accept `coordinated_snapshot`. Output/replay
settings must match the chosen factory. Replay requires a real file and explicit
journal bounds; existing rows require coordinated initialization, not silent
adoption or a state reset. Transaction consumers require atomic,
transaction-enabled query storage.

The existing [SQL, REST and recovery contracts](../../../sources/sqlite/README.md)
apply to the shared implementation. SQL writes use the single owned connection;
query evaluation remains in the hosting DrasiLib instance's graph. This library
does not depend on the graph runtime, query evaluator/parsers or legacy SDK.

## Host bindings

Both `NativePlugin::from_definition(drasi_computation_source_sqlite::plugin()?)`
and a freshly built dynamic library expose the same four factories. Match
factories by implementation identity, not catalogue index. Use the factory's
actual specification/descriptor to construct the graph.

`changes` and `transactions` require `settings.replay: null` and no progress
resource. `replay-transactions` requires the query's actual
`QuerySourceProgressResource` at Checkpoint slot `source_progress`.
`shared-transactions` instead requires the actual `ConsumerProgressSet` at
Checkpoint slot `consumer_progress`. Names alone are not ownership evidence,
and these bindings cannot be interchanged or supplied together. Construction
retains capabilities without reading progress, opening SQLite, or starting a
worker.

The persistent factories require file-backed complete-transaction output and
atomic persistent query storage with `source_transactions` enabled. They reuse
the unchanged journal/position formats from the Rust source. Every member of a
shared binding participates in readiness, generation, failure, replay and
retirement checks. No aggregate cursor or durability is manufactured.

## REST and initialization

`rest_api` is an optional object containing `host` and `port`. The listener runs
inside the source and uses its existing SQL worker, not a second connection.
The routes preserve table filtering, configured keys, primary keys and physical
rowid lookup, CRUD, and atomic transactional batches. Reserved `__drasi_` tables
are not exposed. A successful HTTP response acknowledges local SQL completion,
not downstream query processing.

For ordinary memory queries, attach `drasi.sqlite/file-bootstrap` from the
independent bootstrap library to each query. Keep REST/other writers quiescent
until all queries finish initialization. This mode supplies no source journal
watermark and does not replace coordinated persistent initialization.

Source stop closes new HTTP admission, drains accepted requests, and joins the
real REST/database workers. Cancellation or a cleanup deadline keeps those
workers owned and prevents restart until cleanup completes. A generation-bound
client never follows a replacement worker. Transaction callbacks, rollback and
captured row changes are shared with the host-linked source rather than
reimplemented in the plugin.

## Rust handles and internal coordination

Rust callers retain the source-owned `SqliteHandle` and its existing scoped
transaction callbacks. REST retains the same SQL behavior. Neither requires a
public DrasiLib SQL API or a public SQL-access service across the ABI. Source
schema inspection is a separate read-only capability backed by the actual
bounded worker, not a SQL execution endpoint.

Set `coordinated_snapshot: true` on a persistent source to use its source-owned
snapshot engine with the independently loaded bootstrap library:

| Source | Paired bootstrap | Actual progress |
|---|---|---|
| `drasi.sqlite/replay-transactions` | `drasi.sqlite/coordinated-bootstrap` | One query owner |
| `drasi.sqlite/shared-transactions` | `drasi.sqlite/shared-bootstrap` | Complete fixed consumer set and each actual query owner |

Create one graph-owned `nativeSourceBootstrap` resource using the source's exact
factory identity and configuration. Its `sourceProgress` or `consumerProgress`
dependency must be the source's actual live progress resource. Bind it to the
source's `source_bootstrap` slot, keeping the ordinary progress slot too.
Each query has a separate `nativeBootstrap` resource with `sourceBootstrap`
naming that coordinator and `sourceProgress` naming that query's actual progress.
Declare both dependencies and bind the provider under the query's `bootstrap`
slot. The paired factory accepts only `source` and `stream`; SQLite settings and
the one engine belong to the source resource, not a copied bootstrap connection.
See the [Host resource contracts](../../../host-sdk/README.md#internal-native-sourcebootstrap-coordination).

The graph activates bootstrap readers before source readiness. Live sources use
their separately bound live readers; identical names never substitute for the
actual owners. Snapshot and live workers share one exclusive lease. Interrupted
loading stays incomplete under strict recovery; explicit `AutoReset` can finish
an incomplete initialization without deleting source rows or replacing journal
identity. Drained reconstruction supports safe member enrollment/removal using
the existing shared engine's rules.

Successful provider stop permits replacement while retaining its old handle;
that old handle cannot restart or cancel the replacement. Cancelled/failed stop
does not release the lease. The SDK owns exported-provider leases and retirement;
the family retains actual worker/client leases and refuses unfinished engine
rebinding. Merely constructing a provider does not retire an older stopped export.
Coordinator resource retirement permanently fences new source/provider creation
and waits for real workers. There is no path registry,
second coordinator in the bootstrap library, or public SQL service.

`DrasiLib::get_source_schema` and `get_graph_schema` expose the actual captured
table schema through the optional native schema extension, including Server-hosted
instances. A pending `next()` does not prevent inspection. Stopped, retired or
failed reads return errors rather than configuration metadata or empty success.

## Server configuration

`drasi-server/examples/native-sqlite.rs` generates canonical configuration from
the loaded factory metadata. Initialize an `items(id INTEGER PRIMARY KEY, value
TEXT NOT NULL)` table, then run from the Server checkout:

```sh
cargo run --example native-sqlite -- <plugin-directory> <database> <rest-port> file
cargo run --example native-sqlite -- <plugin-directory> <database> <rest-port> single <index-directory>
cargo run --example native-sqlite -- <plugin-directory> <database> <rest-port> shared <index-directory>
```

File mode has two memory queries and requires quiescent writers during loading.
Single mode has one persistent query; shared mode has two. Paired modes require
an explicit persistent index directory and gate REST admission until snapshots
commit. Save the generated YAML and start Server with those same source/bootstrap
libraries. The source and query-result HTTP listeners are separate.

## Capability preservation

| Existing capability | Native implementation |
|---|---|
| Source-owned SQL, parameters, query results and transaction callbacks | Original Rust APIs remain; the shared bounded worker executes native operations |
| Commit/rollback capture, savepoints, insert/update/delete | Reused capture/transaction engine, including atomic rollback and key changes |
| Memory/file databases, table filters, custom/composite keys, values and rowids | Shared settings, schema and mapper; snapshot/live native identities agree |
| Optional health, table CRUD and transactional batch REST routes | Same routes and SQL generation through the source's actual worker |
| File bootstrap and caller-controlled writer ordering | `file-bootstrap` remains available for memory-backed queries |
| Coordinated persistent initial loading, replay and consumer ownership | Paired single/shared factories reuse the existing Rust snapshot/journal engines |
| Stop, cancellation, bounded admission and restart | Actual workers and generation-bound clients stay owned until cleanup |
| SQLite reaction | No legacy SQLite reaction exists; none is invented |

Legacy Source-plugin dispatch/start/bootstrap settings map to graph lifecycle,
bounded pipes, progress and query-bootstrap resources rather than an old execution
bridge. Original legacy builders and Rust handles remain available. Native
configuration, typed element-ID format and explicit safety bounds are not a
promise of drop-in legacy configuration or persisted-state compatibility.

## Qualification

```sh
cargo +1.95.0 build --offline --locked --lib \
  -p drasi-computation-source-sqlite -p drasi-computation-bootstrap-sqlite \
  --features dynamic-plugin
cargo +1.95.0 test --offline --locked -p drasi-host-sdk \
  --test native_sqlite_components -- --test-threads=1
```

The Host suite exercises static and actual dynamic libraries on both Tokio
runtimes, including two-query file snapshots/REST changes, whole-transaction
query visibility, rollback, persistent restart, and cancelled/timed-out REST
cleanup. Paired cases cover actual query initialization, binding identity,
partial/failed loading and explicit recovery, retained stopped providers, and
member reconstruction. Server's `native_sqlite_test` drives actual HTTP result
routes, public schema inspection and reconstruction from canonical metadata-derived
file/single/shared configurations.

Cold reconstruction with undrained persistent input-pipe frames is not a
qualified guarantee. Source replay, persistent query state and a successful
drained restart do not supply missing queued-frame allocation state.
