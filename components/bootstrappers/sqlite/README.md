# SQLite Bootstrap Provider

## Overview

`drasi-bootstrap-sqlite` provides initial snapshot loading for SQLite-backed
sources. The `native` module binds the independent
[`drasi-computation-bootstrap-sqlite`](../../computation-plugins/bootstrappers/sqlite)
implementation directly to a Rust ComputationGraph host for ordinary file
snapshots, including queries with in-memory storage. It uses the
same schema, type and element-identity mapping as the native SQLite source.
The older Source-plugin callback is still present for consumers awaiting
replacement; it is not the native implementation.

Key capabilities:

- file-backed SQLite snapshot loading
- optional table allow-list
- table key override support for stable element IDs
- automatic primary-key detection and physical-rowid fallback

## Native file snapshots

```rust,ignore
use drasi_bootstrap_sqlite::native::{
    SqliteBootstrapConfig, SqliteBootstrapProvider,
};
use drasi_lib::{computation::v1::*, Query};
use std::sync::Arc;

let source = ComponentId::try_new("sqlite")?;
let output = Endpoint::new(source.clone(), PortId::try_new("out")?);
let bootstrap = SqliteBootstrapProvider::new(
    source,
    StreamId::try_new("sqlite/out")?,
    SqliteBootstrapConfig {
        path: "data/example.db".into(),
        tables: vec!["sensors".into()],
        table_keys: Vec::new(),
        max_tables: 64.try_into()?,
        max_bytes: (1 << 20).try_into()?,
        max_duration_ms: 30_000.try_into()?,
        output_capacity: 64.try_into()?,
        shutdown_timeout_ms: 5_000.try_into()?,
    },
)?;
let queries = drasi.computation_pipeline()?
    .native_source(output)?
    .query_bootstrap("temperatures", Arc::new(bootstrap))?
    .query(Query::cypher("temperatures")
        .query("MATCH (s:sensors) RETURN s.name AS name,s.temp AS temperature")
        .from_input("sqlite")
        .build())
    .build()?;
drasi.add_components(queries).await?;
```

The source ID, stream, tables and key overrides must match the live native
source. Create one provider per query. Labels are table names; select tables
explicitly to restrict the initial data.

**This is not a coordinated snapshot/live handover.** Keep live writers,
including REST clients, quiescent until the query has finished startup. A single
read-only SQLite transaction gives a consistent view across tables even if a
WAL writer commits during the read, but it does not supply a CDC/replay boundary
for those concurrent writes. Use
[`NativeSqliteSource::coordinated`](../../sources/sqlite/README.md#coordinated-native-initial-loading)
when initialization must gate native SQL admission and survive process restart
with a real source journal and persistent atomic query storage.

| Native configuration | Meaning |
|----------------------|---------|
| `path` | Required existing SQLite file. Missing files are not created; memory aliases are rejected. |
| `tables` | Empty selects ordinary application tables; explicit missing tables fail. Internal `__drasi_` journal tables are excluded. |
| `table_keys` | Optional non-null key-column overrides. Otherwise use the primary key or physical rowid. Names use the actual schema's column casing. |
| `max_tables` | Positive bound on selected table metadata, not the number of rows. |
| `max_bytes` | Positive bound on schema metadata and each encoded row independently. |
| `max_duration_ms` | Positive work deadline, including bounded output backpressure and SQLite work. |
| `output_capacity` | Positive bounded row-channel capacity. No whole-snapshot collection. |
| `shutdown_timeout_ms` | Positive cleanup timeout. An unfinished blocking worker stays owned and prevents another snapshot. |

Native IDs preserve SQLite key types and delimiters: BLOB and TEXT keys cannot
collide. Tables without a primary key use an unshadowed physical-rowid alias,
not an application column named `rowid`; shadowing every alias requires explicit
keys. Source capture, this provider and the coordinated snapshot all use
`drasi-sqlite-common`. Invalid schema, null keys, byte limits and worker failures
remain errors, not successful empty snapshots.

Dropping a stream requests cancellation; the query awaits `stop` for actual
cleanup. Cancelled/timed-out cleanup retains the worker. Retired streams cannot
read from, cancel or join a replacement. This provider returns no source
watermark or persisted handover state, and never fabricates a sequence-zero
checkpoint.

The independent native library exports `drasi.sqlite/file-bootstrap` through
the native bootstrap extension, with no dummy processing component. This
crate's own `dynamic-plugin` export still belongs to the older callback family;
build/load the independent native library for the native ABI. The direct Rust
binding delegates to the same implementation without old-event conversion.

## Remaining Source-plugin usage

```rust
use drasi_bootstrap_sqlite::{SqliteBootstrapProvider, TableKeyConfig};

let bootstrap = SqliteBootstrapProvider::builder()
    .with_path("data/example.db")
    .with_tables(vec!["sensors".to_string()])
    .with_table_keys(vec![TableKeyConfig {
        table: "sensors".to_string(),
        key_columns: vec!["id".to_string()],
    }])
    .build();
```

Attach the provider in `SqliteSource::builder(...).with_bootstrap_provider(bootstrap)`.

### Source-plugin configuration

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `path` | `String?` | `null` | SQLite file path. Omit for in-memory (bootstrap will be skipped) |
| `tables` | `Vec<String>?` | `null` | Optional table allow-list. `null` means all user tables |
| `tableKeys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration |

### Source-plugin TableKeyConfig

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name |
| `keyColumns` | `Vec<String>` | Column names to use as primary key |

### Source-plugin behavior

- For file-backed databases, the provider opens a read-only connection and streams table rows.
- For in-memory databases, bootstrap is skipped (no independent snapshot connection exists).
- Labels map directly to table names.
- Element IDs use configured keys first, then detected PK columns.

## Testing

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

The native provider is also exercised with two ordinary hosted queries sharing
one native source in `lib-integration-tests/tests/native_sqlite_host.rs`, on both
Tokio runtime flavors. Those cases cover primary/composite/custom keys, physical
rowids, empty input, live rekeys and deletions without persistent query storage.
