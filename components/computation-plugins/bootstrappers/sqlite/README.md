# Native SQLite file bootstrap

Independent native bootstrap library exposing `drasi.sqlite/file-bootstrap`,
implementation version `1`, configuration version `1`. It exports no processing
component, dummy service or Source-plugin callback.

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

Factory configuration:

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
stronger initialization/recovery contract is needed; it is not provided by this
library.

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
integration suite. The SQLite live source and coordinated snapshot are still
host-linked; building this library alone does not complete their native-ABI or
Server migration.
