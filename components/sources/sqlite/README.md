# SQLite Source

This crate contains two separate implementations. `SqliteSource` is the existing
legacy Source plugin described below. `NativeSqliteSource` (also
`native::SqliteSource`) directly implements ComputationGraph's component and
envelope-source interfaces. Its API and guarantees are described in
[Native ComputationGraph component](#native-computationgraph-component).
The native implementation does not change the legacy SQL API, event format or ABI.

## Overview

`drasi-source-sqlite` is a protocol/local source that owns an embedded SQLite connection and emits Drasi `SourceChange` events for row-level `INSERT`, `UPDATE`, and `DELETE` activity.

Key capabilities:

- Real-time CDC using `rusqlite` `preupdate_hook`
- Transaction-aware dispatch (`commit_hook` flush, `rollback_hook` discard)
- Savepoint-aware buffering for partial rollback support
- Optional table filtering and explicit table key configuration
- Optional REST API for table CRUD and transactional batch operations
- Works with file-backed and in-memory SQLite databases

## Quick Start

```rust
use drasi_source_sqlite::{SqliteSource, TableKeyConfig, RestApiConfig};

let source = SqliteSource::builder("sqlite-source")
    .with_path("data/example.db")
    .with_table_keys(vec![TableKeyConfig {
        table: "sensors".to_string(),
        key_columns: vec!["id".to_string()],
    }])
    .with_rest_api(RestApiConfig {
        host: "127.0.0.1".to_string(),
        port: 9100,
    })
    .build()?;
```

## SQL Handle API

Use `source.handle()` to execute statements through the source-owned SQLite connection:

```rust
let handle = source.handle();
handle.execute("CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT, temp REAL)").await?;
handle.execute("INSERT INTO sensors(id, name, temp) VALUES (1, 'sensor-a', 31.5)").await?;

handle.transaction(|tx| async move {
    tx.execute("INSERT INTO sensors(id, name, temp) VALUES (2, 'sensor-b', 30.0)").await?;
    tx.execute("UPDATE sensors SET temp = 32.1 WHERE id = 1").await?;
    Ok(())
}).await?;
```

## Native ComputationGraph Component

Create `NativeSqliteSource::new(id, stream, settings)` for volatile delivery, or
`NativeSqliteSource::new_replayable(id, stream, settings, progress)` for durable
transaction replay. Use `NativeSqliteSource::coordinated` for initial loading
of an existing database before live writes. `progress` must be the **actual** `Arc<QuerySourceProgress>`
of the one persistent atomic query consuming this source. The graph validates
that ownership; another query's resource or a same-named substitute is not enough.

The replay/paired constructors also accept a `SourceProgressReader` for a
read-only host boundary. Local readers retain the actual ownership check;
external readers do not claim local ownership. Snapshot pairing requires the
same reader handle, and revoked reads stop processing without retiring retained
input. This Rust interface does not yet expose SQLite through the native plugin
ABI; see [W7](../../../lib/docs/computation-graph-reliability-plan.md#w7-preserve-errors-and-expose-shared-services-through-plugins).

| Settings | Output and guarantee |
|----------|----------------------|
| `output: Changes`, `replay: None` | Ordinary `drasi.graph-change` batches, without a source journal, checkpoint feedback or whole-transaction query guarantee. This is the fast path. |
| `output: Transactions`, `replay: None` | One complete `drasi.source-transaction` record per committed SQLite transaction. No restart replay. |
| `output: Transactions`, `replay: Some(...)` | SQLite data and the bounded replay record commit in the same SQLite transaction. Records remain until the bound query commits its input progress. Requires a real file-backed database and persistent query storage. |

Transaction output requires the query's `source_transactions` execution setting,
compatible atomic indexes and atomic publication. Its frame is never split into
independently visible rows. A pipe's envelope limit must also accommodate the
complete transaction. Local SQLite commit is not a distributed transaction with
query storage, pipes or an external reaction.

### Native SQL API and ownership

Call `source.handle()` before moving the component into the graph. The cloneable
handle provides `execute`, `execute_parameterized`, `execute_batch`, `query`,
`query_parameterized` and `transaction`. It becomes usable after source startup.
The transaction callback receives a cloneable `SqliteTransactionHandle` with the
same SQL operations, but cannot start another transaction.

- One source-owned blocking worker owns the connection. Each transaction has a
  private bounded command queue, so other handles cannot execute inside it.
  Within a transaction, use the supplied transaction handle, not the ordinary
  handle: ordinary operations wait for the transaction to finish.
- A script passed to `execute_batch` is one atomic transaction outside a callback
  and part of its enclosing transaction inside one. There is no committed prefix
  when a later statement fails.
- **Any failed SQL statement aborts the whole scope**, including errors caught
  by the callback, failed parameter validation, capture limits and failed commit.
  SQLite's partial `OR FAIL` changes are rolled back. A failed cleanup or uncertain
  commit fences the worker instead of admitting unrelated work. The callback
  future is cancelled if its database scope aborts or expires while it is waiting.
- Raw `BEGIN`, `COMMIT`, `END` and full `ROLLBACK` are rejected by SQLite's
  authorizer, not a string-prefix parser. Savepoints are allowed only through
  transaction SQL operations; quoted names, nested release and rollback restore
  captured changes and their size accounting.
- Query methods are read-only; mutating `RETURNING` statements and savepoint
  control are rejected. SQL/parameter admission and query result storage are
  bounded. Duplicate result column names are rejected; use explicit aliases.
- Dropping a transaction future reserves and submits rollback without spawning
  a cleanup task. Escaped handles cannot write after their scope ends. Cancellation
  **after commit submission has an unknown outcome to the caller**; it is not
  permission to retry the write. This API does not supply caller retry receipts.
- Stop closes new admission, requests rollback of unfinished scopes and awaits
  the actual database worker. An already-submitted commit may still complete.
  Timeout/cancellation retains the worker and blocks restart until a subsequent
  stop finishes cleanup. Old scoped handles cannot target a replacement worker.
  Volatile output may be lost at stop; durable output remains in the journal.

### Native settings

All bounds are explicit positive values; the native API does not choose hidden
transaction-size defaults.

| `SqliteConfig` field | Meaning |
|----------------------|---------|
| `path` | `None` for memory, or a SQLite path. Durable mode verifies that SQLite actually opened a persistent file, rejecting memory/temporary database aliases. |
| `tables` | Selected main-database table names; empty selects all ordinary tables. |
| `output` | `SqliteOutput::Changes` or `SqliteOutput::Transactions`. |
| `replay` | `None`, or `SqliteReplayConfig { max_transactions, max_bytes }` bounding retained transaction records. A full journal rejects a new write and rolls it back. |
| `transactions` | `SourceTransactionLimits { max_changes, max_bytes, max_duration_ms }`. These bound captured groups; row/byte bounds also bound read results and schema metadata. Savepoint count is bounded by `max_changes`. |
| `max_sql_bytes` | Combined SQL and parameter admission limit, also applied to SQLite's SQL parser. |
| `command_capacity` | Bounded ordinary and per-transaction command lanes. At least two, including the reserved rollback slot. |
| `output_capacity` | Bounded output queue. A full queue backpressures further SQL admission. |
| `shutdown_timeout_ms` | Awaited cleanup bound. SQLite lock waits are bounded by the transaction duration, so a shorter cleanup timeout can require another stop. |

Oversized or timed-out transactions are rolled back without successful
acknowledgement. Increase limits and retry only after a **confirmed rollback**.
Lower replay limits cannot discard retained data. Execution deadlines also cover
idle transaction callbacks and long-running SQLite virtual-machine work.

### Coordinated native initial loading

`NativeSqliteSource::coordinated(id, stream, settings, progress)` returns the source
and an `Arc<SqliteSnapshot>`. Attach that provider to the **same** query with
`with_bootstrap(snapshot)`. This mode requires durable complete-transaction
settings and persistent atomic query storage.

Before touching the source database, the query durably records the initialization
identity. A query-owned blocking worker then creates/binds the SQLite journal and
streams a consistent initial read through a bounded queue. Snapshot rows use the
same primary-key encoding and value conversion as live changes. The number of
rows can exceed one transaction's `max_changes`; each row remains bounded by
`max_bytes`, and the entire read, including backpressure, has the configured
transaction deadline. SQLite lock acquisition has the same bounded busy timeout.

The query commits the completed initialization record together with its source
watermark. Zero is a real position in the new, identified journal, not an invented
cursor. Only afterward can the source start accepting SQL writes. This avoids a
snapshot/live gap by delaying native SQL admission, rather than accepting writes
in the background during initialization. External writers are not supported.

Interrupted loading remains incomplete under strict recovery. After correcting
limits, explicit `AutoReset` can restart only that unfinished initialization,
preserving source rows and journal identity. Completed initialization is reused,
not replaced. Missing history/identity, changed bindings, or losing a completed
query's state requires explicit recovery; initialization modes cannot be switched
on an existing journal. Query deprovisioning does not retire the source database
or authorize silently rebuilding a completed initialization.

Dropping the snapshot stream cancels its work. Query stop/deprovision awaits the
actual worker; timeout or cancellation retains ownership and its client lease
until cleanup finishes. Old and replacement sources cannot share an unfinished
client owner. Retained snapshot streams are tied to their original worker; polling
or dropping an old stream cannot affect a replacement.

### Native recovery and limitations

Durable mode owns reserved `__drasi_` tables in the same file as the application
data. They store the source/stream/consumer/table binding, journal identity,
schema, retained records and retirement progress. Application SQL cannot access
or modify them. The component holds an exclusive SQLite file lock and uses
SQLite's full synchronization setting. Its qualified durability declaration is
**process-restart recovery**, not a power-loss or network-filesystem guarantee.

Restart validates the binding, schema, journal accounting and actual committed
consumer cursor. Missing history, damaged records, a different consumer, an
out-of-band schema change, or disabling replay on a journal-owned database fails
explicitly. Replay retains logical transaction identity and timestamps while
advancing transport sequence numbers. Numeric bounds are not part of durable
binding identity.

Only writes through the native handle are captured. Do not modify its database
externally between runs. Replay-only construction may adopt empty tables, but
rejects pre-existing rows; use coordinated initialization to load them. Neither
mode silently marks existing data as consumed.

Captured rows require non-null database primary keys. Native IDs use a versioned
encoding of table and sorted, typed key values, avoiding delimiter and BLOB/text
collisions. A key-changing update emits deletion of the old identity followed by
insertion of the new one in the same transaction. IDs intentionally differ from
legacy SQLite IDs. Ordinary tables, including composite keys and `WITHOUT ROWID`,
are supported; generated columns and virtual tables are rejected. DDL is not
emitted as data. `ALTER TABLE`, `DROP TABLE`, temporary objects, database attachment
and application PRAGMAs are currently rejected rather than silently
invalidating capture. Application indexes, triggers and foreign keys cannot
target the reserved replay tables.

The Rust factory is `SqliteSourceFactory`, implementation `drasi/sqlite-native`,
version `1`. Configure `stream` and the `settings` object. Supply a borrowed
`SqliteClientResource` under the `client` dependency (`ResourceRole::Component`);
its `handle()` is the application entry point. Construct that resource with the
same component ID and settings. Its exclusive owner lease survives unfinished
worker cleanup. Replay also requires `source_progress`
(`QuerySourceProgressResource`, `ResourceRole::Checkpoint`).
For coordinated loading, set `coordinated_snapshot: true` and also supply the
paired `SqliteSnapshot` under `snapshot` (`ResourceRole::Bootstrap`). The query
must use that same provider. Resolved source settings and actual progress ownership
are checked, not just resource names.

Native REST endpoints, Server registration and a
separately exported native plugin ABI are not implemented. The existing REST
server and dynamic-plugin export still serve only the legacy implementation.

## REST API (Optional)

When `with_rest_api()` is configured:

- `GET /health`
- `GET /api/tables`
- `GET /api/tables/{table}`
- `GET /api/tables/{table}/{id}`
- `POST /api/tables/{table}`
- `PUT /api/tables/{table}/{id}`
- `DELETE /api/tables/{table}/{id}`
- `POST /api/batch` (single SQLite transaction for all operations)

## Configuration Options

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `path` | `String?` | `null` | SQLite file path. Omit for in-memory database |
| `tables` | `Vec<String>?` | `null` | Optional table allow-list. `null` means all user tables |
| `tableKeys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration (see below) |
| `restApi` | `RestApiConfig?` | `null` | Optional REST API configuration |

### TableKeyConfig

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name |
| `keyColumns` | `Vec<String>` | Column names to use as primary key |

Configured table keys override automatically detected primary keys from `PRAGMA table_info`. If no configured or detected keys exist, `rowid` is used as the fallback for streaming events.

### RestApiConfig

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | `String` | `"0.0.0.0"` | Address to bind |
| `port` | `u16` | `8080` | Port to bind |

## Stop and Restart

Start registers the database worker before submitting it to Tokio's blocking
pool, waits for the connection/hooks to initialize, and binds any REST listener
before reporting Running. Startup errors retain their SQLite/I/O cause and any
remaining workers until cleanup. `bound_rest_address()` reports the actual
listener address, including an operating-system-selected port when configured
with port zero.

Stop closes public command admission and new HTTP work, then joins existing REST
requests, the database worker and queued change dispatch, in that order. Each
worker join has a five-second bound without aborting its in-flight work. Timeout
or cancellation retains ownership and blocks restart; call stop again to finish
cleanup. Completed cleanup clears old channel subscribers and position filters.
Healthy duplicate starts and repeated stops remain harmless.

REST work and transaction handles are bound to the database worker that accepted
them. A transaction continuation from an earlier run cannot write to a
replacement connection. The ordinary source handle remains reusable after a
successful restart.

## Testing

From this crate directory:

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

The default integration tests validate:

- handle-driven INSERT/UPDATE/DELETE flow
- REST CRUD and transactional batch behavior
- bootstrap change delivery
- multi-table query routing

The current-thread lifecycle cases cover typed startup failures, cancelled
startup/cleanup, a panicked worker, a real write held by another SQLite
transaction, a partially submitted HTTP body, late pipelined requests and three
database/REST restart cycles. The HTTP case waits for `100 Continue` before
stopping, proving that a real request is awaiting its body. Timeout cases check
the actual five-second threshold, then complete and verify the accepted work.

## Limitations

- CDC only captures writes executed through the source-owned connection.
- DDL (`CREATE/ALTER/DROP TABLE`) is not emitted as `SourceChange`.
- BLOB values are emitted as base64-encoded strings.
- These lifecycle guarantees do not provide durable event replay or
  whole-transaction visibility in downstream queries.
- The existing transaction helpers are not isolated concurrent sessions.
  Callers must serialize transaction use; cancellation/escaped transaction
  handles still need stronger scoped-transaction handling.
- The current commit hook publishes buffered changes before SQLite has finished
  committing. Failed-commit publication remains a transaction-boundary gap, not
  a guarantee supplied by worker ownership.

## Troubleshooting

- `source ... is not running`: call `core.start()` before using `SqliteSourceHandle`.
- no events after writes: verify writes go through the source handle or REST API, not an external SQLite connection.
- REST 400 responses: table/column identifiers failed validation.
