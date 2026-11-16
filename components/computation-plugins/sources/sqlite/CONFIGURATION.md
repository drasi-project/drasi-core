# Native SQLite source: configuration

Every factory takes one flat camelCase object. Unknown fields, nested
objects from the previous format and fields of another factory's mode are
rejected. Every bound has a default, so a minimal configuration is:

```json
{"stream": "sqlite/out", "path": "/var/lib/app/app.db", "tables": ["orders"]}
```

The published schema (`configuration_details`) lists each field with its
description, default and range; every published default equals the value the
factory fills in. Secrets are top-level fields and may be secret references
resolved by the Host.

## Factories and mode fields

| Factory | Mode fields |
|---|---|
| `drasi.sqlite/changes` | `stream` |
| `drasi.sqlite/transactions` | `stream` |
| `drasi.sqlite/replay-transactions` | `stream`, `coordinatedSnapshot`, `maxJournalTransactions`, `maxJournalBytes` |
| `drasi.sqlite/shared-transactions` | `stream`, `coordinatedSnapshot`, `maxJournalTransactions`, `maxJournalBytes` |

The journal factories need the Host's progress binding
(`source_progress` for replay, `consumer_progress` for shared) and a `path`.

| Field | Type | Default | Range | Update |
|---|---|---|---|---|
| `stream` | string, required | — | stream identifier | unsupported |
| `coordinatedSnapshot` | boolean | `false` | — | unsupported |
| `maxJournalTransactions` | integer | `10000` | ≥ 1 | restart, increase only |
| `maxJournalBytes` | integer | `268435456` (256 MiB) | ≥ 1 | restart, increase only |

`coordinatedSnapshot: true` initializes each query from a snapshot taken by
the paired provider (`drasi.sqlite/coordinated-bootstrap` for replay,
`drasi.sqlite/shared-bootstrap` for shared) before live writes are admitted.
It is required when the selected tables already contain rows: a journal
source refuses to adopt existing rows silently.

## Database and capture

| Field | Type | Default | Range | Update |
|---|---|---|---|---|
| `path` | string | in-memory database | nonempty | unsupported |
| `tables` | array of strings | `[]` (all ordinary tables) | no `__drasi_` names, no duplicates | unsupported |
| `tableKeys` | array of `{"table", "keyColumns"}` | `[]` | tables must be selected | unsupported |
| `maxTransactionChanges` | integer | `10000` | ≥ 1 | restart (journal: increase only) |
| `maxTransactionBytes` | integer | `16777216` (16 MiB) | 1..=2147483647 | restart (journal: increase only) |
| `maxTransactionDurationMs` | integer | `30000` | 1..=2147483647 | restart |
| `maxSqlBytes` | integer | `1048576` (1 MiB) | 1..=2147483647 | restart |
| `commandCapacity` | integer | `64` | 2..=1048576 | restart |
| `outputCapacity` | integer | `16` | 1..=1048576 | restart |
| `shutdownTimeoutMs` | integer | `10000` | ≥ 1 | restart |
| `walMode` | boolean | `false` | file, non-journal factory | restart |
| `initSql` | string | none | ≤ `maxSqlBytes` | restart |

- `path`: the file is opened read-write and created if missing; its directory
  must exist. Omit `path` for a private in-memory database that starts empty
  on every start; the factories then require `initSql` or `sqlApi`. Do not
  send `path: null`: the Host rejects it as a type error.
- `maxTransactionChanges` and `maxTransactionBytes` bound one captured
  transaction **and** one read result (rows and encoded bytes). A larger
  write rolls back with an error. `maxTransactionBytes` is also SQLite's
  maximum value length.
- `maxTransactionDurationMs` bounds one transaction or read and is SQLite's
  busy timeout.
- `shutdownTimeoutMs` bounds Pause, Stop and erase only when the Host gives
  no deadline.
- `initSql` runs on every connection the source opens, before its authorizer:
  see [DESIGN.md](DESIGN.md#initsql). Make it idempotent for a file database.

## REST API

| Field | Type | Default | Range | Secret | Update |
|---|---|---|---|---|---|
| `restApi` | boolean | `false` | — | | restart |
| `restHost` | string | `"127.0.0.1"` | literal IP | | restart |
| `restPort` | integer | `8080` | 0..=65535 (0: any free port) | | restart |
| `authToken` | string | none | 16..=4096 visible ASCII | yes | in place (rotation); adding or removing restarts |
| `authTokenSecondary` | string | none | as `authToken`, differs from it | yes | in place |
| `allowUnauthenticated` | boolean | `false` | — | | restart |
| `tlsCertificatePem` | string | none | PEM chain | | restart |
| `tlsPrivateKeyPem` | string | none | PEM key matching the chain | yes | restart |
| `tlsClientCaPem` | string | none | PEM CA | | restart |
| `maxRequestBytes` | integer | `1048576` (1 MiB) | 1..=67108864 | | restart |
| `requestTimeoutMs` | integer | `30000` | ≥ 1 | | restart |
| `maxConnections` | integer | `128` | 1..=65536 | | restart |
| `pageSize` | integer | `1000` | 1..=`maxTransactionChanges` | | restart |
| `sqlApi` | boolean | `false` | needs `restApi` and `authToken` | | restart |

The REST fields are validated even while `restApi` is false.

- A non-loopback `restHost` requires `authToken`, or
  `allowUnauthenticated: true` behind an authenticating proxy.
- `authTokenSecondary` requires `authToken`. Rotate by adding the new token as
  secondary, switching clients, then promoting it: both steps apply in place.
- TLS needs both `tlsCertificatePem` and `tlsPrivateKeyPem`; `tlsClientCaPem`
  additionally requires client certificates.

### Routes

| Route | Method | Notes |
|---|---|---|
| `/health` | GET | never authenticated |
| `/api/tables` | GET | selected application tables |
| `/api/tables/{table}` | GET | page: `?limit=&offset=`, ordered by key; `X-Next-Offset` when more rows exist |
| `/api/tables/{table}` | POST | insert one row (JSON object) |
| `/api/tables/{table}/{id}` | GET, PUT, DELETE | `id` is the key value; composite keys joined with `:`; rowid tables use the rowid |
| `/api/batch` | POST | `{"operations": [{"op": "insert"\|"update"\|"delete", …}]}` in one transaction |
| `/api/sql` | POST | only with `sqlApi`; `{"execute": [{"sql", "params"}]}` or `{"query": {"sql", "params"}}` |

| Status | Meaning |
|---|---|
| 200 | committed (writes) or read |
| 400 | invalid table, column, key or body; `/api/sql` SQL or authorizer error (message in `{"error"}`) |
| 401 | missing or wrong bearer token |
| 404 | row or route not found (`/api/sql` when disabled) |
| 413 | body larger than `maxRequestBytes` |
| 500 | SQL failure (constraint violation, limit exceeded); nothing committed |
| 503 | paused (with `Retry-After: 1`) or stopping |
| 504 | `requestTimeoutMs` expired; a submitted commit may still complete |

A 200 confirms the local SQLite commit, not downstream query processing.

## Examples

Volatile changes with an authenticated REST API on all interfaces:

```json
{
  "stream": "inventory",
  "path": "/data/inventory.db",
  "tables": ["items"],
  "restApi": true,
  "restHost": "0.0.0.0",
  "restPort": 8081,
  "authToken": {"secret": "sqlite-rest-token"}
}
```

In-memory source that creates its table and accepts SQL:

```json
{
  "stream": "scratch",
  "initSql": "CREATE TABLE events(id INTEGER PRIMARY KEY, kind TEXT NOT NULL)",
  "restApi": true,
  "restPort": 0,
  "authToken": {"secret": "sqlite-rest-token"},
  "sqlApi": true
}
```

Coordinated replay for one persistent query (pair with
`drasi.sqlite/coordinated-bootstrap` configured with `{"source", "stream"}`):

```json
{
  "stream": "orders",
  "path": "/data/shop.db",
  "tables": ["orders", "lines"],
  "tableKeys": [{"table": "lines", "keyColumns": ["order_id", "line"]}],
  "coordinatedSnapshot": true,
  "maxJournalTransactions": 50000,
  "restApi": true
}
```

Shared journal for a fixed set of queries: the same fields on
`drasi.sqlite/shared-transactions`, with `drasi.sqlite/shared-bootstrap`
providers when `coordinatedSnapshot` is true.

## Prerequisites

- SQLite is bundled; no system library is needed.
- The process needs read-write access to the file and its directory (SQLite
  creates journal files beside it).
- Journal factories take an exclusive lock: no other process may open the
  file while the source runs. With `walMode`, other processes may read.
- Transaction factories need a transaction-enabled query
  (`source_transactions`); journal factories need persistent query storage.

## Updates

See [DESIGN.md](DESIGN.md#management) for the classification. In short:
token rotation applies in place after a Pause; every other supported change
applies at a restart; identity fields (`stream`, `path`, `tables`,
`tableKeys`, `coordinatedSnapshot`), journal limit decreases and any change to
a coordinated source are unsupported.

## Removal and erase

Removing a journal source with erase drops exactly its four
`__drasi_native_*_v1` tables in one transaction; user tables and the file
stay. Erase is idempotent (a missing file or journal is success) and leaves a
journal that belongs to another source untouched. If the database is locked,
erase fails retryably and changes nothing. Volatile sources own nothing; their
erase succeeds without effect.

After erase, any factory can start on the same file. Without erase, a
non-journal source on that file fails with "cannot disable replay on a
journal-owned database".

## Operations

| Symptom | Cause and action |
|---|---|
| `an in-memory SQLite source (no path) needs initSql or sqlApi` | Set `path`, or add `initSql`/`sqlApi`. |
| `restHost … is not a loopback address` | Add `authToken` or `allowUnauthenticated: true`. |
| `cannot disable replay on a journal-owned database` | The file still has a journal; remove the old journal source with erase. |
| `SchemaChanged: the SQLite schema changed outside the source` | The file was altered externally. Remove the source with erase and add it again (with `coordinatedSnapshot` if the tables have rows). |
| `journal belongs to a different source, stream, consumer or table selection` | An identity field changed, or another source used the file. Remove with erase, or restore the configuration. |
| `replay cannot adopt existing rows without coordinated bootstrap` | Set `coordinatedSnapshot: true` and pair the bootstrap provider. |
| `retained journal exceeds configured limits` | A journal limit was lowered externally; raise it back. |
| Writes return 503 with `Retry-After` | The source is paused; retry after resume. |
| Writes fail with a limit error | Raise `maxTransactionChanges`/`maxTransactionBytes`, or split the write. |
| Stop returns "did not drain within its deadline" | An HTTP request or open transaction is still running; Stop again (or Stop with Abort). |
| Erase fails with "could not lock the database" | Another connection holds the file; close it and retry. |

## Migrating from the previous configuration

| Previous (nested) | Now (flat) |
|---|---|
| `settings.path` | `path` (omit for in-memory) |
| `settings.tables` | `tables` |
| `settings.table_keys[].key_columns` | `tableKeys[].keyColumns` |
| `settings.output` | implied by the factory; removed |
| `settings.replay.max_transactions` / `max_bytes` | `maxJournalTransactions` / `maxJournalBytes` (journal factories) |
| `settings.transactions.max_changes` / `max_bytes` / `max_duration_ms` | `maxTransactionChanges` / `maxTransactionBytes` / `maxTransactionDurationMs` |
| `settings.max_sql_bytes` | `maxSqlBytes` |
| `settings.command_capacity` / `output_capacity` | `commandCapacity` / `outputCapacity` |
| `settings.shutdown_timeout_ms` | `shutdownTimeoutMs` |
| `rest_api: {"host", "port"}` | `restApi: true`, `restHost` (literal IP), `restPort` |
| `coordinated_snapshot` | `coordinatedSnapshot` |
| — | new: `walMode`, `initSql`, `authToken`, `authTokenSecondary`, `allowUnauthenticated`, `tls*`, `maxRequestBytes`, `requestTimeoutMs`, `maxConnections`, `pageSize`, `sqlApi` |

Persisted journals, positions and element IDs are unchanged, so a journal
created before the change is adopted when the identity fields are the same.
