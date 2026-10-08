# MySQL Source Plugin

Streams MySQL binlog changes into Drasi `SourceChange` events. Uses `mysql_async`'s native `BinlogStream` API for row-based replication.

## Requirements

- MySQL with binlog enabled
- `binlog_format=ROW`
- `binlog_row_image=FULL`
- `binlog_row_metadata=FULL`
- Replication user with `REPLICATION SLAVE` and `REPLICATION CLIENT`

## Example

```rust
use drasi_source_mysql::{MySqlReplicationSource, StartPosition};

let source = MySqlReplicationSource::builder("mysql-source")
    .with_host("localhost")
    .with_port(3306)
    .with_database("test")
    .with_user("replication_user")
    .with_password("secret")
    .with_tables(vec!["users".to_string()])
    .with_start_position(StartPosition::FromEnd)
    .build()?;
```

## Configuration Options

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `host` | `String` | `"localhost"` | MySQL server hostname or IP address |
| `port` | `u16` | `3306` | MySQL server port number |
| `database` | `String` | **(Required)** | Database name to connect to |
| `user` | `String` | **(Required)** | Database user with replication privileges |
| `password` | `String` | `""` | Database password |
| `tables` | `Vec<String>` | `[]` | List of tables to monitor |
| `sslMode` | `SslMode` | `if_available` | SSL mode: `disabled`, `if_available`, `require`, `require_verify_ca`, `require_verify_full` (see [SSL Modes](#ssl-modes)) |
| `tableKeys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration (see below) |
| `startPosition` | `StartPosition` | `from_end` | Where to start replication: `from_start`, `from_end`, `from_position`, or `from_gtid` |
| `serverId` | `u32` | Auto-generated | MySQL server ID for the replication connection. Auto-generated from source instance ID if not specified. |
| `heartbeatIntervalSeconds` | `u64` | `30` | Heartbeat interval in seconds |

### TableKeyConfig

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name |
| `keyColumns` | `Vec<String>` | Column names to use as primary key |

### SSL Modes

TLS support (rustls) is always compiled in — there is no feature flag to enable.
`sslMode` maps 1:1 to MySQL's `--ssl-mode`:

| Value | MySQL `--ssl-mode` | TLS | Verifies |
|-------|--------------------|-----|----------|
| `disabled` | `DISABLED` | no | — |
| `if_available` (default) | `PREFERRED` | opportunistic; falls back to plaintext | no |
| `require` | `REQUIRED` | required | no (encrypt-only) |
| `require_verify_ca` | `VERIFY_CA` | required | CA chain (skips hostname) |
| `require_verify_full` | `VERIFY_IDENTITY` | required | CA chain + hostname |

`if_available` and `require` skip certificate verification (matching MySQL), so they
protect against passive eavesdropping but not an active man-in-the-middle. Use
`require_verify_ca` or `require_verify_full` when server authenticity matters.

## Limitations

- Packets > 16 MB are not supported.

## Lifecycle and reliability boundary

The source serializes start/stop and owns its replication task before spawning
it. Stop wakes subscriber, initial-boundary, idle-binlog and reconnect waits,
then joins the worker before clearing subscriptions. Submitted connection,
dispatch and state work is not aborted. A cancelled stop or five-second cleanup
timeout retains ownership and prevents replacement; retry stop after the pending
operation finishes. Worker errors and panics remain observable during cleanup.

This repairs the Drasi-owned worker, not every driver-owned resource.
`mysql_async` 0.37.1 can detach connection cleanup on some failed connection or
binlog-registration paths, and its `BinlogStream::close` discards underlying
close failures. Awaiting the source worker therefore does **not** yet prove
complete driver cleanup. Full MySQL lifecycle qualification remains incomplete.

These changes do not alter the legacy event format, bootstrap or checkpoint
semantics and do not provide whole-transaction query visibility or exactly-once
delivery.

## Native ComputationGraph source

`native::MySqlSource` and `native::MySqlSourceFactory` are separate from
`MySqlReplicationSource`. They own their TCP/TLS connections directly; they do
not create `mysql_async` connections, pools or detached driver cleanup tasks.
The legacy API and its existing identity format are unchanged.

Construct the native source with a component ID, stream ID, `MySqlConfig` and
optional actual `Arc<QuerySourceProgress>`. Its `out` port advertises the schema
for the selected output mode. The factory identity is `drasi/mysql-native`, version
`1`; its `stream` and secret-backed `settings` configuration use the same types.
Recovery additionally requires the `source_progress` checkpoint resource of the
actual consuming query. A matching name or a resource from another graph is not
sufficient.

| Setting | Behavior |
|---------|----------|
| `output: changes`, `replay: null` | Fast ordinary graph changes, without whole-transaction assembly, hashing, journal writes or persistent progress. No atomic visibility or replay guarantee. |
| `output: transactions`, `replay: null` | One complete committed upstream transaction per envelope, without persistent replay ownership. |
| `replay: server` | Transactions resume from the consumer's durable checkpoint while MySQL retains the required history. Missing history fails explicitly; this does not advertise guaranteed retention. |
| `replay: until_processed` | Requires automatic binlog expiry disabled and advertises replay to the bound atomic consumer. Operators must maintain safe retention. |
| `transactions` | Explicit change-count, encoded-byte and assembly-duration limits. An oversized/incomplete transaction publishes no prefix and advances no checkpoint; retry after increasing limits. |
| `max_protocol_bytes` | Independently bounds each logical wire packet, retained catalog results, table-map cache, decoded fast-output batch and initial-load row, including charged per-envelope overhead. Initial/live JSON expansion and depth are also bounded. |
| `io_timeout_ms` | Bounds connection/setup, partial packet I/O and awaited cleanup. An otherwise idle stream does not expire. |

Neither retention mode changes MySQL retention settings or purges files. Strict retention
checks both the seconds setting and the older `expire_logs_days` setting; where
available, `binlog_expire_logs_auto_purge=OFF` also disables automatic expiry.
Operators must keep expiry disabled for `until_processed` while replay is needed.
Replay requires transaction output and either a fixed initial file/position or the
explicit coordinated snapshot mode below; `start: end` is only for non-replay
operation. A fixed initial position must be a real transaction boundary.
Resume binds to the server UUID, database, selected column definitions,
source/stream and consumer scope. It rereads and verifies the last committed
transaction's content before skipping it, rejecting replaced/reused history.
**Retain the file containing that checkpoint transaction as well as all later
files**, even though the checkpoint transaction itself is already processed.
The declared survival scope is Drasi process restart, not arbitrary upstream
data loss or storage power failure.

Native requirements are MySQL 8.0.20 or newer (not MariaDB), InnoDB tables,
`ROW` binlogs, `FULL` row images and metadata, `CRC32` checksums, explicit table
selection, full-column non-null primary keys, and UTF-8/ASCII/binary columns.
Use a distinct nonzero replication `server_id` for each active source. In
addition to replication permissions, the user needs selected-table/catalog read
access. Native row IDs use the shared typed `mysql:v1:` format, including binary
and composite keys; key-changing updates emit delete plus insert. These IDs are
not interchangeable with legacy bootstrap IDs.
Binary and BIT values preserve their bytes as integer lists; other values retain
the shared MySQL formatting. Unsupported selected column types are rejected
during startup, not deferred until the first changed row.

TLS is directly owned too. `require_verify_full` verifies the trusted CA and
hostname; `require_verify_ca` verifies the CA only. `require` and `if_available`
provide encryption without server identity verification. Native `if_available`
uses plaintext only when TLS is not advertised; an advertised TLS handshake
failure is an error, not a plaintext retry. `tls_ca_pem` accepts a single bounded
CA certificate. Authentication supports `mysql_native_password` and
`caching_sha2_password`, including TLS and bounded RSA full authentication.

Stop cancels waits and joins the owned worker/socket cleanup; cancellation or
timeout retains the cleanup obligation and prevents replacement. Failures keep
their original causes. There is no silent reconnect/reset-to-end fallback.

`new_with_progress_reader` also accepts an optional `SourceProgressReader`;
paired constructors accept either a local progress owner or that reader.
Local readers preserve actual-owner checks. External readers provide reads only,
not local ownership proof; pairing requires the same reader handle. Fast mode
does not construct a reader or subscription. This prepares the Rust boundary,
not the plugin protocol.

**Still incomplete:** native plugin ABI and Server registration. Starting an
unpaired source does not initialize existing rows. DDL affecting the selected database,
statement-based changes, XA, compressed/tagged transactions, partial JSON updates,
spatial/vector columns and opaque binary JSON extensions in live events are not
supported and fail explicitly. Initial JSON uses MySQL's textual representation;
that does not add support for opaque binary JSON replay.

### Opt-in coordinated initial loading

`native::MySqlSnapshot` is a separate native bootstrap provider, not a change to
the legacy MySQL bootstrapper. It requires a persistent atomic query, transaction
output, one of the replay policies above, and explicit permission to use
`FLUSH TABLES WITH READ LOCK`. Ordinary streaming never acquires this lock.

Given a native configuration, query and its actual progress resource:

```rust
use drasi_source_mysql::native::{MySqlOutput, MySqlRetention, MySqlSource, MySqlStartPosition};
use std::num::NonZeroU64;

config.output = MySqlOutput::Transactions;
config.replay = Some(MySqlRetention::UntilProcessed);
config.start = MySqlStartPosition::Snapshot {
    lock_timeout_ms: NonZeroU64::new(2000).unwrap(),
};
let (source, snapshot) =
    MySqlSource::coordinated(source_id, stream_id, config, progress.clone())?;
let query = query.with_bootstrap(snapshot);
```

The serialized start setting is
`{"mode":"snapshot","lock_timeout_ms":2000}`. The native factory also accepts a
typed `snapshot` bootstrap resource, paired with its `source_progress` resource.
Direct and factory construction require the same source settings and actual
progress object, not merely matching identifiers. Besides selected-table reads
and replication privileges, the account needs `RELOAD`; cleanup kills only its
own sessions and does not require permission to kill other users' connections.

The provider saves initialization intent in query-owned storage, opens a
repeatable-read view under the global lock, pins selected table definitions,
captures the binlog boundary, and **releases the global lock before scanning**.
Owned binary server-side cursors read one row at a time through a bounded channel.
Initial rows and live changes share types and typed identities. Client buffering
is bounded; MySQL may materialize server cursors internally. Ordinary writes can
continue during scanning, but selected-table DDL waits until the read transaction
ends.

`lock_timeout_ms` bounds Drasi's lock-establishment attempt and sets a server
lock-wait timeout. It is **not an unconditional application-pause bound**:
cancelling a waiting `FLUSH TABLES` can leave that table waiting for a pre-existing
reader to finish, even after every Drasi snapshot session is gone. Drasi never
kills that unrelated reader. Hard process loss also relies on MySQL's session
cleanup; the deadline is not an independent server-side lease.

After scanning and server-observed session retirement, the query atomically saves
the completed marker, handover metadata and source boundary. Initial loading is
row-wise, not one upstream transaction. Partial loading cannot be mistaken for
completion: strict recovery rejects it; an explicit `QueryRecoveryPolicy::AutoReset`
reloads a new view. Completed initialization resumes without rescanning.
Cancellation, timeout and connection failures retain the worker until awaited
cleanup finishes; replacement is blocked and earlier cleanup errors remain visible.

The boundary is verified against a bounded-memory hash of its binlog file prefix,
both at initialization and before admitting the first live transaction. Retain
that **entire anchor file and all later history** until an ordinary transaction
checkpoint replaces it. Large files can require a higher `io_timeout_ms`.
The lock timeout and byte limits may be raised for explicit retry without changing
data identity. Reused positions with different contents fail rather than silently
skip changes.

## Testing

Current-thread unit cases cover cancelled registration, subscriber waits,
failure/panic cleanup and a real TCP handshake blocked across cancelled and
timed-out stops:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test -p drasi-source-mysql --lib
```

The existing integration tests use testcontainers:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test -p drasi-source-mysql --test integration_test -- --ignored --test-threads=1
```

Native current-thread tests include direct and factory graph pipes, actual
RocksDB query reconstruction, process exits before/after query commit, whole
transaction limits, history loss/reuse, strict schema/value checks, malformed
packets and cancelled startup. Coordinated bootstrap cases include concurrent
writes, DDL exclusion, least-privileged factory construction, empty initialization,
snapshot-only restart after rotation, actual process exits during/after loading,
oversized rows, lock cancellation/timeouts, and retained cleanup while a real
server is paused. TLS tests generate short-lived certificates for real
CA/hostname acceptance and rejection:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test -p drasi-source-mysql --lib native -- --include-ignored --test-threads=1
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test -p drasi-mysql-common --lib
```

The process-exit helper is driven by its parent test; a standalone no-op helper
execution is not crash-recovery evidence.
