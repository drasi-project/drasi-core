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

### Dependency advisory boundary

The October 2026 dependency review retains `rsa` 0.9 for public-key password
encryption; these connectors do not hold RSA private keys or call private-key
decryption/signing, the operations affected by `RUSTSEC-2023-0071`.

`mysql_common` 0.37's binlog feature still requires `zstd` 0.13.3 /
`zstd-safe` 7.3. Its transaction-payload decoding uses fresh **read-side**
decoders (`copy_decode` / `Decoder::with_buffer`), not borrowed/reused contexts
or write-side decoders. Those paths are outside `RUSTSEC-2026-0391` and
`RUSTSEC-2026-0403`; the latter advisory explicitly excludes read-side
decoders. Seekable, experimental and multithreaded zstd features are not
enabled, and these connectors do not train dictionaries or construct raw
input/output buffers. Native parsing continues to reject unsupported compressed
transaction events without advancing a checkpoint.

This is an exposure assessment, not an audit ignore or removal of compressed
legacy binlog support. Reassess before adding other zstd operations, and move
to a patched upstream binlog dependency when one is available. Other compression
users such as gRPC use the patched zstd 0.14.3 family.

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
Binlog filename suffixes are ordered numerically, so rotation from
`mysql-bin.999999` to `mysql-bin.1000000` preserves checkpoint and replay order.

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

## Lifecycle and reliability boundary

The source serializes start/stop and owns its replication task before spawning
it. Stop wakes subscriber, initial-boundary, idle-binlog and reconnect waits,
then joins the worker before clearing subscriptions. Submitted connection,
dispatch and state work is not aborted. A cancelled stop or five-second cleanup
timeout retains ownership and prevents replacement; retry stop after the pending
operation finishes. Worker errors and panics remain observable during cleanup.
Cleanup also runs for starting or failed sources. Once a failed worker has been
joined, stop still clears its subscriptions and resume filters before returning
the failure with status `Error`. Retry stop to acknowledge that failure and reach
`Stopped`; a failed stop never authorizes a replacement worker.

This repairs the Drasi-owned worker, not every driver-owned resource.
`mysql_async` 0.37.1 can detach connection cleanup on some failed connection or
binlog-registration paths, and its `BinlogStream::close` discards underlying
close failures. Awaiting the source worker therefore does **not** yet prove
complete driver cleanup. Full MySQL lifecycle qualification remains incomplete.

These lifecycle guarantees are separate from the checkpoint-position changes
described above. The legacy row stream does not provide whole-transaction query
visibility or exactly-once delivery.

## Native ComputationGraph source

The native ComputationGraph sources are the separate plugin
[`drasi-computation-source-mysql`](../../computation-plugins/sources/mysql/README.md),
with its paired bootstrap providers in
[`drasi-computation-bootstrap-mysql`](../../computation-plugins/bootstrappers/mysql).
Its factories declare their compatibility (output format, replay, progress and
snapshot coordination) and take one flat camelCase configuration
([CONFIGURATION.md](../../computation-plugins/sources/mysql/CONFIGURATION.md));
the design, including cursors, snapshot handover, retention and DDL policy, is
in [DESIGN.md](../../computation-plugins/sources/mysql/DESIGN.md). They share
the directly owned protocol, decoder, snapshot and replay engine in
`drasi-mysql-common` and do not wrap this legacy source. This crate exposes no
native source or factory of its own.

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

The live tests of the shared native engine and the plugin's factories live in
the plugin crate (see its
[README](../../computation-plugins/sources/mysql/README.md#qualification)).

