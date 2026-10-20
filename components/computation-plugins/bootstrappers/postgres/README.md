# Native PostgreSQL bootstrap

`drasi-computation-bootstrap-postgres` is a bootstrap-only library. Its factories
use implementation/configuration version `1` through the native bootstrap
extension of ComputationGraph ABI `1.0.0`. It does
not register a dummy processing component or start a separate graph or service.

| Factory | Use case | Progress and state |
|---|---|---|
| `drasi.postgres/snapshot` | Standalone read-only table snapshot, including for another live source or a memory query | No replication role, publication, slot, source-progress binding or CDC watermark |
| `drasi.postgres/coordinated-bootstrap` | Initial state plus gap-free native complete-transaction CDC | Actual persistent query owner and atomic handover state |
| `drasi.postgres/coordinated-row-bootstrap` | Initial state plus gap-free ordinary per-row CDC | The same persistent ownership, with row-position watermarks |
| `drasi.postgres/shared-row-bootstrap` | Per-query snapshots and one shared configured-slot row source, including memory queries and late enrollment | Actual query owner; no fabricated persistent state or ownership of the configured slot |
| `drasi.postgres/shared-transaction-bootstrap` | Per-query snapshots and one shared configured-slot whole-transaction source | Actual atomic persistent transaction-query owner; distinct whole-commit boundary, no ownership of the configured slot |

## Standalone snapshots

Select `drasi.postgres/snapshot` with `source`, `stream` and a secret `settings`
object containing `host` (default `localhost`), `port` (5432), `database`, `user`,
`password` (empty), `ssl_mode` (`prefer`), optional `tls_ca_pem`, explicit `tables`,
optional `table_keys`, and positive `max_tables`, `max_protocol_bytes`,
`io_timeout_ms`, `max_duration_ms`. Tables may be schema-qualified; selection,
keys and bounds are validated before connection.

The provider uses a normal PostgreSQL connection, REPEATABLE READ/READ ONLY and
bounded server-cursor fetches. A SELECT-only role works with `wal_level=replica`.
Domain base types, SQL NULL, arrays and the existing compact-JSON-text mapping
reuse the shared PostgreSQL decoder. Primary/configured keys use the same stable
native identities as CDC. Keyless tables are still readable: their rows receive
distinct snapshot-local UUID identities with a warning, not a fabricated stable
key or replay guarantee.

No WAL position is invented for a non-PostgreSQL live source. The caller must
order initial loading before unrelated live writes; standalone SELECT does not
establish gap-free PostgreSQL CDC handover. Dropping a stream closes its owned
read-only transaction. Await `stop()` before another snapshot; cancelled cleanup
retains ownership. Missing tables, decoding errors, expired bounds and incomplete
streams fail rather than returning an empty successful snapshot.

## Configured-slot shared snapshots

Use `drasi.postgres/shared-row-bootstrap` with
`drasi.postgres/shared-changes`. Its `source`, `stream` and secret `settings`
(`PostgresTransactionConfig`) must match the live source's semantic binding.
Supply the actual query-owned `QuerySourceProgress`, query ID and graph/instance
scope. Construction is configuration-only. The factory supports both persistent
and ordinary memory query owners; it does not write a managed-slot ownership
record or claim that volatile query progress is durable.

For whole transactions, select `drasi.postgres/shared-transaction-bootstrap`
with `drasi.postgres/shared-transactions`. It has the same configuration and
actual-owner binding, but requires persistent query progress and
transaction-enabled atomic queries. `PostgresSharedSnapshot::transactions`
selects this mode; the existing `new` constructor remains row mode. Neither
constructor reads progress before lifecycle initialization.

The configured `pgoutput` slot must already exist in the correct database,
with the same retention settings required by the source. The provider requires
an idle live slot before capture. Core must await the old source's stop and
establish all required consumers before producer resume; sampling an inactive
slot is not a substitute for that lifecycle contract.

Each provider samples PostgreSQL's clock, validates the publication/tables/keys,
and creates its own uniquely named **TEMPORARY** logical slot with an exported
snapshot. A second owned connection imports the snapshot before any subsequent
command or close on the exporter. Closing the exporter removes only its
temporary slot; the imported read-only transaction remains valid. Bounded cursor
reads reuse the coordinated implementation's key/value mapping and protocol/I/O
limits. The configured live slot is never reset, dropped, recreated or given
invented managed ownership.

Only full stream exhaustion and a final configured-slot coverage check make the
watermark available. The provider rejects feedback movement during required
initialization and missing retained WAL. The row watermark uses an explicit
version-2 **configured-slot after-commit boundary**: real binding and LSN,
no managed slot, no row offset, and logical sequence zero. Normal live row
cursors remain version 1. Version-1 managed snapshot validation is unchanged;
removing a managed slot field from old state does not convert it into this mode.

The shared-transaction watermark instead uses the new
`SharedTransactionPosition` contract: `version=1`,
`mode=configured_snapshot`, a domain-separated transaction binding, the real
exported `commit_lsn`, and `sequence=0`. It has no managed-slot field. Live
transactions use `mode=whole_commit` with a positive logical sequence. See the
[source position contract](../../sources/postgres#shared-complete-transactions)
for the exact fields. Old single-owner transaction and row positions cannot be
reinterpreted as this format.

Core publishes the boundary only after the complete snapshot. Independent
queries may capture different boundaries while writes continue; the shared source
replays the earliest and each query filters input covered by its own position.
Snapshot rows still use normal query initialization, not fabricated source
transactions; subsequent transaction inputs remain complete atomic groups.
Feedback remains behind every required initialization/partial transaction.
An incomplete, failed or cancelled stream publishes no boundary and closes its
owned read-only transaction; await `stop()` before reuse.

For memory-owner restart, preparation requests `RefreshVolatile`; Core performs
its explicit volatile initialization reset and prepares again before taking the
new snapshot. Healthy persistent completion reuses its actual cursor.
Incomplete persistent initialization still requires the configured explicit
recovery policy, not silent state erasure. Non-Ready preparation is retryable
after that reset; duplicate Ready preparation/snapshot operations and cancelled
state remain fenced by the native lifecycle and cleanup guards.

Late enrollment replaces the real `ConsumerProgressSet` through Core
reconciliation, drains previously accepted inputs, and keeps the source stopped
while the new required query captures and commits its snapshot. Re-enrollment
also uses an ordered source stop/rewind/resubscribe boundary. Both paths retain
one live source for the group, not one source/slot per query.

## Managed coordinated snapshots

The factory accepts `source`, `stream` and `settings`
(`PostgresTransactionConfig`, as used by the
[transaction source](../../sources/postgres)). The host must provide the actual
query-owned `QuerySourceProgress`, a matching query ID and instance/graph scope.
Attach the returned provider to that query's `QueryBootstrapResource` or
`with_bootstrap`. The source and bootstrap may be independently loaded libraries;
only host-owned committed progress connects their handover.

```rust,ignore
let plugin = drasi_host_sdk::computation::NativePlugin::from_definition(
    drasi_computation_bootstrap_postgres::plugin()?,
)?;
let snapshot = plugin.bootstrap_factories()[0].create_provider(
    query_id,
    serde_json::json!({"source": "postgres", "stream": "postgres/out", "settings": settings}),
    Some(scope),
    Some(actual_query_progress.clone()),
)?;
```

Factory construction is configuration-only. Credentials in a persisted recipe
belong in a secret configuration reference, not in literal topology fields.
Host-side ownership checks compare the actual query owner, not names or paths.

## Snapshot and WAL boundary

This mode requires PostgreSQL 15+ and a slot-name prefix of at most 30 characters.
The provider persists initialization intent in the query's atomic storage before
creating its dedicated UUID-suffixed persistent slot. It exports a PostgreSQL
snapshot, imports it on a directly owned second connection, and reads bounded
cursor rows while the slot retains concurrent WAL. Shared decoding and key
mapping make initial and live rows identical.

Only successful stream exhaustion yields completed ownership state and a
watermark. The host commits these with the query's initial state and completed
marker. The watermark carries the managed slot identity, real binding and
snapshot LSN; later source positions retain that identity. No speculative
checkpoint, fabricated transaction or cross-library Rust pointer is used.

Healthy restart reuses the completed snapshot and slot. Interrupted initialization
fails under strict recovery. Explicit `AutoReset` can replace only the unfinished
slot named in that query's retained ownership record. Existing external slots are
never adopted or deleted. Missing completed history, changed bindings and missing
ownership state fail rather than silently resnapshotting. Completed-slot
retirement remains an explicit operator action before removing ownership state.

Pre-migration completed positions without the managed-slot ownership field are
not silently upgraded. They fail explicitly; this is not a state-transparent
rollout onto an existing installation. Preserve its slot and durable data until
an explicit transition is selected, rather than deleting state to force startup.

The source uses `coordinated_snapshot=true` and the same source ID, stream and
semantic settings. It waits for the committed position and verifies its binding
against PostgreSQL before starting replication or sending feedback.

For ordinary row queries, select `drasi.postgres/coordinated-row-bootstrap`
with `drasi.postgres/changes` and `persistent=true`. The initial row-sequence
watermark is zero with the **real exported snapshot LSN in its position**;
subsequent row positions preserve that slot and carry their offset/count.
This is cursor-bearing initialization, not a sequence-zero checkpoint fabricated
for a cursorless snapshot. Row and transaction handover state reject each other's
formats rather than reinterpreting retained data.

Snapshot connections use the same verified TLS policy and I/O/frame bounds as
replication. Initial row timestamps use PostgreSQL's clock sampled before the
snapshot boundary, not the host clock; clock skew cannot make the immediately
following WAL updates/deletes appear older than those rows.
Dropping a snapshot closes its owned read-only transaction; the host
must await provider cleanup before reuse. Supported publications contain complete
insert/update/delete changes for ordinary non-partitioned tables. Row/column
filters, inheritance and partition-root remapping reject explicitly.

```sh
cargo build -p drasi-computation-bootstrap-postgres --features dynamic-plugin
make build-native-postgres-plugins
cargo test -p drasi-source-postgres --test native_transactions
```

The legacy `BootstrapProvider` callback remains available for unconverted
consumers. The native standalone factory supplies its independent read-only use
case; it does not reinterpret the callback's per-row handover format.
`shared-row-bootstrap` supplies configured-slot multi-query and no-store memory
initialization through the actual owner/group contract, not through standalone
SELECT or the old managed-slot format. `shared-transaction-bootstrap` supplies
the corresponding persistent whole-transaction path. Both preserve the live
configured slot during interruption, explicit reset and late enrollment.
