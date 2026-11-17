# Native MySQL source: configuration

Every factory takes one flat camelCase object. The shared fields below are
also accepted by the native MySQL bootstrap providers
([`drasi-computation-bootstrap-mysql`](../../bootstrappers/mysql)), which add
`source` and `stream`. Unknown fields are rejected. Design: [DESIGN.md](DESIGN.md).

```json
{"stream": "orders", "database": "shop", "user": "drasi",
 "password": {"secret": "mysql-password"}, "sslMode": "require_verify_full",
 "tables": ["orders"], "startFrom": "position",
 "startBinlogFile": "binlog.000042", "startBinlogPosition": 4}
```

## Factories and mode fields

| Field | Type | Default | Factories | Update |
|---|---|---|---|---|
| `stream` | string, required | | all | unsupported |
| `retention` | `server` \| `untilProcessed` | `server` | tracked: `transactions`, `shared-transactions`, `shared-changes`, `shared-positional-changes` | unsupported |
| `persistent` | boolean | `false` | `shared-changes`, `shared-positional-changes` | unsupported |

`transactions` and `shared-transactions` require `startFrom` `position` or
`snapshot`. `snapshot` is accepted only by `transactions`,
`shared-transactions` and `shared-changes`. Positional factories require
explicit `@N` `tableKeys`.

## Settings

| Field | Type | Default | Range | Secret | Update |
|---|---|---|---|---|---|
| `host` | string | `localhost` | | | restart |
| `port` | integer | `3306` | 1..65535 | | restart |
| `database` | string, required | | 1-64 chars, any identifier | | unsupported |
| `user` | string, required | | | | restart |
| `password` | string | `""` | no NUL | yes | restart |
| `sslMode` | `disabled`, `if_available`, `require`, `require_verify_ca`, `require_verify_full` | `if_available` | | | restart |
| `tlsCaPem` | PEM | none | ≤ 64 KiB | | restart |
| `tlsClientCertPem` | PEM | none | with `tlsClientKeyPem` | | restart |
| `tlsClientKeyPem` | PKCS#8 PEM | none | with `tlsClientCertPem` | yes | restart |
| `serverId` | integer | derived from the component ID | 1..2^32-1, unique among replicas | | restart |
| `heartbeatIntervalMs` | integer | `30000` | ≥ 1 | | restart |
| `tables` | array of names | `[]` (all supported base tables) | distinct, 1-64 chars | | unsupported |
| `tableKeys` | `[{table, keyColumns}]` | `[]` | | | unsupported |
| `startFrom` | `end`, `beginning`, `gtid`, `position`, `snapshot` | `end` | | | restart (untracked, or before the first checkpoint) |
| `startGtidSet` | string | none | `startFrom: gtid` | | as `startFrom` |
| `startBinlogFile` / `startBinlogPosition` | string / integer | none | `startFrom: position`, position ≥ 4 | | as `startFrom` |
| `snapshotLockTimeoutMs` | integer | `10000` | ≥ 1 | | restart |
| `maxTransactionChanges` | integer | `100000` | ≥ 1 | | restart (grow only) |
| `maxTransactionBytes` | integer | `67108864` | ≥ 1 | | restart (grow only) |
| `maxTransactionDurationMs` | integer | `300000` | ≥ 1 | | restart |
| `maxProtocolBytes` | integer | `16777216` | ≥ 256; also bounds the pause buffer | | restart |
| `ioTimeoutMs` | integer | `30000` | ≥ 1 | | restart |
| `reconnectBudgetMs` | integer | `300000` | 0..86400000; 0 disables | | in place |

An empty `tables` selection skips (with a warning) tables it cannot capture:
unsupported column types or character sets, no usable key, prefix primary
keys, and non-InnoDB tables for transaction output or snapshots. An explicit
selection fails instead.

## Server prerequisites

- MySQL 8.0.20 or newer (positional factories: 5.7.9). **MariaDB is not
  supported** and is rejected at the handshake.
- `log_bin` on, `binlog_format=ROW`, `binlog_row_image=FULL`,
  `binlog_checksum=CRC32`, and (except positional) `binlog_row_metadata=FULL`.
- `gtid_mode` is optional; it is used only for `startFrom: gtid`.
- `retention: untilProcessed` needs automatic expiry disabled
  (`binlog_expire_logs_seconds=0`, `expire_logs_days=0`, or
  `binlog_expire_logs_auto_purge=OFF`).
- Privileges: `REPLICATION SLAVE`, `REPLICATION CLIENT` and `SELECT` on the
  captured tables (catalog via `information_schema`). Coordinated bootstrap
  additionally needs `RELOAD` (FTWRL) and `performance_schema` connection
  attributes for its session cleanup.
- No `net_write_timeout` tuning is needed for long pauses: the source drains
  the socket while paused and reopens after a deliberate close.

## Credentials

`password` and `tlsClientKeyPem` must be secret references in host
definitions. With activation credentials the host supplies user and password
per activation (context: `host`, `port`); `password` may then be omitted.

## Updates

In place after a completed pause: `reconnectBudgetMs`. At Stop: connection,
credentials, TLS, `serverId`, `heartbeatIntervalMs`, `snapshotLockTimeoutMs`,
timeouts and limits (transaction limits may only grow). Start fields: restart
for untracked factories, or for tracked ones before the first checkpoint;
switching to or from `snapshot` is unsupported. Unsupported (remove with
erase and add again): `stream`, `database`, `tables`, `tableKeys`,
`retention`, `persistent`.

## Removal and erase

The source owns no server-side state. Erase is an idempotent no-op that
refuses while running. Binlogs are never purged by the source.

## Failures

| Failure | Meaning | Action |
|---|---|---|
| `SourceFailure::SchemaChanged` | A selected table was dropped/renamed/truncated, its keys or key types changed, a table joined a tracked database-wide selection, or the committed cursor has another binding (including every cursor written before binding v2) | Remove with erase and add again (new snapshot) |
| `SourceFailure::PositionUnavailable` | The resume binlog is purged, or the dump was refused (error 1236, also a duplicate `serverId`) | Restore history or re-create the source; make `serverId` unique |
| `SourceFailure::Unsupported` | Server settings, version, a different `@@server_uuid` after reconnect, statement-based DML on a selected table | Fix the server or configuration |
| "reconnect budget exhausted" | Transport failures outlasted `reconnectBudgetMs` | Check connectivity; Stop and Start |

## Operations

- Logs carry the component ID: "is streaming", "is reconnecting", "failed",
  skipped tables, adopted schema changes, pause buffer overflow.
- After a schema migration that only adds or changes non-key columns, nothing
  is needed. Key changes require erase and re-add.
- A failover to another server needs a new source (no GTID cursors).

## Migrating from the previous configuration

| Previous (`settings.*`) | Now |
|---|---|
| `settings` object (secret) | flat top-level fields; only `password` is secret |
| `connection.host/port/database/user/password` | `host`, `port`, `database`, `user`, `password` |
| `connection.ssl_mode`, `tls_ca_pem` | `sslMode`, `tlsCaPem` |
| `connection.server_id` (required) | `serverId` (optional) |
| `connection.heartbeat_interval_ms` | `heartbeatIntervalMs` |
| `table_keys: [{table, key_columns}]` | `tableKeys: [{table, keyColumns}]` |
| `start: {mode: end \| start \| gtid \| position \| snapshot}` | `startFrom: end \| beginning \| gtid \| position \| snapshot` with `startGtidSet`, `startBinlogFile`, `startBinlogPosition`, `snapshotLockTimeoutMs` |
| `output` | implied by the factory |
| `replay: server \| until_processed \| null` | `retention: server \| untilProcessed` (tracked factories only) |
| `transactions.max_changes/max_bytes/max_duration_ms` | `maxTransactionChanges`, `maxTransactionBytes`, `maxTransactionDurationMs` |
| `max_protocol_bytes`, `io_timeout_ms` | `maxProtocolBytes`, `ioTimeoutMs` |
| (none) | `reconnectBudgetMs`, `tlsClientCertPem`, `tlsClientKeyPem` |

Existing committed cursors use binding v1 and fail with `SchemaChanged`;
re-create those sources.
