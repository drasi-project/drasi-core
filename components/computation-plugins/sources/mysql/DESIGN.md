# Native MySQL source: design

`drasi-computation-source-mysql` streams MySQL row-based binary logs into a
ComputationGraph. It is a native plugin (ABI 4.0) with seven factories, each at
implementation and configuration version `1`. The replication engine lives in
`drasi-mysql-common` (`native` feature): the owned MySQL protocol and
authentication (`mysql_native_password`, `caching_sha2_password`), TLS
(`native-tls`, including client certificates), the catalog reader, the binlog
decoder, key and value mapping, bounded transaction assembly, the cursor
binding, the DDL policy and the worker. The native MySQL bootstrap providers
and the in-process wrapper in `drasi-source-mysql` use the same engine. None
depends on `drasi-lib`, and the native build does not compile `mysql_async`
or rustls.

Field reference, prerequisites and runbook: [CONFIGURATION.md](CONFIGURATION.md).

## Modes

| Factory | Emits | Progress | Guarantee |
|---|---|---|---|
| `drasi.mysql/changes` | One `GraphChangeCodec` envelope per row change | None | Live changes from `startFrom`; no recovery claim |
| `drasi.mysql/untracked-transactions` | One `SourceTransactionCodec` envelope per transaction | None | Whole transactions; no recovery claim |
| `drasi.mysql/transactions` | Whole transactions | One atomic persistent query (`source_progress`) | Whole commits; replay from the committed cursor |
| `drasi.mysql/shared-transactions` | Whole transactions | A fixed consumer set (`consumer_progress`) | As above, per member; replay from the minimum member |
| `drasi.mysql/shared-changes` | Row changes | A fixed consumer set | Partial transactions resume at the unhandled row |
| `drasi.mysql/positional-changes` | Rows keyed by `@N` column offsets | None | MySQL 5.7 / limited row metadata profile |
| `drasi.mysql/shared-positional-changes` | Positional rows | A fixed consumer set | As shared rows, positional profile |

Factory identity selects the contract. Mode fields (`retention`,
`persistent`) are accepted only by the factories they apply to; every other
field is the shared flat configuration also used by the bootstrap providers.

## Capture

The source registers as a replica (`COM_REGISTER_SLAVE` with `serverId`) and
requests a binlog dump at a file offset (`COM_BINLOG_DUMP`) or, for
`startFrom: gtid` before the first transaction, after a GTID set
(`COM_BINLOG_DUMP_GTID`). Every frame is checked for its CRC32 checksum,
contiguity (`log_pos` and event size), the format description and the
expected event types. Row images must be complete (`binlog_row_image=FULL`)
and, except in positional mode, carry full metadata
(`binlog_row_metadata=FULL`): column names, primary key flags, signedness,
character sets, ENUM/SET labels.

A transaction is assembled from its GTID (or anonymous GTID) event to its XID
or `COMMIT`. Transaction output buffers the transaction within
`maxTransactionChanges`, `maxTransactionBytes` and
`maxTransactionDurationMs`; a transaction over a limit fails the source
without publishing a prefix and is retried after the limit is raised. Row
output emits each row event's changes as it is decoded (untracked) or expands
a decoded transaction row by row (tracked rows). An update that changes the
key is a delete plus an insert.

### Element identity and types

Element IDs are `mysql:v1:` typed identities over the database, table and key
values (`keys::transaction_element_id`), shared with the bootstrap providers.
The label is the table name. The key is `tableKeys` when configured, else the
primary key; a keyless table without `tableKeys` is refused at start (a column
named `id` is never taken as identity). Positional rows use `@N` property names and
explicit `@N` keys.

| MySQL | Property |
|---|---|
| integers, YEAR | integer (unsigned BIGINT above `i64::MAX` as a decimal string) |
| FLOAT, DOUBLE | float (non-finite rejected) |
| DECIMAL, temporal types | canonical string |
| CHAR, VARCHAR, TEXT, ENUM, SET | string; non-UTF-8 character sets (latin1, cp1250, utf16, ...) are transcoded losslessly with `encoding_rs` from the column's catalog character set |
| BINARY, VARBINARY, BLOB, BIT | list of byte integers |
| JSON | canonical JSON text |
| GEOMETRY and its subtypes | extended WKT, `SRID=4326;POINT(1 2)` |
| VECTOR (MySQL 9) | list of floats |

The schema reader (`SourceSchemaReader`) publishes the captured catalog of the
current activation: one node per selected table with each column's type hint
and description. It reads no database on demand, returns nothing before
capture, and is fenced after Dispose.

## Cursors and binding

- **Transactions:** the cursor is `{version, binding, file, start, end,
  digest}`: the transaction's binlog file, its GTID event offset, its commit
  end offset, and (with retention) a SHA-256 of its raw events. The logical
  sequence is `file number << 32 | end`. Resumption re-reads the committed
  transaction at `start` and compares it with the cursor (`anchor`), so a
  reused or rewritten history fails instead of being reinterpreted.
- **Rows:** the row cursor adds the row's offset and count within the
  transaction and its logical allocation counter. The last row represents the
  completed boundary; a partial transaction resumes at its start and skips the
  handled rows.
- **Snapshot boundary (version 2):** a coordinated snapshot hands over at
  `{file, start: 4, end: boundary}` with a constant-size proof of the file's
  identity: a SHA-256 of its format description event (creation time,
  originating server, server version, header lengths, checksum algorithm),
  read by starting the dump at the boundary. With the binding's
  `@@server_uuid` it detects a reset or rewritten binlog; the server refuses
  a boundary beyond the file, and the first event must start exactly at the
  boundary. Verification therefore costs two events whatever the file size,
  at the provider, at every source start and on reconnect. A reset that
  recreates the same file name within the same second on the same server is
  not detected.
- **Progress records (row factories):** an empty change set carrying a row
  cursor without a row offset (`GraphChangeCodec::encode_source_progress`),
  committed by consumers like any other event. A fresh group (no member has a
  cursor) with `startFrom: end` first emits its origin, an **origin boundary
  (version 3)** `{file, start: 4, end}` proven like a snapshot boundary, and
  holds every row until each member has committed a cursor; a restart, even
  with a new source object, resumes there and delivers changes made while the
  process was down. While the selected tables are quiet (30 seconds without an
  emission), the end of the next transaction that touches no selected table
  is emitted as a progress record (a version-1 transaction cursor with no
  rows), so committed cursors follow binlog rotation and purging. The
  transaction factories already do this with an empty committed transaction
  for every binlog transaction.
- **Shared progress:** replay starts at the members' minimum physical
  position, and a member without a cursor joins there (a crash between two
  members' first commits therefore restarts); every member's retained
  history is verified, and members that disagree about the same boundary
  fence the source.

The **binding** (format version 2) is a SHA-256 over the facts a cursor
depends on: the scope (`mysql-native-v2`, `mysql-native-consumers-v2` or
`mysql-native-consumer-rows-v2`, plus the owner), `@@server_uuid`, the
database, the selected table set with each table's key columns and their
declared types, the source, the stream and the configured start
(`snapshot` without its lock timeout). Positional sources wrap it in
`mysql-positional-rows-v2`. Non-key columns are not bound, so adding,
dropping or changing an ordinary column keeps committed cursors. A cursor
written under a different binding (including every version-1 cursor) fails
with the typed `SourceFailure::SchemaChanged`.

## Snapshot handover

`startFrom: snapshot` pairs the source with a coordinated bootstrap provider
(`drasi.mysql/coordinated-bootstrap`, `shared-transaction-bootstrap` or
`shared-row-bootstrap`). The provider samples the server clock, takes
`FLUSH TABLES WITH READ LOCK` (bounded by `snapshotLockTimeoutMs`) on one
session, starts `START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY` on
another, pins the selected tables' metadata locks, reads the binlog position
and releases the global lock before scanning through bounded server-side
cursors. The boundary and its file-identity proof are committed by the query
together with the snapshot. The source then refuses to start until every
owner has a completed boundary, and admits live changes only after every
retained boundary's file identity matches. FTWRL is the only server-wide lock the source
family takes, and only while the view is established.

## Positional profile

The positional factories support MySQL 5.7.9 or newer and servers without
`binlog_row_metadata=FULL`. Properties are projected by column ordinal and
typed with captured catalog hints. Before admitting rows the source scans the
retained history from its start for catalog-changing statements and checks the
catalog is stable; live catalog changes fence the source. Keys must be
explicit `@N` offsets. There is no coordinated snapshot.

## Retention checks

Startup and every reconnect check that the binlog file at the resume offset
is still listed by `SHOW BINARY LOGS`; otherwise the source fails with the
typed `SourceFailure::PositionUnavailable` (and the legacy
`SourceError::PositionUnavailable` for the in-process wrapper). A dump refused
with error 1236 is also `PositionUnavailable`. `retention: untilProcessed`
additionally requires automatic expiry disabled
(`binlog_expire_logs_seconds=0` and `expire_logs_days=0`, or
`binlog_expire_logs_auto_purge=OFF`). The source never changes server
settings and never purges logs.

## DDL policy

Schema statements are `QUERY_EVENT`s. The policy (`native::ddl`):

| Statement | Effect |
|---|---|
| Any statement that names no selected table, including DDL on unselected tables and statements logged in another schema | Ignored |
| Administrative statements: `ANALYZE`, `OPTIMIZE`, `REPAIR`, `FLUSH`, `GRANT`, `REVOKE`, `SET`, `SAVEPOINT`, users, roles, routines, triggers, events, views | Ignored |
| `ALTER`/`RENAME`/`DROP`/`CREATE` (`TABLE`, `INDEX`) naming a selected table | The worker re-reads that table's catalog through a separate connection. A dropped or renamed table, or changed key columns or key types, fences with `SourceFailure::SchemaChanged`; any other change is adopted and decoding continues |
| `TRUNCATE` of a selected table | `SchemaChanged` (rows disappear without row events) |
| `DROP DATABASE` of the configured database | `SchemaChanged` |
| Statement-based DML on a selected table | `SourceFailure::Unsupported` |

A new table appearing in a tracked database-wide selection is
`SchemaChanged` (the selected table set is bound); an untracked database-wide
row source adds it, or skips it with a warning when it cannot be captured.
Matching is case-insensitive and conservative: an identifier that only
resembles a selected table causes a harmless re-inspection. The re-read
catalog is the current one, so a source far behind several schema changes may
fence where an exact historical catalog would not. Positional sources keep
their stricter catalog fence.

## Worker

One worker task owns the replication connection. It validates the host's
committed progress on every change (failed, reset, unready or non-persistent
owners fence the source), decodes events, queues decoded envelopes and
admits them through a bounded channel (capacity one), expanding tracked rows
lazily.

### Pause

Pause stops at a transaction boundary. A transaction being decoded is
finished first: in untracked row mode its rows keep flowing; in transaction
and tracked row modes its output is held, not emitted. While paused nothing is
decoded or emitted, and the worker acknowledges the pause to the hook. It
keeps reading the socket into a buffer bounded by `maxProtocolBytes`, so the
server's dump thread is not blocked into `net_write_timeout`; heartbeats are
discarded and the dead-peer watchdog keeps running. If the buffer would
overflow, or the connection ends while paused, the worker closes the
connection deliberately and Resume reopens it at the end of the last emitted
transaction (one attempt even when `reconnectBudgetMs` is 0). Resume decodes
the buffered events first. A cancelled or expired Pause leaves the source
running. The SDK `PauseGate` is not used because it has no acknowledgement.

### Reconnection

A transport failure (I/O error, timeout, a dead peer, or server errors 1040,
1053, 1077-1079, 1152-1161, 1203, 1927, 3169, 4031) reconnects in every mode
with full-jitter exponential backoff (250 ms doubling to 30 s,
`drasi_computation_contracts::reconnect::BackoffPolicy`) until
`reconnectBudgetMs` is spent; `0` disables it. The worker reconnects at the end
of the last transaction it emitted, so nothing is lost or repeated for
transaction modes and tracked rows; for transaction modes it re-reads that
transaction and compares it with its cursor. Before resuming it checks
`@@server_uuid` (a different server is `SourceFailure::Unsupported`: failover
needs GTID cursors, which are not implemented), the bound facts (tracked
modes: the binding; untracked: key facts of the captured tables) and
retention. Untracked row mode may repeat the rows of a transaction that was
interrupted mid-way, because it emits rows before their commit.
Permanent failures (`SourceFailure`, authentication, protocol violations)
never reconnect.

The **dead-peer watchdog** fails a read that sees no event or heartbeat for
three `heartbeatIntervalMs` periods (at least `ioTimeoutMs`); this is a
transport failure. When the server kills the dump thread of a lingering
session, re-registering with the same `serverId` replaces it, so the source
needs no `KILL` ownership proof (unlike the stored-procedure sink's SQL
sessions).

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Validates the configuration. No I/O. |
| `start` | Takes activation credentials. Waits for progress readiness (which may include a coordinated snapshot) bounded only by the host deadline and cancellation; then connects, inspects and opens replication within `ioTimeoutMs`. A failed or cancelled start leaves no worker and keeps a cleanup guard: Stop is required before the next start. |
| `pause` / `resume` | As above. |
| `stop` | `Drain` closes the replication connection gracefully at the next await; `Abort` cancels the worker. Both honour the deadline; on expiry the worker stays owned and a later Stop completes. A worker that had already failed (reported once through `next`) does not fail Stop. |
| `dispose` | Retires the schema reader. Refuses while a worker is owned. Idempotent. |

### Erase (owned versus borrowed state)

The source owns no server-side state: no slot, no table, no registration that
outlives its connection (a replica registration ends with the connection).
Query progress and consumer progress are host-owned. The factories declare
`erases_owned_state`, and Dispose with `StateIntent::Erase` is an idempotent
no-op beyond retiring the schema reader (the HTTP and gRPC sources follow the
same rule). Binlog files are never purged.

## Management

`Factory::assess` (pure) and `NativeManagement` classify every field; see
[CONFIGURATION.md](CONFIGURATION.md#updates). `reconnectBudgetMs` applies in
place after a completed pause. Connection, credential, TLS, `serverId`,
heartbeat, timeout and limit changes apply at Stop (transaction limits may
only grow). Anything the cursor or element identities are bound to
(`stream`, `database`, `tables`, `tableKeys`, `retention`, `persistent`) is
unsupported; start fields are a restart only for untracked factories or
before the first checkpoint. `apply` refuses when the current configuration,
the plan or the proposed configuration does not match.

## Security

- `password` and `tlsClientKeyPem` are the only secret fields; both are top
  level, so hosts resolve them from secret references. `Debug` redacts them.
- Activation credentials (user/password from the host) replace the configured
  user and password for one activation; the context is the configured host
  and port.
- Only `require_verify_full` verifies the server certificate and host name;
  weaker modes log a warning (see CONFIGURATION.md).
- Catalog queries use hex string literals (`_utf8mb4 0x...`), and snapshot
  statements quote table names with backticks, doubling embedded backticks
  (`keys::quote_identifier`), so any MySQL identifier is safe.
- Every buffer is bounded by `maxProtocolBytes` or the transaction limits.

## Observability

Readiness changes (streaming, reconnecting with the cause, failed) are logged
through the `log` facade with the component ID; retries, skipped tables,
adopted schema changes and pause-buffer overflows are logged too. The host's
`ControlSender` is bound only for components with a control interface, which
this source does not declare, so readiness is not reported to the host; the
SDK has no metric facility yet (Source X-07).

## Unsupported

- MariaDB (rejected at the handshake) and MySQL before 8.0.20 (5.7.9 for the
  positional factories).
- GTID-based cursors and resumption on another server after a failover:
  checkpoints are file positions bound to `@@server_uuid`. `startFrom: gtid`
  only selects the initial position.
- Statement-based or mixed binlogs, partial row images, partial JSON updates
  (`binlog_row_value_options=PARTIAL_JSON`), binlog encryption on the wire,
  compressed transaction payloads (`binlog_transaction_compression`).
- Coordinating the source and its bootstrap providers through the SDK
  bootstrap coordinator (MY-16): the pair shares configuration and committed
  progress instead.
- Inferring identity from a column name: a keyless table needs `tableKeys`.

## Differences from the legacy source

- One flat camelCase configuration; secrets only for `password` and the TLS
  client key; managed updates; activation credentials.
- Typed element IDs (`mysql:v1:`) and cursors; the legacy string IDs and
  checkpoints are not read.
- Whole-transaction and consumer-progress modes, coordinated snapshots, and
  retained-history verification.
- The legacy source ignored DDL entirely; the native source ignores unrelated
  DDL and fences only changes to bound facts.
- The legacy heartbeat default (30 s) and automatic `serverId` are kept; the
  native source derives `serverId` from a SHA-256 of the component ID.
- Values are decoded strictly (invalid text in a column's character set
  fails instead of being replaced).
- A keyless table no longer falls back to a column named `id`; it needs
  `tableKeys`.
