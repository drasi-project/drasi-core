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

## Checkpoint positions and resuming

Each emitted change carries an opaque position token used to resume replication
after a restart. Nonfinal rows in a transaction are checkpointed individually,
including across multiple statements. In the default `Channel` dispatch mode,
partial resume replays that transaction and suppresses rows already processed by
each subscriber, without skipping its remaining rows. Partial-row checkpoints
are rejected in `Broadcast` mode, which cannot filter replay per subscriber;
live and completed-boundary subscriptions retain their existing behavior.

The final emitted row, including the only row in a single-row transaction, uses
a completed boundary token, as do bootstrap boundaries. That token represents a
position after all rows at the cursor: resume uses its GTID when present or its
commit-end file position, rather than rewinding to the transaction start.
The token's `transaction_start_position` and `row_offset` fields are internal,
not source configuration options.

**Partial-row resume requires the same binlog history**, even with GTID enabled.
The source logs a warning when a partial token includes a GTID and uses the
transaction's file position: requesting its already-executed GTID would skip the
remaining rows. Partial checkpoints are not portable across primary failover or
binlog renaming. Those changes, or changing the source table/key configuration,
require a fresh bootstrap/checkpoint. GTID-based whole-transaction start positions
remain supported; they do not make partial-row checkpoints failover-safe.

Incomplete, invalid, or oversized tokens fail explicitly. A boundary must contain
a nonblank GTID or a nonempty binlog filename with a position of at least byte 4;
an unusable checkpoint never falls back to the configured start position.
Encoded tokens must be smaller than 64 KiB; binlog filenames are limited to 255
bytes and GTID sets to 60 KiB. Detailed decode errors are logged rather than
returned to subscribers.
Malformed transaction boundaries fail replication with bounded reconnect attempts
and an Error status, rather than silently discarding rows or skipping ahead.

Rolled-back transactional rows are not written to MySQL's row-format binlog.
Nontransactional changes (such as MyISAM writes) cannot be rolled back and are
still delivered if MySQL logs them in a transaction ending with `ROLLBACK`.

Rows lost with an older source are not reconstructed automatically, and older
source binaries cannot safely resume the new partial-row tokens.

## Lifecycle

`stop()` cleans up running, starting, and failed sources. It aborts and joins the
replication task, closes stale streaming subscriptions, and clears their resume
positions before reporting `Stopped`. Repeated direct stops are safe; new
subscriptions are required after a restart to supply fresh resume positions.
Both database sources use `SourceBase::abort_and_join_task()`: expected task
cancellation succeeds, while an unexpected task failure is logged and returned
after cleanup, leaving status `Error`. A subsequent stop can then reach `Stopped`.
Task joining is not timed out, so an old task cannot outlive cleanup or overlap a
restart; replication tasks must remain cooperative async tasks.

## Testing

Integration test uses testcontainers:

```bash
cargo test -p drasi-source-mysql -- --ignored --nocapture
```
