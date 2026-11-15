# Native PostgreSQL source: configuration

Every factory takes one flat camelCase object: the mode fields, plus the
shared settings below. The coordinated and shared bootstrap providers in
[`drasi-computation-bootstrap-postgres`](../../bootstrappers/postgres) take the
same settings beside their `source` and `stream`, so the source and its
provider compute one binding and one managed slot identity.

```json
{
  "stream": "orders",
  "host": "db.internal",
  "database": "shop",
  "user": "drasi",
  "password": {"secret": "postgres-password"},
  "sslMode": "require",
  "publication": "drasi_orders",
  "tables": ["public.orders", "public.order_lines"]
}
```

`password` is the only secret field. A host accepts only a secret reference
for it, never a literal. It may be omitted when the host supplies activation
credentials (username and password or token), which then replace `user` and
`password` for that activation.

## Factories and mode fields

| Factory | Output | Progress binding | Mode fields |
|---|---|---|---|
| `drasi/postgres-transactions` | `SourceTransactionCodec`: whole commits | `source_progress` (atomic persistent query) | `coordinatedSnapshot` |
| `drasi.postgres/changes` | `GraphChangeCodec`: one envelope per row change | `source_progress` | `coordinatedSnapshot`, `persistent` |
| `drasi.postgres/shared-changes` | `GraphChangeCodec` | `consumer_progress` (fixed consumer set) | `persistent` |
| `drasi.postgres/shared-transactions` | `SourceTransactionCodec` | `consumer_progress` (atomic persistent queries) | none |

| Field | Type | Default | Notes | Update |
|---|---|---|---|---|
| `stream` | string | required | Output stream identifier on every change. | unsupported |
| `coordinatedSnapshot` | boolean | `false` | Start from a snapshot taken by the paired coordinated provider, which creates and hands over a managed slot. Requires managed slot ownership and no `startLsn`; row mode also requires `persistent`. | unsupported |
| `persistent` | boolean | `false` | Rows only. Require persistent consumer progress so committed rows survive a process restart. | unsupported |

## Settings

| Field | Type | Default | Range and notes | Secret | Update |
|---|---|---|---|---|---|
| `host` | string | `localhost` | Host name or address; TLS verifies it. | | Restart |
| `port` | integer | `5432` | 1..=65535. | | Restart |
| `database` | string | required | At most 63 bytes. Must equal the server's reported database. | | unsupported |
| `user` | string | required | Needs `REPLICATION` and `SELECT` on published tables; managed slots also need slot create/drop rights. Replaced by activation credentials. | | Restart |
| `password` | string | empty | Secret reference. Replaced by activation credentials. | yes | Restart |
| `sslMode` | string | `prefer` | `disable`, `prefer` or `require`. TLS always verifies the certificate and host name; `prefer` falls back to plaintext only when the server refuses TLS, never after a failed handshake. | | Restart |
| `tlsCaPem` | string | none | Extra trusted CA certificates (PEM), at most 64 KiB and within `maxProtocolBytes`. | | Restart |
| `publication` | string | required | Must publish `insert, update, delete` and must not publish `truncate`. At most 63 bytes. | | unsupported |
| `tables` | array of strings | `[]` | `schema.table` (`public` may be omitted), each in the publication. Empty emits every publication table. | | unsupported |
| `tableKeys` | array | `[]` | `{"table": "schema.table", "keyColumns": ["a", "b"]}` for tables without a usable primary key. Columns nonempty and unique; the replica identity must carry them. | | unsupported |
| `slotOwnership` | string | `managed` | `managed` or `external`. See [Slots](#slots). | | unsupported |
| `slotName` | string | none | Required for `external`, rejected for `managed`. 1 to 63 lowercase letters, digits or underscores. | | unsupported |
| `startLsn` | string | none | Initial cursor such as `0/16B3748`, used only before the first checkpoint. When absent, streaming starts at the slot's confirmed position. Not allowed with `coordinatedSnapshot`. | | Restart before the first checkpoint, otherwise unsupported |
| `maxTransactionChanges` | integer | `100000` | ≥ 1. A larger transaction fails without publishing a prefix and is retried after the limit grows. | | Restart (increase only) |
| `maxTransactionBytes` | integer | `67108864` | ≥ 1. Encoded transaction size. | | Restart (increase only) |
| `maxTransactionDurationMs` | integer | `300000` | ≥ 1. Assembly and publish deadline per transaction. | | Restart |
| `maxProtocolBytes` | integer | `16777216` | ≥ 64. Largest protocol message, catalog result or relation metadata. | | Restart |
| `ioTimeoutMs` | integer | `30000` | ≥ 1. Connect, catalog query, feedback write and start budget (bounded further by the host's start deadline). | | Restart |
| `feedbackIntervalMs` | integer | `10000` | ≥ 1. Standby status interval. Keep it below the server's `wal_sender_timeout` (60 s by default); a paused source relies on it to keep its session. | | Restart |
| `reconnectBudgetMs` | integer | `300000` | 0..=86400000. Total time to keep reconnecting with jittered exponential backoff (250 ms doubling to 30 s) after a transport failure. `0` disables reconnection. | | InPlace |

Unknown fields are rejected, including the removed fields listed in
[Migrating from the previous configuration](#migrating-from-the-previous-configuration).

## Slots

**Managed** (default). The source owns its slot. Its name is derived from the
graph and component identifiers: `drasi_<24 hex>_<32 hex>`, the first part
being this source's prefix. On first start, if no checkpoint exists and the
slot is absent, the source creates it. A coordinated provider creates its own
`<prefix>_<uuid>` slots with the same prefix and hands one over through the
committed cursor. Erasing removal drops every slot with this source's prefix in
the database, and refuses while any of them is active. A slot that disappears
after a checkpoint is never recreated: the source fails with
`PositionUnavailable`.

**External.** An operator creates `slotName` (a persistent `pgoutput` slot in
the configured database) and removes it. The source never creates or drops it,
including on erasing removal. Use external ownership when one slot must
outlive the source, or when the role cannot create slots.

```sql
SELECT pg_create_logical_replication_slot('drasi_orders', 'pgoutput');
```

## Server prerequisites

| Requirement | Why |
|---|---|
| PostgreSQL 13 or later (15 or later for coordinated snapshots) | `wal_status`, `max_slot_wal_keep_size`; snapshot export rules. Older servers are refused with an `Unsupported` error. |
| `wal_level = logical` | Logical decoding. |
| `max_replication_slots` and `max_wal_senders` with headroom | One slot and one sender per source, plus one slot and session per running snapshot. |
| A publication: `CREATE PUBLICATION drasi_orders FOR TABLE orders, order_lines WITH (publish = 'insert, update, delete');` | TRUNCATE is refused (decision D-9): it cannot be represented as row changes. |
| A primary key, or a `tableKeys` entry whose columns the replica identity carries (`REPLICA IDENTITY FULL` or a `USING INDEX` identity) | Stable element identity. Tables without a usable key are rejected (decision D-10). |
| `max_slot_wal_keep_size = -1` (recommended) | A finite value is allowed (decision D-8); the source logs a warning, and if the slot falls behind and is invalidated (`wal_status = lost`) it fails with `PositionUnavailable`. |
| `wal_sender_timeout` above `feedbackIntervalMs` | Keeps idle and paused sessions alive. |

Monitor `pg_replication_slots` (`active`, `wal_status`, `safe_wal_size`, and
`pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn)`) for every source
slot: a stopped or stalled source retains WAL.

## Updates

The factory and the live object assess updates field by field (see the Update
column). InPlace changes need a completed pause; Restart changes are applied
while stopped and take effect at the next start, which reopens replication at
the committed cursor and refuses a different server or database through the
binding check. Fields marked unsupported are part of what the committed cursor
is bound to: remove the source with erase and add it again, which rebuilds
downstream queries from a new snapshot. The source deliberately does not
claim a Recreate plan, because it cannot itself coordinate the downstream
rebuild a new binding needs.

## Removal and erase

Removal without erase keeps every slot. Removal with erase (dispose with
`StateIntent::Erase`) drops all managed slots of this source and is safe to
repeat; it fails with `SlotActive` while another session uses one of them, so
stop every user first. External slots are never dropped; remove them yourself
when no longer needed, or they retain WAL indefinitely.

## Failures

| Error | Meaning | Action |
|---|---|---|
| `SchemaChanged` | A selected table's key columns, key column types or replica identity, the publication's operations or row filters, the slot, or the table set (for an all-tables publication) changed. Adding, dropping or retyping a **non-key** column is not a schema change (decision D-7). | Remove the source with erase and add it again. |
| `PositionUnavailable` | The slot is missing, invalidated, or no longer retains the committed cursor. | Remove with erase and re-add (a new snapshot), or restore the slot. |
| `Unsupported` | Server older than 13, publication missing or publishing TRUNCATE, a slot that is not a persistent `pgoutput` slot. | Fix the server or publication. |
| `SlotActive` | Erase found an owned slot in use. | Stop the other session; retry. |

Transport failures (connection reset, timeouts, server shutdown and
connection SQLSTATEs) reconnect within `reconnectBudgetMs` in every mode.
Permanent failures never reconnect: the source reports them through `next`,
and the host stops it.

## Operations

- **Pause** stops reading and emitting at the next transaction boundary; an
  open transaction is assembled first and held. Feedback continues, so a pause
  longer than `wal_sender_timeout` keeps its session.
- **Stop** with `Drain` sends final feedback at the committed cursor and closes
  the protocol cleanly; `Abort` drops the connection. Both honour the host
  deadline and keep ownership if it expires.
- **Schema discovery** describes the selected publication tables with the
  labels the source emits (`table` in `public`, otherwise `schema.table`) and
  property types matching the emitted values (dates, times and JSON as
  strings).

## Migrating from the previous configuration

This is a breaking change with no automatic migration.

| Previous | Now |
|---|---|
| `settings: {connection: {...}, ...}` nested, whole object secret | Flat fields; only `password` is secret. |
| `connection.publication_name` | `publication` (no default; was `drasi_publication`) |
| `connection.slot_name` (default `drasi_slot`) | `slotOwnership` + `slotName`; managed slots are named by the source. |
| `connection.ssl_mode`, `table_keys[].key_columns` | `sslMode`, `tableKeys[].keyColumns` |
| `start_lsn` (required) | `startLsn` (optional) |
| `transactions: {max_changes, max_bytes, max_duration_ms}` | `maxTransactionChanges`, `maxTransactionBytes`, `maxTransactionDurationMs` (defaults added) |
| `max_protocol_bytes`, `io_timeout_ms`, `feedback_interval_ms` (required) | camelCase, with defaults |
| `coordinated_snapshot` | `coordinatedSnapshot` |
| `auto_create_slot`, `start_from_slot` | Removed: managed slots are created when absent before the first checkpoint, and streaming starts at the slot's confirmed position unless `startLsn` is set. |
| `reconnect_attempts`, `reconnect_delay_ms` (rows only) | `reconnectBudgetMs`, every mode. |

Committed cursors from the previous release do not match the new binding (it
now covers only key columns) and are reported as `SchemaChanged`; remove and
re-add existing sources.
