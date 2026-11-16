# SQLite common implementation

Shared SQL parameter/REST/table-key configuration, bounded schema discovery,
SQLite value conversion and native graph identity mapping for the SQLite source
and its native bootstrap providers.

Primary/configured keys use `sqlite:v1:` followed by the table and a sorted map
of column names to SQLite type/value pairs. Physical-rowid identities use
`sqlite:rowid:v1:` followed by the table and actual rowid. User columns that
shadow SQLite rowid aliases are never mistaken for the physical identity.
These current-version formats must remain consistent across capture,
standalone file bootstrap and coordinated source snapshots.

`read_table` streams through a callback and respects early termination; it does
not collect rows or create a worker. Connection ownership, read transactions,
limits, cancellation, source replay and query handover belong to its callers.
The default feature set depends on shared core values and SQLite/data utilities,
not the query runtime, evaluator or either plugin SDK.

## Shared native engine

The optional `native-runtime` feature supplies the existing bounded native SQL
handle, isolated transaction command lanes, capture worker, source transaction
journal and per-consumer progress handling. `native::runtime::SqliteRuntime`
owns the database worker and retains caller-supplied ownership leases through
actual worker completion. The host-linked source and independent native source
use this implementation rather than separate journals or execution engines.

The coordinated snapshot engines retain the same query-owned initialization
state, real source positions and shared snapshot/live exclusion. Their bounded
readers are independent of an async-stream or plugin ABI. Host bindings retain
the actual owner/set guard; moving implementation code does not replace that
ownership evidence.

The existing callback-based `SqliteHandle::transaction` uses a reserved rollback
slot and private command lane, without a forwarding task. Dropping an unfinished
scope requests rollback; cancellation after commit submission still has an
unknown caller outcome, not a safe retry receipt. These remain plugin-specific
Rust operations, not a public SQL service on DrasiLib or across the ABI.

`SqliteHandle::describe_schema` runs through that worker's bounded command lane
and returns the existing `core-types::schema::SourceSchema` data model. It reports
actual captured table/column names and key descriptions, with conservative
SQLite affinity hints. It does not invent relationship edges, expose reserved
journal tables, or add a rowid property that capture does not emit. The serialized
JSON result is bounded by the configured transaction byte limit; configuration
metadata is not treated as the source schema.

The native `SourceSchemaReader` captures a generation-bound worker handle for
each request. Stop/drop retires that generation before cleanup; a held request
cannot retarget a replacement worker. The stable reader can inspect a new
generation only through a new request after successful startup.

The optional `rest` feature supplies the shared HTTP routes and parameterized SQL
generation through `SqliteRestClient`. Legacy clients and the native SQL handle
implement that interface; REST does not select another connection or convert
native events into legacy Source callbacks. `run_rest_api` keeps the legacy
route set (unauthenticated, unpaged). `rest_api::router` builds the same routes
with `RestOptions`: bearer authentication, a body limit, a request deadline, a
pause gate (503 with `Retry-After`), paging and the optional `/api/sql` route.

`SqliteRuntime` lifecycle: `pause` asks the worker to park at its next
transaction boundary and waits for its acknowledgement (writes are refused with
the typed `SqlitePaused`, output and journal replay are held, reads continue);
`resume` reopens. `stop(mode, deadline)` runs in two phases: REST stops
accepting requests and the worker drains (serving accepted requests without
ever waiting for output), then the worker refuses queued commands
(`SqliteStopping`) and exits. Expiry keeps the workers owned for a retried
stop; undelivered volatile output is reported in `StopOutcome`.
`drain_open_transactions` chooses whether Drain lets an open transaction scope
finish (native plugin) or rolls it back (host-linked source).
`SqliteConnectionOptions` carries `initSql` and WAL mode for every connection
the source opens, including snapshot connections. `journal::erase_owned` drops
a source's own journal tables in one transaction, idempotently.

The native features use `drasi-computation-contracts` and the caller's Tokio
runtime. They do not create a Drasi graph or depend on `drasi-lib`, the query
evaluator/parsers or a plugin SDK. Source/query resource identity, progress
binding and lifecycle policy remain the responsibility of each host binding.
Existing serialized journal, key, rowid and initialization formats are unchanged.
