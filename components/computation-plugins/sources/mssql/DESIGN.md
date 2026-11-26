# Native SQL Server source: design

`drasi-computation-source-mssql` streams SQL Server change data capture
(CDC) into a ComputationGraph. It is a native plugin (ABI 0.2) with one
factory, `drasi.mssql/changes`, at implementation and configuration version
`1`. The CDC engine (settings, catalog and binding, value mapping, paging,
snapshots and owned markers) lives in `drasi-mssql-common::native`, shared
with the SQL Server bootstrap providers; the SQL Server client (TLS, trust,
authentication, connect, error classification) lives in
`drasi-mssql-common::client`, shared with the stored-procedure sink. Neither
depends on `drasi-lib`.

Field reference, prerequisites and runbook: [CONFIGURATION.md](CONFIGURATION.md).

> **Qualification status.** Live SQL Server qualification is explicitly
> deferred by the user (CW-78; see
> `components/host-sdk/tests/fixtures/native_mssql/README.md`). Everything
> below is verified with unit tests against an in-memory implementation of
> the database seam (`CdcSession`) and with database-free Host checks. The
> SQL text itself, the server's CDC shapes (MS-15) and the commit-time
> conversion have not run against a real server. The live test modules are
> kept compiling and current for a future authorized run.

## Scope

One envelope per CDC row change (`GraphChangeCodec`), for a fixed set of
consumers bound through `consumer_progress`, each with its own committed
cursor and an input-position policy that skips covered replay. Initial data
comes from the paired providers in
[`drasi-computation-bootstrap-mssql`](../../bootstrappers/mssql), which take
the same settings.

## Architecture

```
start ──► wait for consumers ─► connect ─► read catalog + check grants ─► check cursor ─► spawn worker
                                                                                              │
worker loop: [pause gate] ─► emit queued change ─► observe consumers ─► poll one page ─► queue │
                     ▲                                   │ not ready: hold      │ transport error
                     └───────────────── idle (pollIntervalMs) ◄─────────────────┘ ─► reconnect (backoff)
next() ◄── capacity-one channel ◄── worker
```

The component (`component.rs`) owns lifecycle and management; the worker
(`worker.rs`) owns one session, the catalog, the read cursor and a queue of
at most one page. `next` only receives from a capacity-one channel, so a
cancelled `next` never abandons SQL work.

### The database seam

`CdcSession` is every question the engine asks SQL Server: database
identity, capture instances, table facts, change-table permission, a catalog
fingerprint, time zone, LSN bounds, group keys and row ranges, and marker
erase. `TiberiusSession` implements it with parameterized SQL; the tests use
`native::memory::MemoryDatabase` (feature `test-support`), which models
change tables, retention, catalog changes, transport faults and blocking
queries. Paging, cursors, grouping, binding, retry classification, pause,
stop, reconnect and erase are all written against the trait.

## CDC polling model

A poll reads one page (`native/paging.rs`):

1. If the catalog fingerprint changed (a cheap query over
   `cdc.change_tables` and the source tables' `modify_date`), reread the
   catalog. A different binding is `SchemaChanged`; anything else (a
   non-key column) is adopted.
2. Read `sys.fn_cdc_get_max_lsn()` and every capture instance's
   `sys.fn_cdc_get_min_lsn` in one round trip each. A cursor ahead of the
   maximum, or behind a minimum, is `PositionUnavailable`.
3. Read at most `maxRows + 1` change-group keys `(lsn, seqval, table)` after
   the cursor with one query: per-table `TOP` branches merged by a global
   `TOP`, so no table can hide an earlier key.
4. Fetch each table's rows for its keys with **one range query per table**
   (MS-09; previously one query per group, at most 32 groups a page),
   decode the groups and fill the page until `maxRows` changes or `maxBytes`
   of record payload.
5. Recheck retention. Only then is the page's boundary returned.

`maxRows` is the page size in groups and changes; `maxBytes` bounds the
record payload of a page (both images of an update count), and one change
must fit. There is no `maxBytes/128` heuristic.

### Cursor

A cursor is `{version 1, binding, lsn, row}`. `row = None` covers the whole
commit LSN; a row cursor `(seqval, table object, image ordinal)` covers one
emitted image. A page's boundary is the maximum LSN only when the key read
proved the interval exhausted and every change fit; otherwise it is the last
emitted change's row position. A row cursor's own group is read again for
its remaining images (a key change emits a delete at ordinal 0 and an insert
at ordinal 1, which may land on different pages) without counting toward the
page window, so paging always progresses.

### Binding (MS-14, decision D-7)

The binding is SHA-256 over: a version tag, the database's
`service_broker_guid` and name, and for each selected table its schema,
name, object ID, capture instance and change-table object, key columns and
key column base types. **Host and port are not bound** (MS-14): a DNS or
availability-group listener rename keeps every cursor. **Non-key columns
are not bound** (D-7): adding a column to a table does not change its
capture instance, and a capture instance's columns never change. Bound
facts that change fail with a typed `SchemaChanged`:

- a new or replaced capture instance on a selected table (SQL Server allows
  two during a migration; the source refuses the ambiguity);
- a key column change, a retyped key column, an explicit `tableKeys` change
  (configuration, so refused by update planning);
- with empty `tables`, a capture instance on a new table.

Capture-instance changes on **unselected** tables are ignored.

### Grouping (MS-15)

A group `(lsn, seqval, table)` must be a single insert (2) or delete (1), or
an update before/after pair (3, 4). An update whose key changed becomes a
delete and an insert. Any other shape (three images, an unpaired image)
fails with a typed `Unsupported` error rather than being guessed at. Whether
SQL Server emits split updates or several `__$command_id` rows for one
`(lsn, seqval)` in practice needs a live server and remains open.

### Commit times (MS-10, decision D-7)

`effective_from` is the commit time in UTC. The range query converts with
`sys.fn_cdc_map_lsn_to_time(__$start_lsn) AT TIME ZONE @zone`, which yields
a `datetimeoffset` carrying the offset **in force at that commit**, so
historical commits across a DST change are correct (the previous release
shifted every commit by today's offset). The zone is `serverTimeZone` (checked
against `sys.time_zone_info`) or, when absent, `CURRENT_TIMEZONE_ID()`
(SQL Server 2022 and Azure SQL). An older server without `serverTimeZone`
fails at start with an actionable `Unsupported` error rather than guessing.

## Data mapping

Labels are the table name; element IDs are `schema.table:key` (multiple key
values joined by `_`, with `%` and `_` escaped), shared with the bootstrap
providers through `native::element`. Primary keys come from the catalog for
the selected tables only (MS-08: the previous release read every primary key
in the database on every poll); `tableKeys` overrides them. Key columns must
be captured and of a keyable type.

Values (MS-06, MS-20) are converted in SQL so no type rejects a table:

| SQL Server type | Value | Schema type |
|---|---|---|
| tinyint, smallint, int, bigint | integer | Integer |
| float, real, smallmoney, decimal/numeric with precision ≤ 15 | float (exact for ≤ 15 digits) | Float |
| decimal/numeric with precision > 15, money | exact decimal text | String |
| bit | boolean | Boolean |
| datetime, datetime2, smalldatetime | local date-time | Timestamp |
| datetimeoffset | zoned date-time | Timestamp |
| date, time | ISO text | String |
| char, nchar, varchar, nvarchar, text, ntext | text | String |
| uniqueidentifier | lowercase text | String |
| binary, varbinary, image, rowversion | `0x` hex text | String |
| json (SQL Server 2025) | structured value | Json |
| xml, geography, geometry (WKT), hierarchyid, sql_variant | text | none |
| anything else (CLR types, vector, …) | NULL | none |

Identifiers may be bracket-quoted (`[Sales Ops].[Order Lines]`); every name
is re-quoted before it reaches SQL and catalog lookups bind names as
parameters.

## Consumers and recovery

### Declared description

Outputs carry only source replay progress (`untracked` identity). The
source's progress is its consumers' committed cursors, bound as the
`consumer_progress` dependency, so it declares exactly the durability that
resource declares: durable consumer progress (queries with persistent
indexes) gives durable progress that resumes after a process restart;
volatile consumer progress gives volatile progress, and a restarted process
begins again at `startPosition`. Either binding is accepted (requirement
`any`); an undeclared binding leaves the source's progress undeclared,
which satisfies no downstream requirement. The source does not check
durability at start.

### Replay

Replay starts at the members' minimum committed cursor; a member without a
cursor joins there. A group in which no member has a cursor starts at
`startPosition`:

- `current` resolves the maximum LSN and emits it as a **progress-only
  record** (an empty change set carrying the source position,
  `GraphChangeCodec::encode_source_progress`) before anything else. Polling
  is held until every member has committed a cursor, so the origin is durable
  before any change is published: a restart, even with a new source object,
  resumes there and delivers changes made while the process was down. A
  restart before every member committed the record re-resolves `current`,
  which loses nothing because nothing was published.
- `beginning` is each capture instance's retained minimum; it needs no record.

A crash between two members' first commits therefore restarts: the member
that committed holds the origin (or a later change), and the other joins at
the group minimum. Failed, reset, regressed or rebound members fence the
source. A member that is momentarily **not ready** (a consumer restarting)
holds polling and emission instead of failing the source (MS-11). Membership
is fixed per binding; changing it is a host-level replacement.

While the selected tables are idle, an empty page's boundary (the maximum LSN
the poll proved empty) is emitted as a progress record when the database's
maximum LSN has moved past the last emitted position, at most once every 30
seconds of quiet. Committed cursors therefore follow CDC retention, and a
restart after the cleanup job removed the rows before that boundary resumes
instead of reporting `PositionUnavailable`. This is the principle the MySQL
transaction factories follow with empty committed transactions.

Logical sequences stay above every member's checkpoint and transport
sequence and above this object's earlier allocations.

### Replay retention (MS-21)

The source declares `consumer_replay_retention = LOCAL_PROCESS_RESTART`.
CDC change tables do survive power loss, but that does not make a stronger
claim true: the cleanup job purges change rows by age (three days by
default), so recovery after an outage longer than retention (while the
process is down, no record advances the cursor) is conditional on it.

## Snapshot handover and markers (decision D-1, MS-03)

The coordinated provider briefly takes shared table locks, fixes a
SNAPSHOT-isolation view while no selected-table write can commit, commits a
marker row into the administrator-provisioned `barrierTable` in the
transaction that releases the locks, waits for it in the barrier's change
table and returns that LSN as the cut. The scan reads row versions and never
blocks writers (see the bootstrapper README). Each marker's `token` is a
`uniqueidentifier` whose first 64 bits derive from the graph and **this
source** (`SHA-256("drasi.mssql/marker/v1", graph, source)`), with 64 random
bits after. The provider deletes its marker after every attempt; a crash or
failed cleanup can leave one behind.

**The source owns its markers.** Erasing removal deletes every marker with
this source's prefix (`DELETE … WHERE UPPER(LEFT(CONVERT(char(36), token),
18)) = @prefix`), and nothing else; a missing barrier table or no matching
rows is success, so a retry is safe. The bootstrapper does **not** sweep:
bootstrap providers have no erase hook, and a startup sweep could delete
the marker of a snapshot still in progress. Without `barrierTable` the
source owns no external state and erase is a documented no-op. Erase
connects with the activation credentials the host resolves for the
erasing dispose (`DisposeContext::credentials`, MS-22), falling back to the
last activation's credentials or the literal login, so a source rebuilt
after a process restart erases its markers without starting first.

## Lifecycle

| Hook | Behaviour |
|---|---|
| `initialize` | Validates the configuration. No I/O. |
| `start` | Takes activation credentials, waits for every consumer to be ready, connects, reads the catalog (checking SELECT on each change table, MS-07), checks the committed cursor's binding and retention, and starts the worker — all within the host deadline and abandoned on cancellation (a pre-cancelled start returns `Cancelled` without connecting; an expired one is retryable). A failed start requires a stop before the next start. |
| `pause` | Asks the worker to stop at the next page boundary: an in-flight query finishes, queued changes are held, nothing more is read or emitted. Returns once the worker acknowledges; on timeout or cancellation the pause is withdrawn and the source keeps running. A reconnecting worker counts as paused. |
| `resume` | Lifts the gate; held changes are emitted. |
| `stop` | `Drain`: the in-flight query finishes, then the session closes gracefully. `Abort`: the worker task is cancelled, which drops the connection. Both honour the host deadline and keep the worker owned on timeout (retryable), so a later stop completes it. A polling failure already reported through `next` does not make stop fail. |
| `dispose` | Requires a stopped source. `Preserve` does nothing; `Erase` deletes this source's orphaned markers (above). |

**Tiberius cannot send a TDS ATTENTION**, so no query can be cancelled in
place. Pause and Drain therefore wait for the in-flight query (bounded by
`commandTimeoutMs`); Abort drops the connection, and SQL Server ends the
request when it notices the closed connection.

### Reconnection (MS-05)

A transport failure — classified by type (`std::io::Error`, timeouts, the
connect timeout, Tiberius I/O, protocol and routing errors) or SQL Server
error number (1205, 1222, 3960, 3961, in-memory OLTP conflicts; 233, 64,
4221, 10053, 10054, 10060, 10928, 10929, 40197, 40501, 40613, 49918–49920)
and never by message text — drops the session and reconnects with
full-jitter backoff (250 ms doubling to 30 s) until `reconnectBudgetMs` is
spent. A reconnect rereads the catalog and requires the same binding, then
resumes at the worker's **read cursor**: everything before it was already
queued in this worker, so nothing is lost or replayed within an activation.
After a restart the source resumes at the members' committed minimum
instead. Typed failures (`SchemaChanged`, `PositionUnavailable`,
`Unsupported`), authentication and permission errors never reconnect.

## Management

The factory's `UpdatePlanner::assess` and `NativeManagement` classify every
field (table in [CONFIGURATION.md](CONFIGURATION.md#settings)); assessment is
pure.

- **InPlace** (after a completed pause): `pollIntervalMs`, `maxRows`,
  `maxBytes`, `reconnectBudgetMs`; the worker reads them before each poll or
  reconnect.
- **Restart** (while stopped): connection, TLS, credentials, timeouts,
  `serverTimeZone`, `startPosition`.
- **Unsupported**: `stream`, `database`, `tables`, `tableKeys`,
  `barrierTable` — what cursors and markers are bound to. The source does not
  claim a Recreate plan because it cannot coordinate the downstream rebuild
  a new binding needs.

`apply` refuses when the current configuration no longer matches, the plan
changed, the proposal is invalid, or the boundary was not established.

## Security

- `encryption` defaults to `required` (TLS for the whole session, server
  certificate verified). `loginOnly` and `disabled` are explicit opt-ins and
  log a warning at start, as does `trustServerCertificate`. `trustedCaPem`
  adds trust roots; `hostNameInCertificate` overrides the expected name.
- `password` is the only secret, a top-level field; `Debug` redacts it.
  Activation credentials replace the literal login per activation:
  username/password as SQL authentication, a token as a Microsoft Entra ID
  access token (`AADToken`, MS-12). A provider-bound source never falls back
  to literals.
- All values are bound parameters; object names come from the catalog or
  validated configuration and are bracket-quoted.
- The login needs read access only: catalog views, `cdc.change_tables`, the
  CDC functions, and SELECT on each selected change table. Erasing removal
  additionally needs DELETE on `barrierTable`.

## Observability

Logs go through the `log` facade: transport warnings at start, columns
streamed without a schema type, a held source while consumers are not
ready, each reconnect attempt and its success, erased marker counts, and a
worker failure. Errors keep their causes and typed `SourceFailure` variants.

## Known limitations

- Live qualification is deferred (above); MS-15 shapes are unverified.
- A process that stays down for longer than CDC retention fails on restart
  with `PositionUnavailable` (see replay retention); while it runs, idle
  progress records keep cursors inside retention.
- No column-exclusion list; bulky columns are always read.
- Tiberius has no ATTENTION; see Lifecycle.
- Windows integrated authentication is not supported.

## Differences from the legacy source

| Legacy (`drasi-source-mssql`) | Native |
|---|---|
| `fn_cdc_get_all_changes_*` | Direct change-table reads; SELECT on each change table is checked at start (MS-07) |
| Nested `MsSqlSourceConfig`, `encryption` default `not_supported` | Flat camelCase settings, `encryption` default `required` |
| Any error fell back to the current LSN on an invalid position | Typed `PositionUnavailable`, never a silent fallback |
| Retry on any error with fixed backoff | Transport failures only, jittered backoff within a budget |
| Decimals and money as floats | Exact decimal text above 15 digits |
| Unsupported column types NULL with a warning | Converted to text where possible; schema type `None` |
| Commit times not converted | UTC with the historical offset |
| Dynamic subscribers | Fixed consumer set with per-consumer replay |
| SQL authentication only | SQL authentication and Entra ID access tokens via activation credentials |
