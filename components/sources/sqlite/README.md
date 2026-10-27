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
transaction replay. Use `NativeSqliteSource::coordinated` for persistent,
coordinated initial loading before live writes. Ordinary file snapshots for
in-memory queries use the [standalone native bootstrap provider](../../bootstrappers/sqlite/README.md#native-file-snapshots)
with caller-controlled initialization/write ordering. For these single-owner
constructors, `progress` must be the **actual** `Arc<QuerySourceProgress>`
of the one persistent atomic query consuming this source. The graph validates
that ownership; another query's resource or a same-named substitute is not enough.

For multiple persistent consumers, use `new_replayable_with_consumers` or
`coordinated_with_consumers` with the actual `Arc<ConsumerProgressSet>`. See
[Shared native recovery](#shared-native-recovery) for initialization, membership
and factory bindings. Neither API creates a source or journal for each query.

The replay/paired constructors also accept a `SourceProgressReader` for a
read-only host boundary. Local readers retain the actual ownership check;
external readers do not claim local ownership. Snapshot pairing requires the
same reader handle, and revoked reads stop processing without retiring retained
input. The [independent native source library](../../computation-plugins/sources/sqlite)
exports fixed-schema source factories, reusing this implementation without a
Source-plugin adapter. The standalone file bootstrap has its own independently
built native library. The same bootstrap library also exports paired single/shared
factories using the source-owned graph resource. Their internal coordination is
separate from file initialization; it is not a public SQL service on DrasiLib or
across the ABI.

| Settings | Output and guarantee |
|----------|----------------------|
| `output: Changes`, `replay: None` | Ordinary `drasi.graph-change` batches, without a source journal, checkpoint feedback or whole-transaction query guarantee. This is the fast path. |
| `output: Transactions`, `replay: None` | One standard graph-change envelope per committed SQLite transaction, carrying every operation in order plus a current complete-transaction marker. No restart replay. |
| `output: Transactions`, `replay: Some(...)` | SQLite data and the bounded replay record commit in the same SQLite transaction. Records remain until every required consumer commits its input progress. Requires a real file-backed database and persistent query storage. |

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

`handle.describe_schema().await?` retrieves the actual worker's bounded
`SourceSchema`, including captured columns and element-key descriptions. It is
available only while that SQL client is active, remains usable while source
`next()` is pending, and cannot make a retired generation follow a replacement.
SQLite integer/text/real affinities are hints; numeric, untyped and BLOB columns
do not falsely advertise one fixed graph property type.

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
| `table_keys` | Optional `TableKeyConfig` entries selecting non-null element-key columns; otherwise use the primary key, or physical rowid when no primary key exists. Capture, standalone native file snapshots and coordinated snapshots use the same identity. A key change emits deletion of the old identity and insertion of the new one. |
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

Explicit key columns are validated against the actual table schema. Empty,
duplicate, missing or null keys fail rather than falling back to another
identity. Tables without a declared primary key use their physical rowid unless
overridden. If every rowid alias is shadowed by user columns, explicit keys are
required rather than inventing a snapshot identity. Replay and snapshot bindings
include custom key declarations, so changing them cannot silently reinterpret
retained input or existing query rows. Omitting overrides preserves the existing
current-version primary-key identity and binding.

The shared schema and row mapper live in `drasi-sqlite-common`, with no query
runtime or plugin SDK dependency. The source and both native snapshot paths use
that implementation rather than keeping separate ID/type conversions.

### Native REST API

The native source reuses the existing REST routes and SQL generation:

```rust,ignore
let source = NativeSqliteSource::new(id, stream, settings)?
    .with_rest_api(RestApiConfig {
        host: "127.0.0.1".into(),
        port: 0,
    })?;
```

After startup, `bound_rest_address()` reports the listener address, including the
assigned port when configured with port zero. The native factory accepts the
same optional `rest_api` object beside `stream` and `settings`. Transport settings
do not change source replay identity.

Table listing/filtering, row CRUD and transactional batches use the source's one
native SQL worker. Key lookup uses its actual captured schema (including explicit
keys and rowid fallback), not application PRAGMA statements. Internal journal
tables are not exposed by REST. Route IDs remain key values, separated by `:` for
composite keys, rather than the native graph's full versioned element references.

Stop rejects new HTTP/ordinary SQL admission, lets already accepted HTTP requests
finish through their generation-bound SQL client, and then stops the database.
Cancelled/timed-out cleanup retains both worker ownership and the client lease;
restart remains blocked until cleanup finishes. An old REST client cannot follow
the shared client resource into a replacement worker. Bind/startup and unexpected
REST-worker failures remain explicit. Successful HTTP writes confirm local SQL
completion, not downstream query processing or external effects.

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

Captured rows use non-null configured/database keys or physical rowids. Native
key IDs use a versioned encoding of table and sorted, typed key values, avoiding
delimiter and BLOB/text collisions. Rowid IDs use a distinct `sqlite:rowid:v1`
encoding, and snapshots select an unshadowed rowid alias rather than a same-named
user column. A key-changing update emits deletion of the old identity followed by
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

The native Rust REST endpoints described above use the same shared routes as
the legacy source, through the native SQL worker. This crate's `dynamic-plugin`
export still serves only the legacy implementation. SQL handles stay in the
plugin-specific Rust API; no public SQL-access service is exposed through the
native ABI. Build `drasi-computation-source-sqlite` for the native ABI; ordinary
Server file initialization and persistent single/shared coordination are
demonstrated by the `file`, `single` and `shared` modes of
`drasi-server/examples/native-sqlite.rs`. All modes keep query evaluation in the
hosting instance's graph and expose the actual worker-backed schema through
DrasiLib inspection.

### Shared native recovery

`NativeSqliteSource::new_replayable_with_consumers(id, stream, settings, consumers)`
retains a real `Arc<ConsumerProgressSet>` for the source's graph and component ID.
Each member must be a distinct actual progress owner, not an aggregate or a
same-named replacement. Construction only retains the resources; startup waits
for every member and requires recovered, persistent progress. Volatile query
storage and graph-owned volatile `input_progress` are not substitutes for this
journal's persistent recovery contract.

The source validates each member's readiness, failure, reset generation and
opaque journal position independently. It replays from the oldest required
SQLite transaction and retires only the prefix handled by every member. A slow
member therefore pins journal capacity; new writes fail and roll back when that
bound is reached. A stopped, failed or reset member fences live processing
without discarding retained records. The graph receives the actual consumer set
and one immutable SQLite cursor policy: positions compare the same journal epoch
and logical transaction sequence, not bootstrap row counts or transport counters.

For existing rows or late enrollment, use:

```rust,ignore
let consumers = Arc::new(ConsumerProgressSet::new(
    graph_id,
    source_id.clone(),
    [first_progress.clone(), second_progress.clone()],
)?);
let (source, snapshots) = NativeSqliteSource::coordinated_with_consumers(
    source_id, stream, settings, consumers,
)?;
let first = first_query
    .with_source_progress(first_progress)?
    .with_bootstrap(snapshots.for_consumer(&first_id)?);
let second = second_query
    .with_source_progress(second_progress)?
    .with_bootstrap(snapshots.for_consumer(&second_id)?);
```

Each query records its own initialization identity and commits its own snapshot
boundary. Providers share one serialized snapshot-worker/connection slot and
bounded row queues; they work with sequential or concurrent query startup. The
worker reads the actual journal boundary, then waits for the query's durable
initialization intent before mutating journal metadata or emitting rows. Live
SQL admission remains closed until every required query has committed its
initialization. The same exclusive slot prevents snapshot and live connections
from overlapping. Cancellation, timeout and error retain worker/client ownership
until awaited cleanup finishes; stale streams cannot affect replacement work.

The source/stream/graph/table binding for shared recovery is distinct from the
unchanged single-owner binding. Opening one mode's journal through the other
fails; no existing identity is silently reinterpreted. The shared mode uses the
existing transaction journal plus bounded consumer-enrollment metadata, not a
second journal. It records at most 256 consumer identities, including retired
identities; reaching this bound requires explicit recovery rather than silently
forgetting ownership.

Membership is immutable while running. After stop, reconstruction may add a
consumer if its checkpoint is still retained, or initialize it through its paired
snapshot in coordinated mode. A new snapshot uses the current journal head, so
older consumers may replay retained transactions without reapplying them to the
newly initialized query. Removal requires that the removed member has handled the
entire current head; a lagging or incomplete enrollment cannot be discarded.
Re-enrollment with an old cursor behind retired history fails. Losing completed
query initialization is not permission to replace it silently; interrupted
initialization follows the existing explicit `AutoReset` recovery policy.

For `SqliteSourceFactory`, bind `consumer_progress` to the actual
`ConsumerProgressSet` as a `Checkpoint` resource instead of `source_progress`.
For coordinated loading, set `coordinated_snapshot: true` and bind
`consumer_snapshots` to the returned `SqliteConsumerSnapshots` as a `Bootstrap`
resource; each query uses its own `for_consumer` provider. The source's existing
`client` dependency is unchanged. Single-owner/group progress and snapshot slots
are mutually exclusive, and configuration, graph/producer identity and exact
resource ownership are validated.

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
