# Native SQLite bootstrappers

Independent native bootstrap library (ComputationGraph ABI 5.0.0),
implementation/configuration version `1`. It exports no processing component,
dummy service or Source-plugin callback. Configuration is flat camelCase and
uses the same field names (`path`, `tables`, `tableKeys`) as the
[native SQLite source](../../sources/sqlite). Every factory declares its
bootstrap output, which the graph checks when the provider's edge binds; see
the [native bootstrap provider standard](../NATIVE-BOOTSTRAP-PROVIDER-GUIDE.md).

| Factory | Initialization | Progress binding |
|---|---|---|
| `drasi.sqlite/file-bootstrap` | Independent read-only file snapshot | None |
| `drasi.sqlite/coordinated-bootstrap` | Source-owned persistent single-query snapshot | Actual query plus source coordinator |
| `drasi.sqlite/shared-bootstrap` | Source-owned persistent member snapshot | Actual member plus shared source coordinator |

```sh
cargo build -p drasi-computation-bootstrap-sqlite --features dynamic-plugin
```

Load `libdrasi_computation_bootstrap_sqlite.dylib` on macOS,
`libdrasi_computation_bootstrap_sqlite.so` on Linux, or
`drasi_computation_bootstrap_sqlite.dll` on Windows. The native bootstrap
extension supplies its query-scoped factory. The component-factory list is
intentionally empty; hosts must validate the bootstrap catalogue as well.
Use an ABI 5.0.0 host with bootstrap-only catalogue support; an empty
processing-factory list is not an empty bootstrap catalogue.

## File snapshots

`drasi.sqlite/file-bootstrap` configuration:

```json
{"path": "data/example.db", "tables": ["sensors"]}
```

| Field | Type | Default | Notes |
|---|---|---|---|
| `path` | string, required | — | existing file, opened read-only |
| `tables` | array | `[]` (all) | must equal the served source's selection; an upper bound on what a query's labels select |
| `tableKeys` | array of `{"table", "keyColumns"}` | `[]` | must equal the served source's keys |
| `maxTables` | integer ≥ 1 | `1024` | |
| `maxBytes` | integer 1..=2147483647 | `16777216` | bounds schema metadata and each row |
| `maxDurationMs` | integer 1..=2147483647 | `300000` | whole snapshot, and the busy timeout |
| `outputCapacity` | integer ≥ 1 | `16` | rows buffered ahead of the query |
| `shutdownTimeoutMs` | integer ≥ 1 | `10000` | worker stop bound |

The previous nested `settings` object (snake_case) and the former `source` and
`stream` fields are rejected.

The provider declares a quiescent completion in its served source's namespace
and its element identities (the selected tables and keys). The served source
comes from the graph relation: the SQLite source whose `bootstrap` input the
provider is connected to, or, as a query's local provider, the single source
the query reads. Its rows use that source's element IDs and output stream. The
graph refuses the edge when the served source declares other identities (a
different table selection, or keys versus rowids), so a snapshot can no longer
silently produce rowid identities beside keyed live updates. Each query reads
only the configured tables whose label (the table name) it declares.

Construction validates configuration only; it does not open the file or start
workers. The factory needs the query's explicit instance/graph scope. It does not
bind a source-progress reader.

The provider opens an existing file read-only, streams one consistent SQLite
read transaction, and uses `drasi-sqlite-common` for the same typed keys and
physical-rowid mapping as native live capture. All limits are positive;
`maxBytes` bounds schema metadata and each row independently, while
`outputCapacity` provides backpressure rather than whole-snapshot collection.
Dropping a stream requests cancellation. Awaited stop retains unfinished workers
after timeout/cancellation and fences stale stream generations.

**Keep live writers, including REST clients, quiescent until query startup
finishes.** This ordinary file provider supplies no source watermark or durable
handover state. Use the source's coordinated persistent snapshot when that
stronger initialization/recovery contract is needed; it is not provided by the
file factory.

Rust callers can use `SqliteBootstrapProvider` with the SDK's
`QuiescentBootstrapProvider` interface. The existing host-linked
`drasi_bootstrap_sqlite::native` binding delegates to this implementation without
Source-plugin event conversion. Its
[usage and bounds](../../../bootstrappers/sqlite/README.md#native-file-snapshots)
apply to both paths.

The production dependency graph contains no query evaluator, parser, graph
runtime or legacy SDK. The Host publishes this factory as a bootstrap provider
graph node. Connect its `snapshot` output to a source's `bootstrap` input, or to
a query's `bootstrap` input as the local provider of a query that reads one
SQLite source; it needs no `source_progress` dependency. Native catalogue discovery, YAML/configuration
round-trips and memory-backed query loading are covered by the Server native
integration suite. The independently packaged
[`drasi-computation-source-sqlite`](../../sources/sqlite) can supply live changes
and REST writes to the same graph after file initialization. The two-query
Server example is `drasi-server/examples/native-sqlite.rs`, using its `file` mode.
This does not turn file initialization into coordinated journal ownership:
writers must remain quiescent until both queries finish loading.

A [`drasi.query/indexed`](../../transformers/query/README.md#durable-provider-bootstrap)
query configured with `bootstrap: true`, or the standard
`drasi.standard/middleware` transformer (which accepts only quiescent
providers), can bootstrap from the same provider node connected to its
`bootstrap` input. The consumer drives the provider's real
prepare/stream/complete/stop lifecycle over the bootstrap edge; writers must
remain quiescent until initialization completes.

## Source-paired snapshots

Use `drasi.sqlite/coordinated-bootstrap` with
`drasi.sqlite/replay-transactions`, or `drasi.sqlite/shared-bootstrap` with
`drasi.sqlite/shared-transactions`. Set `coordinatedSnapshot: true` in the
source configuration. Paired factories take no configuration (`{}`): the
source owns its path, tables, keys and stream.

The source declares its coordination protocol
(`drasi.sqlite/coordinated-snapshot` or `drasi.sqlite/shared-snapshot`) and
requires it; the paired factory declares the same protocol. The graph refuses
the provider's edge to any other source, and refuses an unpaired provider on a
coordinated source, when the edge binds. Connected to the source's `bootstrap`
input, the paired provider node uses the coordinator the source created from
its actual factory, immutable settings and single owner or complete consumer
group. Each query snapshots only the captured tables whose label it declares. The node binds
each query's actual progress under `source_progress`.
Each query bootstraps through the source's `snapshot` output.
The source and query also retain their normal progress bindings. Actual owner
identity is validated; names, matching paths or independently rebuilt resources
cannot replace it.

The source library owns the coordinator and creates the provider handle. This
bootstrap library checks the typed pairing (source, protocol, query, labels),
then opens that handle; it never reads the source's configuration or plugin
identity. It neither duplicates the engine nor starts a forwarding
worker. Existing bounded snapshot queues, journal positions, query-owned intent
and atomic completion state are reused. Live admission waits for all required
snapshots; snapshots and live work cannot overlap.

Stopped providers may be superseded while an old handle is retained, but the
retired handle cannot affect its replacement. The SDK owns export-instance leases
and validates a replacement before retiring the old export; family construction
does not retire it early. Cancelled or failed cleanup retains actual worker
ownership. Resource retirement is permanent. Incomplete loading
requires explicit recovery; completed/missing state and unsafe membership changes
are not silently repaired.

The source's `initSql` also runs on the snapshot connection, so tables it
creates exist before the snapshot reads them.

See the [source binding](../../sources/sqlite/README.md#host-bindings)
for `single`/`shared` configurations and the
[existing Rust initialization contract](../../../sources/sqlite/README.md#coordinated-native-initial-loading)
for recovery limits. There is no public DrasiLib SQL API, SQL service across the
ABI, or cold undrained-persistent-pipe reconstruction guarantee.
