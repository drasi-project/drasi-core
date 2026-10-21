# Native SQLite bootstrappers

Independent native bootstrap library, implementation/configuration version `1`.
It exports no processing component, dummy service or Source-plugin callback.

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
Use a matching host with bootstrap-only catalogue support; an older host can
reject the empty processing-factory list even when it understands ABI 1.0.

## File snapshots

`drasi.sqlite/file-bootstrap` configuration:

```json
{
  "source": "sqlite",
  "stream": "sqlite/out",
  "settings": {
    "path": "data/example.db",
    "tables": ["sensors"],
    "table_keys": [],
    "max_tables": 64,
    "max_bytes": 1048576,
    "max_duration_ms": 30000,
    "output_capacity": 64,
    "shutdown_timeout_ms": 5000
  }
}
```

Construction validates configuration only; it does not open the file or start
workers. The factory needs the query's explicit instance/graph scope. It does not
bind a source-progress reader.

The provider opens an existing file read-only, streams one consistent SQLite
read transaction, and uses `drasi-sqlite-common` for the same typed keys and
physical-rowid mapping as native live capture. All limits are positive;
`max_bytes` bounds schema metadata and each row independently, while
`output_capacity` provides backpressure rather than whole-snapshot collection.
Dropping a stream requests cancellation. Awaited stop retains unfinished workers
after timeout/cancellation and fences stale stream generations.

**Keep live writers, including REST clients, quiescent until query startup
finishes.** This ordinary file provider supplies no source watermark or durable
handover state. Use the source's coordinated persistent snapshot when that
stronger initialization/recovery contract is needed; it is not provided by the
file factory.

Rust callers can use `SqliteBootstrapProvider` with the SDK's
`NativeBootstrapProvider` interface. The existing host-linked
`drasi_bootstrap_sqlite::native` binding delegates to this implementation without
Source-plugin event conversion. Its
[usage and bounds](../../../bootstrappers/sqlite/README.md#native-file-snapshots)
apply to both paths.

The production dependency graph contains no query evaluator, parser, graph
runtime or legacy SDK. Server's existing `nativeBootstrap` resource recipe
accepts this factory with `component` naming its query and the configuration
above; omit `sourceProgress`. Native catalogue discovery, YAML/configuration
round-trips and memory-backed query loading are covered by the Server native
integration suite. The independently packaged
[`drasi-computation-source-sqlite`](../../sources/sqlite) can supply live changes
and REST writes to the same graph after file initialization. The two-query
Server example is `drasi-server/examples/native-sqlite.rs`, using its `file` mode.
This does not turn file initialization into coordinated journal ownership:
writers must remain quiescent until both queries finish loading.

## Source-paired snapshots

Use `drasi.sqlite/coordinated-bootstrap` with
`drasi.sqlite/replay-transactions`, or `drasi.sqlite/shared-bootstrap` with
`drasi.sqlite/shared-transactions`. Set `coordinated_snapshot: true` in the
source configuration. Paired factory configuration is deliberately only:

```json
{"source": "sqlite", "stream": "sqlite/out"}
```

A graph-owned `nativeSourceBootstrap` resource captures the actual source factory,
immutable settings and single owner or complete consumer group. Each paired
`nativeBootstrap` resource declares `sourceBootstrap` and its query's
`sourceProgress`, with matching `Bootstrap` and `Checkpoint` dependencies.
The source and query also retain their normal progress bindings. Actual owner
identity is validated; names, matching paths or independently rebuilt resources
cannot replace it.

The source library owns the coordinator and creates the provider handle. This
bootstrap library validates source identity, mode, configuration and scope,
then opens that handle. It neither duplicates the engine nor starts a forwarding
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

See the [source binding and Server examples](../../sources/sqlite/README.md#rust-handles-and-internal-coordination)
for `single`/`shared` configurations and the
[existing Rust initialization contract](../../../sources/sqlite/README.md#coordinated-native-initial-loading)
for recovery limits. There is no public DrasiLib SQL API, SQL service across the
ABI, or cold undrained-persistent-pipe reconstruction guarantee.
