# Native MySQL sources

This library hosts MySQL replication inside the calling DrasiLib instance's one
ComputationGraph. It reuses the directly owned protocol, TLS, catalog, decoder,
key/value and recovery implementation in `drasi-mysql-common`; it does not wrap
the legacy source's queue or open a `mysql_async` pool.

| Factory | Output and ownership |
|---|---|
| `drasi.mysql/changes` | Untracked ordinary changes. `output: changes`, `replay: null`; no snapshot or recovery claim. |
| `drasi.mysql/untracked-transactions` | Whole transaction envelopes without a recovery owner or replay claim. |
| `drasi.mysql/transactions` | Whole transactions with one actual `source_progress` owner. |
| `drasi.mysql/shared-transactions` | Whole transactions with an actual `consumer_progress` set. |
| `drasi.mysql/shared-changes` | Ordinary rows with an actual `consumer_progress` set; supports memory queries and persistent consumers. |
| `drasi.mysql/positional-changes` | Explicit untracked `@N` column projection for MySQL 5.7/limited-metadata binlogs. |
| `drasi.mysql/shared-positional-changes` | The same positional rows with actual fixed consumers, partial-row cursors and optional required persistence. |

All factories have implementation/configuration version `1`. `stream` and
`settings` are required. Settings use the existing `MySqlConfig` structure
documented in [the MySQL source](../../../sources/mysql/README.md), not a second
connection implementation. `settings` must be a secret configuration reference
in a host definition. Both shared-row factories additionally accept `persistent`
(default `false`); `true` requires persistence from every actual consumer.

An empty `settings.tables` selects the database's current base-table catalog at
startup. Untracked ordinary-row mode also discovers newly created tables when
their first row event arrives. Tracked and whole-transaction modes fence a
changed database-wide selection instead of changing an existing recovery
binding. `settings.table_keys` accepts `{table, key_columns}` entries to choose
the graph key explicitly. Otherwise the database primary key is used, with an
`id` column fallback when no primary key exists. Key columns may be declared
nullable, but actual key values must remain non-null and deterministic; changing
key selection changes the recovery binding rather than silently reinterpreting
retained data.

The resolved `settings` shape for a coordinated ordinary-row source is:

```json
{
  "connection": {
    "host": "mysql.example",
    "port": 3306,
    "database": "application",
    "user": "replication_user",
    "password": "resolved-by-the-host",
    "ssl_mode": "require_verify_full",
    "tls_ca_pem": null,
    "server_id": 1201,
    "heartbeat_interval_ms": 1000
  },
  "tables": ["items"],
  "start": {"mode": "snapshot", "lock_timeout_ms": 5000},
  "output": "changes",
  "replay": "server",
  "transactions": {
    "max_changes": 4096,
    "max_bytes": 8388608,
    "max_duration_ms": 30000
  },
  "max_protocol_bytes": 8388608,
  "io_timeout_ms": 10000
}
```

This is the resolved object, not a literal to place in the canonical graph.
Use a configuration resource for the credentials. The same resolved object goes
to each query's matching bootstrap provider. For transaction factories select
`output: transactions` and a query backend that actually supports complete
atomic output; ordinary in-memory indexes do not provide that capability.

Tracked rows use `output: changes` and explicit `replay: server` or
`replay: until_processed`. Tracked transaction factories require transaction
output. The latter retention policy verifies that automatic binlog expiry is
disabled; no mode changes retention settings or purges history.

Untracked factories also accept `start: {mode: start}` for the earliest retained
binlog and `start: {mode: gtid, set: "server-uuid:1-42"}` for MySQL GTID selection.
The GTID request uses the same `mysql_common` SID parser and protocol encoder as
the reference driver. It does not turn existing file-bound native checkpoints
or partial rows into portable failover cursors.

For initial data, select `start: {mode: snapshot, lock_timeout_ms: 5000}` and bind
the matching native bootstrap provider to each real query. The query publishes
its cursor only after complete initialization. Independent source/bootstrap
libraries exchange that actual committed progress, not a Rust object across
the ABI or an invented aggregate checkpoint.

## MySQL 5.7 and limited metadata

The positional factories support MySQL 5.7.9 or newer without pretending that
the binlog supplied FULL column metadata. Select `output: changes`, a non-snapshot
start, and explicit offset keys such as
`table_keys: [{table: "items", key_columns: ["@0"]}]`. Properties are projected by
column ordinal: query them with names such as ``n.`@0` `` and ``n.`@1` ``.
There is no implicit `id` fallback in this profile.

The same owned protocol, decoder and row-progress machinery are used. Captured
real catalog hints provide bounded text conversion and unsigned/binary value
interpretation, without rewriting TABLE_MAP metadata. Schema inspection exposes
the positional names and describes their captured physical columns. Limited
metadata does not supply ENUM/SET labels. Element time is processing time, not
a fabricated server commit clock. Keys retain the shared typed native identity
format; older legacy string IDs and checkpoint tokens are not reinterpreted.

Historical starts require a matching catalog profile. Before admitting rows,
the source scans the requested retained history for catalog-changing statements
and checks that catalog capture remained stable. GTID starts conservatively
check retained files, including events excluded by the GTID set. After a schema
migration, use an explicit known matching-schema start instead of decoding old
rows with current charset/signedness hints. Selected-data DDL must be binlogged;
unlogged schema changes are outside this profile's contract. Live schema changes
fence the source, including qualified changes issued from another database.

Tracked positional rows use a distinct binding namespace over the actual
catalog, keys and owner scope. They preserve real partial-row replay and
persistent reconstruction, but do not claim whole-transaction query visibility,
coordinated snapshots or completed bootstrap state. Initial loading through
`start: {mode: start}` is ordinary historical row delivery, including its
downstream effects. The existing FULL-metadata and coordinated factories keep
their stronger requirements unchanged.

## Row recovery

Each row cursor contains its exact transaction file/start/end and content hash,
row offset/count, and logical allocation counter. This is a separate format
from the existing native transaction cursor and from legacy row tokens.
The last row represents the completed boundary; partial rows resume from the
transaction start. File suffixes are compared numerically, not lexically.

Minimum progress is selected by physical binlog coverage, never by unrelated
consumer counters. History is checked for all members, including non-minimum
members. Source-specific position filtering suppresses covered rows queued or
replayed with newer counters while transport observations advance. Allocation
stays above all relevant consumer counters and the source's retained allocation
floor. Missing, failed, reset or unready members fence replication; they are not
omitted. An all-fresh streaming-only group differs from a partially initialized
group, which cannot silently begin at `start: end`.

Row streaming supports InnoDB, MyISAM and MEMORY. MySQL omits rolled-back
transactional rows from its binlog, but logged nontransactional writes ending in `ROLLBACK`
are still delivered. Coordinated snapshots and whole-transaction factories
require InnoDB; they do not silently claim an atomic snapshot of MyISAM data.

Stop and join before rewind, restart or membership replacement. Retain the
checkpoint transaction's file (or the entire anchor prefix for a snapshot)
and later history. Reused history, selected-table DDL and unsupported changes
fail explicitly. Partial cursors do not imply GTID failover portability.

## Build and host

```sh
cargo build -p drasi-computation-source-mysql --features dynamic-plugin
```

Static hosting uses `NativePlugin::from_definition(plugin()?)`; dynamic hosting
loads the separately built library. Both use the same factories and graph
owner. Rebuild the actual library after SDK/common changes; a path dependency
alone does not rebuild a loaded artifact.

`make test-native-mysql` builds all three matching libraries and runs the native
Host matrix with fresh Podman databases. It exercises static and dynamic
hosting on both Tokio runtimes, including real queries and procedure effects,
partial-row and whole-transaction fan-out, persistent reconstruction and owned
operation cleanup. It does not use or reset installed demo state.

The source exposes the shared native `SourceSchemaReader` over its actual
captured catalog, independently of a pending `next` operation. Construction
performs no discovery I/O; reads return no schema before catalog acquisition,
and retained readers reject retired owners. Completed stop clears the captured
catalog; restart captures the new worker's actual catalog. Live database-wide
row discovery publishes newly validated tables without waiting for `next`.
Integer/float hints and string/byte
representations reflect the shared decoder (including JSON and temporal values
encoded as strings). Unsigned BIGINT has no single type hint because its values
can be integers or lossless decimal strings. This source-data schema is distinct
from factory config.

The FULL-metadata factories require MySQL 8.0.20 or newer (not MariaDB),
non-null graph key values, supported UTF-8/ASCII/binary column
types, `ROW`/`FULL` binlogs and `CRC32` checksums. Configured GTID starts, manual
table-key overrides and standalone initialization use their explicit native
settings/factory; they do not reinterpret legacy cursor or snapshot records.
Positional factories have the separately declared MySQL 5.7/limited-metadata
contract above; they are not an automatic downgrade of those requirements.
