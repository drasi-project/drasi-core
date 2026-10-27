# Native SQL Server CDC source

**User-approved deferral: SQL Server qualification is parked and explicitly
incomplete. No further local or remote database attempts are authorized.**

`drasi-computation-source-mssql` exports `drasi.mssql/changes`, using computation
ABI **1.0.0**, wire **1**. Static `plugin()` and the `dynamic-plugin` entrypoint
construct the same implementation. Dependencies contain no query runtime,
parser, legacy SDK or source/reaction adapter.

**Status: implementation and unit/ABI checks available; real database
qualification is blocked, not complete.** On the current ARM64 Podman machine,
SQL Server 2022 CU22 amd64 exits 139 in the supported x86-64 emulator, including
the isolated CPU/seccomp variant. SQL Server 2019 CU32 stays alive but never
accepts SQL connections. No ARM skip or Azure SQL Edge substitutes for this gate.

## Configuration and ownership

```json
{
  "stream": "items/out",
  "settings": {
    "connection": {
      "host": "localhost", "port": 1433, "database": "example",
      "user": "application", "password": "resolve-through-Host-secrets",
      "tables": ["dbo.Items"], "poll_interval_ms": 1000,
      "encryption": "on", "trust_server_certificate": false,
      "start_position": "current"
    },
    "max_rows": 100000, "max_bytes": 67108864,
    "operation_timeout_ms": 30000, "shutdown_timeout_ms": 30000
  }
}
```

The `settings` field is secret-bearing: serialized Server definitions must use a
Host configuration reference. Actual instance configuration preserves every
field, including credentials. Construction opens no network connection.

Bind the required `consumer_progress` dependency to the actual fixed set of
query-owned progress resources. There is **one CDC producer**, not a producer per
query. Consumers retain their independent real CDC positions. Logical and
transport counters allocate envelopes only; neither becomes a database cursor.
The source waits for every member's readiness and fails on missing/mixed,
regressing, reset or failed consumer progress.

The source itself is read-only. Database/table CDC must already be enabled and
SQL Server Agent must capture committed transactions. Application credentials
need SELECT on captured data/CDC and catalog visibility, not writes, CREATE,
sysadmin or KILL permissions. Empty `tables` discovers CDC tables; an explicitly
configured bootstrap `barrier_table` is excluded from source selection.

## Bounded pages, not bounded history

`max_rows` limits a returned CDC page, not the retained backlog. Bounded
per-table key windows are merged by actual commit LSN / sequence / table ID.
The producer reads complete update pairs, and a rekey's delete/insert images
retain their distinct ordinal cursors when a page ends between them. A partial
page returns its last emitted cursor; only proven interval exhaustion returns
the sampled maximum LSN. Retention is checked before and after each page.

`max_bytes` is the **sum of the actual graph-record codec payload byte lengths**,
including both full images for an UPDATE and the full before image for a DELETE.
It is not Debug-string length, total history size, total Rust heap usage, or
transport-envelope/cursor overhead. Pages/queues stay within this budget; the
worker can additionally retain one bounded CDC pair plus catalog/driver state.
One indivisible change that exceeds the configured byte budget fails explicitly,
without truncation. Large histories of individually valid rows continue paging.

For bounded keyset reads, paging uses the selected CDC capture tables directly,
with the normal CDC minimum-LSN/authorization checks. This needs explicit SELECT
on those capture tables. **Function-only legacy grants are not enough** for this
native path; the plugin does not grant permissions itself. This permission
difference and actual query-plan/performance behavior remain part of deferred
parity qualification.

## Legacy parity and corrections

| Capability | Native implementation / qualification limit |
|---|---|
| Tiberius connection, LSN decoding, SQL key/value helpers | Reused from `mssql-common`; CDC operation decoder and its original assertions moved there and re-exported by the legacy source. |
| INSERT / UPDATE / DELETE | Complete CDC operation groups, including update-before images; an UPDATE publishes a full replacement, not a property patch. Full before/after images are retained in standard graph change records. Real SQL tests remain deferred. |
| Rekey | Explicit old-key DELETE then new-key INSERT; distinct CDC image ordinals permit partial replay. Legacy discarded update-before images and could leave the old key behind. |
| Transactions / rollback | Uses committed CDC rows sorted globally by commit LSN, sequence, table and image. Row-mode delivery does **not** claim atomic multi-row graph evaluation. Legacy “transaction grouping” also dispatched individual rows, not an atomic graph transaction. |
| Primary/custom/composite keys | Shared catalog/type logic, canonical schema-qualified IDs; null custom keys fail. Composite separators are escaped to prevent legacy underscore collisions, so migration requires authoritative query reconstruction. |
| Selected tables / capture identities | Actual `cdc.change_tables` metadata, not guessed `schema_table` capture names. Multiple generations for one table fail explicitly; schema, capture and key changes require reset, not silent adoption. |
| Beginning/current | Retained beginning or strictly after observed current CDC maximum. Fresh streaming-only current has no preexisting state. |
| Replay/restart | Minimum actual consumer cursor; partial transactions retain per-row positions. Expired CDC, missing partial rows, database/catalog changes and malformed positions fail. No fake persistent state or auto-skip. CDC retention is finite and operator-managed. |
| Retry | Legacy reconnect/log-and-retry loops are not a native handled guarantee. Native failures propagate; no log-and-drop or automatic gap fallback. |
| Data schema | A stable reader exists at construction and reads real catalog independently of pending `next()`. Stop/restart fences retired reads. Public Host/Core inspection assertions are in the real fixture, but remain unqualified while the database cannot run. |
| Bootstrap | Separate native role; see [bootstrap modes](../../bootstrappers/mssql/README.md). The live source does not require a marker or database writes. |
| SQL authentication/TLS | Existing SQL login and native-tls/rustls encryption modes retained. Windows/Azure AD were explicitly unimplemented in the common connection and remain rejected. |
| Shutdown | Caller cancellation/timeout retains owned SQL work. Awaited stop may fail/time out; neither dropping a socket nor CLOSED means server work stopped. No KILL permission required. |

A persistent query provider can retain these opaque cursors; the source does not
create a private checkpoint store. Changing family/ID/key/capture definitions
requires reconstruction, not an unverified automatic migration.

Run `make test-native-mssql` after building matching libraries. The Host tests
exercise two real queries, static/dynamic libraries and both Tokio flavors.
Database startup failures are failures, never ignored successes. Recovery
interleavings, cross-process persistent resume, exhaustive type/schema changes,
and concurrent snapshot/CDC cut proofs still require a working real SQL Server
environment before production qualification.
