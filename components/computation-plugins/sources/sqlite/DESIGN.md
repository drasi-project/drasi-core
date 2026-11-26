# Native SQLite source: design

`drasi-computation-source-sqlite` is a native ComputationGraph source (ABI
0.2.0) that owns one SQLite connection. Every write that reaches the database
through the source (its REST API, or a Rust `SqliteHandle` when the crate is
linked) runs on that connection, under an authorizer, and is captured by
SQLite's pre-update hook inside the same transaction. A committed transaction
becomes one output envelope; a rolled-back one produces nothing.

The engine (worker, capture, journal, snapshots, REST routes) lives in
`drasi-sqlite-common` (feature `native-runtime`, `rest`) and is shared with
the host-linked native source in `drasi-source-sqlite`. This crate adds the
four factories, the flat configuration, lifecycle, management, the hardened
REST listener and the source/bootstrap coordinator.

The source is not a change-data-capture reader for a database that other
processes write: writes made by another connection are never captured (see
[External changes](#external-changes-and-ddl)).

## Modes

| Factory | Output port schema | Journal | Progress binding | Recovery |
|---|---|---|---|---|
| `drasi.sqlite/changes` | `GraphChangeCodec` | none | none | volatile |
| `drasi.sqlite/transactions` | `SourceTransactionCodec` | none | none | volatile, atomic per transaction |
| `drasi.sqlite/replay-transactions` | `SourceTransactionCodec` | owned | one persistent query (`source_progress`) | process restart |
| `drasi.sqlite/shared-transactions` | `SourceTransactionCodec` | owned | fixed consumer set (`consumer_progress`) | process restart |

All factories use implementation and configuration version `1`. The output
port is `out`.

The description is declared: outputs carry only source replay progress.
The journal factories require their progress binding (`source_progress` or
`consumer_progress`) to declare durability, so the graph refuses wiring them
to volatile or undeclared progress (an `Incompatible` binding failure on the
source node) before the database is opened; the source does not check
durability itself. The volatile factories keep no progress. The in-process
wrapper in `drasi-source-sqlite` declares the same description
(`drasi_sqlite_common::native::description`).

- **changes** emits each committed transaction's row changes as one graph
  change envelope. Nothing is retained: output not taken by the host before
  Stop is lost (and reported, see [Stop](#stop)).
- **transactions** emits complete source transactions for a
  transaction-enabled query. Also volatile.
- **replay-transactions** appends each committed transaction to a journal in
  the same database file, in the same SQLite transaction as the user's write.
  The worker replays the journal from the query's committed cursor and
  retires records once the query has handled them. A Stop loses nothing.
- **shared-transactions** shares one journal across a fixed set of
  persistent consumers; a record is retired when every member has handled it.

### Journal tables and ownership

The journal factories own four tables in the user's database file:

| Table | Content |
|---|---|
| `__drasi_native_state_v1` | binding (source, stream, graph, owner, tables, keys), journal epoch, head, retired position, retained bytes, capture identity |
| `__drasi_native_outbox_v1` | retained transactions (sequence, timestamp, payload) |
| `__drasi_native_consumers_v1` | shared mode: per-consumer boundary and position |
| `__drasi_native_bootstrap_v1` | coordinated mode: the snapshot epoch |

They are created in one transaction on first start. The binding records the
source component ID, so a journal created by one source is never adopted by
another, and a start with a different stream, owner, table selection or keys
fails. The authorizer denies every application statement that names a
`__drasi_` object; REST never lists them; `initSql` may not touch them.

A file holds at most one journal. A non-journal factory refuses to start on a
file that has a journal ("native SQLite cannot disable replay on a
journal-owned database"); remove the journal source with erase first.

Replay modes open the file with `journal_mode=DELETE`,
`locking_mode=EXCLUSIVE` and `synchronous=FULL`. The exclusive lock means no
other process can read or write the file while the source runs. The claimed
durability is process restart, not power loss on every file system.

### Element identity

Shared with the bootstrap providers through `drasi-sqlite-common`:

- configured or primary keys: `sqlite:v1:` + table + a sorted map of key
  column to typed value;
- otherwise the physical rowid: `sqlite:rowid:v1:` + table + rowid, using a
  rowid alias that no user column shadows.

Labels are the table name; properties are every column (BLOBs as base64). An
update that changes the key emits a delete of the old identity and an insert
of the new one in the same transaction.

## Architecture

```
REST listener ──┐                       ┌─► output channel ─► next()
(network-common │  bounded command lane │
 serve + auth)  ├─► database worker ────┤
Rust handle ────┘  (one connection,     └─► journal (replay modes)
                    authorizer, hooks)
```

- **Database worker**: one blocking thread (`spawn_owned_blocking_worker`)
  owns the connection. It takes commands from a bounded lane
  (`commandCapacity`): schema reads, key lookups, read-only queries and
  transaction scopes. A transaction scope has its own command lane, so a
  caller's statements run in order inside `BEGIN IMMEDIATE … COMMIT`.
- **Capture**: the pre-update hook converts each row change of a selected
  table into a `SourceChange`, bounded by `maxTransactionChanges` and
  `maxTransactionBytes`. The commit hook vetoes a commit whose capture failed,
  so a write is never committed without its output (or journal record).
- **Output**: at most one committed envelope is held by the worker beyond the
  output channel (`outputCapacity`). While it is held, new writes wait; this
  is the source's backpressure.
- **REST listener**: the shared routes from `drasi-sqlite-common::rest_api`,
  served by `drasi-computation-network-common`'s listener (connection limit,
  10 s header-read deadline, optional TLS, graceful drain) with bearer
  authentication from the same crate's `Authenticator`.

### Authorizer

Application statements (REST, `/api/sql`, Rust handle) run under an
authorizer that allows reads, DML and DDL on non-reserved tables and schema
objects, and denies:

- virtual tables;
- every `PRAGMA`;
- `BEGIN`/`COMMIT`/`ROLLBACK` (the source owns transaction boundaries);
  `SAVEPOINT` is allowed only inside a scope and is tracked by capture;
- `ATTACH`/`DETACH`, temporary tables, indexes, triggers and views;
- `load_extension`;
- any statement naming a `__drasi_` object.

### Capture identity and schema evolution

The source binds only what element identity needs, as PostgreSQL's binding
covers only key columns (decision D-7): for each captured table, the columns
its element IDs are derived from (configured keys, the primary key, or the
rowid). Every commit that changed the schema is checked against it:

- **Allowed** (identity extended): new tables, `ALTER TABLE … ADD COLUMN`,
  dropping or renaming a non-key column, indexes, views and triggers, and
  any DDL on a table the source does not capture. Capture re-reads the
  schema when `schema_version` changes, so later changes carry the new
  columns. Run a column change in its own transaction: a write in the same
  transaction as the `ALTER` is refused.
- **Refused** (identity broken): dropping or renaming a captured table, or
  changing its key columns, would orphan the element IDs queries hold. Through
  the source the transaction rolls back with `SourceFailure::SchemaChanged`
  and the source keeps running.

In journal modes the capture identity is recorded in the state table and
updated in the same transaction as the DDL. DDL applied with another tool
while the source is stopped is checked the same way at the next start: an
extension is recorded and replay continues, a broken identity fails start
with `SourceFailure::SchemaChanged`. Volatile modes check DDL through the
source the same way.

### `initSql`

`initSql` is a script run on every connection the source opens (the live
worker and coordinated snapshot workers), after limits are applied and
before the authorizer and capture hooks are installed. It may create tables,
indexes, views and triggers and set pragmas. A guard authorizer still denies
`__drasi_` objects, `ATTACH`/`DETACH` and `load_extension`. Its changes are
not captured. It must leave no transaction open.

It makes an in-memory source usable through a loaded library, where no Rust
handle exists. For a file database make it idempotent (`CREATE TABLE IF NOT
EXISTS`); in journal modes it must not add rows after the first start, and
any schema change it makes must keep the capture identity (see above).

### `/api/sql`

Disabled by default. When `sqlApi: true` (which requires `restApi` and an
`authToken`), `POST /api/sql` accepts either

```json
{"execute": [{"sql": "INSERT INTO t VALUES(?, ?)", "params": [1, "a"]}, …]}
{"query": {"sql": "SELECT * FROM t WHERE id = ?", "params": [1]}}
```

`execute` runs every statement in one transaction and returns
`{"rowsAffected": [..]}`; `query` runs one read-only statement and returns
`{"rows": [..]}`. Both go through the worker and the authorizer, so they are
captured exactly like REST writes. SQL errors return 400 with the message.

### In-memory sources

A source without `path` opens a private in-memory database that lives only
while the worker runs: each start begins empty. The factories refuse an
in-memory configuration unless it has `initSql` or `sqlApi`, because a loaded
plugin would otherwise have no way to create tables. The Host rejects a
literal `path: null` as a type error before the plugin sees it; omit the
field instead. A Rust caller that constructs `SqliteSource::new` directly may
use an in-memory database without either, since it can create tables through
`handle()`.

## Ordering, checkpoints and recovery

Output follows commit order on the single connection. Volatile modes number
envelopes with a transport sequence starting after the owner's last observed
sequence. Journal modes identify each transaction by `(epoch, sequence)`
(JSON, version 1); the query's committed checkpoint carries that position.
On start the worker validates the binding, the recorded capture identity, the journal
accounting and the consumer cursor, then replays from the cursor. Feedback
from consumer progress retires handled records. The journal is bounded by
`maxJournalTransactions` and `maxJournalBytes`; writes fail while it is full.

Coordinated initialization (`coordinatedSnapshot: true`) pairs the source with
`drasi.sqlite/coordinated-bootstrap` or `drasi.sqlite/shared-bootstrap` through
the source's `NativeSourceBootstrapCoordinator`: each query's provider reads a
consistent snapshot before the live worker admits writes, and the snapshot
and live workers share one exclusive lease. The coordinator holds the
validated configuration of its source; a source whose request differs from
its coordinator's is refused.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Validates the configuration and that a database file's directory exists. Opens nothing. |
| `start` | Refused while an earlier activation is owned. Honours cancellation and the deadline across progress readiness, opening the database, `initSql`, journal validation and binding the listener. A failed or cancelled start signals every spawned worker to exit; Stop joins them. A pre-cancelled start does nothing. After success the source reports `ready` through its `ControlSender`. |
| `pause` | See [Pause](#pause). |
| `resume` | Reopens writes, journal replay and output. |
| `stop` | See [Stop](#stop). |
| `dispose` | Requires a stopped source. See [Erase](#erase). |

### Pause

Pause gates the source's independent work:

1. REST refuses every write (`POST`, `PUT`, `DELETE`, `/api/sql`) with `503`
   and `Retry-After: 1` from the moment Pause is requested.
2. The worker parks at its next transaction boundary: an open transaction
   finishes first, then the worker acknowledges. Pause returns when it does,
   within the deadline.
3. While paused the worker refuses transaction commands with the typed
   `SqlitePaused` error (handle writes; REST maps it to 503), stops journal
   replay and holds any committed envelope instead of emitting it. Reads
   (`GET` routes, `query`) are still answered, and consumer feedback still
   retires journal records, so memory stays bounded.

A cancelled or expired Pause reopens admission and leaves the source running
as before. Resume clears the request; the worker resumes output at once.

### Stop

Stop runs in two phases so that no accepted request can wait on a worker that
is itself waiting for output (the SQ-01 deadlock):

1. REST stops accepting requests and the worker starts *draining*: it keeps
   serving requests already accepted, but never waits for output. A held
   envelope is delivered if the channel has room, otherwise dropped
   (volatile) or left in the journal (replay modes).
2. When REST has drained, the worker refuses queued commands with
   `SqliteStopping`, rolls back anything open and exits.

`Drain` waits for in-flight HTTP requests and lets an open Rust-handle
transaction finish; `Abort` cancels the listener (dropping connections) and
rolls back at once. Both honour the deadline (or `shutdownTimeoutMs` when the
host supplies none). On expiry Stop returns a retryable error and keeps every
unfinished worker owned; a later Stop completes the join, and Start is refused
until then. Committed volatile transactions that could not be delivered are
counted and logged as a warning; journal modes never lose output. An earlier
worker or listener failure was already reported through `next()`; Stop after
it succeeds.

### Erase

`dispose` with `StateIntent::Erase`:

- journal factories drop `__drasi_native_{outbox,consumers,bootstrap,state}_v1`
  in one `BEGIN IMMEDIATE` transaction on a `spawn_blocking` thread. User
  tables are never touched; the database file is never deleted;
- a missing file, a file without a journal, or a journal whose binding names
  another source is success (nothing this source owns is there);
- a locked database (`SQLITE_BUSY` after the busy timeout) fails retryably
  and changes nothing, so the host's retry repeats safely;
- volatile factories own no external state: erase is a documented no-op.

Every factory declares `erases_owned_state`. Erase does not touch rows the
source wrote into user tables; those are the application's data.

## Management

The factory's `UpdatePlanner::assess` and `NativeManagement::assess` are pure:
they parse the current and proposed configuration and classify each changed
field by the weakest safe boundary; the strongest wins.

| Boundary | Fields |
|---|---|
| InPlace (Pause) | `authToken`, `authTokenSecondary` (token rotation; the listener reads the token store per request) |
| Restart (Stop) | `maxTransactionDurationMs`, `maxSqlBytes`, `commandCapacity`, `outputCapacity`, `shutdownTimeoutMs`, `walMode`, `initSql`, `restApi`, `restHost`, `restPort`, `allowUnauthenticated`, `tlsCertificatePem`, `tlsPrivateKeyPem`, `tlsClientCaPem`, `maxRequestBytes`, `requestTimeoutMs`, `maxConnections`, `pageSize`, `sqlApi`; adding or removing `authToken`; `maxTransactionChanges` and `maxTransactionBytes` (journal factories: increase only); `maxJournalTransactions`, `maxJournalBytes` (increase only) |
| Unsupported | `stream`, `path`, `tables`, `tableKeys`, `coordinatedSnapshot`; decreasing a journal limit; any change to a coordinated source |

Unsupported changes need removal (with erase for a journal factory) and a new
source. Journal limits may only grow so a retained record always stays
replayable. A coordinated source must match its coordinator exactly.

`apply` refuses (leaving the object unchanged) when the request's current
configuration differs from the object's, when the re-assessed plan differs,
or when the proposal does not validate. An in-place apply requires a
completed Pause; a restart apply requires a stopped source.
`configuration()` returns the flat configuration the object runs with,
defaults filled, including after an update. It includes secrets: it is the
persistence hook.

## Security

- The REST listener is off by default and binds `127.0.0.1` by default.
  `restHost` must be a literal IP address; a non-loopback address requires
  `authToken` or an explicit `allowUnauthenticated: true` (for deployments
  behind an authenticating proxy).
- With a token, every route except `/health` requires
  `Authorization: Bearer <token>` (constant-time comparison; two tokens allow
  rotation). Failures return 401 with `WWW-Authenticate: Bearer` and are logged
  at debug level.
- Bodies above `maxRequestBytes` get 413; a request running longer than
  `requestTimeoutMs` gets 504 (a write whose commit was already submitted may
  still complete); connections above `maxConnections` are closed; a client
  that does not send its headers within 10 s is disconnected.
- `GET /api/tables/{table}` is paged (`?limit=&offset=`, at most `pageSize`,
  ordered by the table key; `X-Next-Offset` names the next page), so a table
  larger than the read bound is still listable.
- Optional TLS (`tlsCertificatePem`, `tlsPrivateKeyPem`) and mutual TLS
  (`tlsClientCaPem`).
- `authToken`, `authTokenSecondary` and `tlsPrivateKeyPem` are top-level
  secrets; `Debug` output redacts them and summarises `initSql` by length.
- `/api/sql` exists only when enabled and never without a token. All SQL runs
  under the authorizer.

## External changes and DDL

Only writes on the source's own connection are captured. Do not write the
database from another process while the source runs; in journal modes the
exclusive lock prevents it. If the file is changed externally between runs:

- rows changed externally are not emitted, and the queries diverge from the
  table until they are re-initialized;
- DDL is checked against the capture identity at the next start in journal
  modes: additive changes are recorded and replay continues; dropping a
  captured table or changing its key columns fails with
  `SourceFailure::SchemaChanged`. Remove the source with erase and add it
  again (with `coordinatedSnapshot` if the tables have rows);
- in volatile modes the worker re-reads the schema per transaction, so added
  columns appear in later changes; a dropped key column fails capture of that
  table's writes.

DDL through the source is checked at commit as described in
[Capture identity](#capture-identity-and-schema-evolution).

## WAL mode

`walMode: true` sets `journal_mode=WAL` on a file database without a journal,
so other processes can read the file while the source writes (they still must
not write it). Start fails if SQLite cannot enable WAL. Journal factories
refuse `walMode`: they keep `DELETE` mode with an exclusive lock.

## Failure model

| Failure | Behaviour |
|---|---|
| Invalid configuration | Refused at construction (`from_value`), with the field named. |
| Database open, `initSql`, journal or capture identity check fails | Start fails; workers are signalled to exit; Stop joins. A broken capture identity is `SourceFailure::SchemaChanged`. |
| DDL through the source breaks the capture identity | That transaction rolls back with `SourceFailure::SchemaChanged`; the source keeps running. |
| REST bind fails | Start fails. |
| Listener or worker ends after start | Reported once through `next()`; the source withdraws readiness (`ControlSender::not_ready`). Stop, then start again. |
| Commit outcome uncertain | The worker stops (fenced) rather than guess; the failure surfaces through `next()`. |
| A write exceeds a limit | That transaction rolls back; the caller gets an error; the source keeps running. |
| Journal full | Writes fail until consumers handle retained records. |
| Stop deadline expires | Retryable error; workers stay owned. |
| Erase while locked | Retryable error; nothing dropped. |

## Observability

Logs go through the `log` facade; a dynamically loaded library falls back to
standard error for warnings and errors (`drasi_computation_network_common::diag!`).
Per-request REST rejections log at debug level. Readiness is reported through
the `ControlSender`: `ready` after a successful start, `not_ready` when the
worker or listener fails. The SDK has no metric facility yet, so counters
(commits, rollbacks, journal depth, REST requests) are not exported.

## Rust embedding

A host that links the crate can construct `SqliteSource::new`,
`new_replayable` or `new_replayable_with_consumers` from a `SqliteConfig`, call
`handle()` for SQL (writes captured, refused while paused) and
`with_rest_api` for an unauthenticated loopback listener. `bound_rest_address`
returns the bound address (useful with port 0). These are not available
through a loaded library.

## Known limitations

- No capture of writes made outside the source's connection.
- No metrics export (no SDK facility).
- No rate limiting on the REST listener.
- `path: null` is refused by the Host's type check, not with the plugin's
  in-memory message; omit `path` instead.
- Cold reconstruction with undrained persistent input-pipe frames is not a
  qualified guarantee.

## Differences from the legacy source

| Legacy (`drasi-source-sqlite`) | Native |
|---|---|
| Ordinary Source-plugin ABI; hostname allowed for REST host | ComputationGraph ABI 0.2.0; literal IP |
| Nested snake_case configuration | Flat camelCase, defaults for every bound |
| Unauthenticated REST, unbounded `GET /api/tables/{table}` | Optional bearer/TLS, body and time limits, paging |
| Arbitrary SQL, DDL and transaction callbacks through Rust APIs | REST, `/api/sql` (authorizer applies) and `initSql`; Rust handle when linked |
| Legacy element IDs | Versioned typed IDs (`sqlite:v1:`, `sqlite:rowid:v1:`) |
| No pause | Pause gates writes and output at a transaction boundary |
| No erase | Erase drops the journal tables |

The host-linked native source (`drasi_source_sqlite::native::SqliteSource`)
uses the same engine. It now forwards pause, resume, initialize and dispose
(erase) like this plugin; its Drain rolls back an open Rust transaction
callback instead of waiting for it, as before.
