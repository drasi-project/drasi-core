# Native PostgreSQL sources

`drasi-computation-source-postgres` provides two explicit factories through native
ComputationGraph ABI `1.0.0`, both at implementation/configuration version `1`.

| Factory | Output | Query requirements |
|---|---|---|
| `drasi/postgres-transactions` | `SourceTransactionCodec`: complete committed transactions | Atomic persistent query with `source_transactions` enabled |
| `drasi.postgres/changes` | `GraphChangeCodec`: individual committed row changes | Ordinary query, including memory storage; an actual progress-owner binding is still required |

Factory identity selects the contract; neither silently changes the other's
schema, checkpoint format or processing guarantees.

The source reuses the PostgreSQL connection, SCRAM, pgoutput decoding, key
mapping, TLS and bounded transaction assembly in `drasi-postgres-common`.
Neither this library nor that shared implementation depends on `drasi-lib`, a
query evaluator, query parser or the legacy plugin SDK.

## Hosting and configuration

Both static and dynamic hosting attach to the **hosting DrasiLib instance's one
ComputationGraph**:

```rust,ignore
let plugin = drasi_host_sdk::computation::NativePlugin::from_definition(
    drasi_computation_source_postgres::plugin()?,
)?;
// Or load the independently built library:
let plugin = drasi_host_sdk::computation::load(path)?;
```

Register its source factory in a component batch. Supply `stream` (the explicit
output stream), `settings` (`PostgresTransactionConfig`), and optional
`coordinated_snapshot` (default `false`). `settings` contains credentials, so a
graph specification must use a secret `ConfigurationResolverResource` reference,
not a literal.

The required `source_progress` dependency is a real
`QuerySourceProgressResource` for the immediate atomic, persistent query owner.
The query uses that same actual owner and enables `source_transactions` with
appropriate limits. Equal graph/query names are not ownership evidence. Every
branch must lead to that owner; another independent query requires its own
source/slot.

## Ordinary row queries

Select `drasi.postgres/changes` for ordinary queries without
`source_transactions`. It uses the same strict decoder, bounded committed
transaction assembly, stable row keys and TLS implementation, then emits each
change separately. Intermediate changes remain visible to the ordinary query.

Its additional fields are:

| Field | Default and behavior |
|---|---|
| `persistent` | `false`: permits an actual volatile query owner and declares no process-restart retention guarantee. `true` requires persistent progress. |
| `auto_create_slot` | Defaults to `true` only for non-coordinated volatile mode. Creates a missing fixed slot before first use; never recreates a slot when committed progress exists. |
| `start_from_slot` | Defaults to `true` only for non-coordinated volatile mode. Fresh streaming starts at the existing/created slot's confirmed boundary. A recorded query cursor always takes precedence. |
| `coordinated_snapshot` | Default `false`. `true` requires `persistent=true` and the `drasi.postgres/coordinated-row-bootstrap` provider on the same actual owner. |
| `reconnect_attempts` | Default `1`, maximum `32`; `0` disables automatic row-mode reconnection. |
| `reconnect_delay_ms` | Default `5000`; enabled reconnect uses a positive delay of at most 60,000 ms. |

Persistent or coordinated rows reject `auto_create_slot=true` and
`start_from_slot=true`; use the retained slot and explicit `settings.start_lsn`,
or the coordinated provider. Volatile streaming alone is not an initial snapshot
and does not recover a lost in-memory query after process failure.

Each handled row retains its commit LSN, row offset/count, previous complete
commit and stable logical sequence. Reconstructing after a partial transaction
replays only its unhandled suffix. Feedback does not cross the transaction until
all its rows have been handled by the owner. Replay transport counters may
increase independently of that logical sequence. No queue acceptance or
`read_lsn` is used as query completion.

Ordinary row mode retries transient connection loss in its existing owned worker.
It closes the old connection, revalidates the same database/schema/slot binding,
and resumes from the actual owner's handled row cursor. Already queued rows may
be replayed with the same logical sequence and a newer transport sequence; they
must not repeat query effects. Failed/fenced/reset owners, changed bindings,
missing retained cursors, malformed data and exceeded transaction limits do not
become reconnect-at-latest fallbacks. Stop cancels and joins an in-flight reconnect.
This does not change the complete-transaction factory's graph-owned restart policy.

## Transaction-source settings

| Settings field | Contract |
|---|---|
| `connection` | `PostgresSourceConfig`: host, port, database, user, password, tables, slot name, publication, SSL mode and optional table-key overrides. |
| `tls_ca_pem` | Optional trusted PEM CA, at most 64 KiB and within `max_protocol_bytes`. Hostname and certificate verification remain enabled. |
| `start_lsn` | Explicit initial WAL position for an externally managed slot. Unused after committed progress exists. |
| `transactions` | Positive `max_changes`, `max_bytes`, `max_duration_ms`. Rejected groups never publish a prefix or advance feedback. |
| `max_protocol_bytes` | Positive independent wire-frame, catalog-result and relation-metadata bound; at least 64 bytes. |
| `io_timeout_ms` | Positive startup, incomplete-frame, feedback and cleanup deadline. |
| `feedback_interval_ms` | Positive heartbeat interval, below the server's WAL sender timeout. |

Replay-only mode requires a pre-existing, exclusively owned persistent `pgoutput`
slot, PostgreSQL 13+, `max_slot_wal_keep_size=-1`, and
`idle_replication_slot_timeout=0` where supported. Startup rejects unavailable
positions, temporary slots, changed source bindings and unsafe retention settings.
Monitor retained WAL disk usage; administrative deletion or storage loss is not
recoverable through this contract.

`coordinated_snapshot=true` uses the separate
[PostgreSQL bootstrap library](../../bootstrappers/postgres). Configure its
`source`, `stream` and settings for this source and attach it to the same owner
query. No source-to-bootstrap Rust resource or object crosses the library
boundary. The source waits for the query's committed snapshot position, which
contains the managed slot identity, database/schema binding and LSN. It never
creates or adopts a slot based only on a matching name.

## Recovery and cleanup

Feedback follows committed query progress, never plugin-queue acceptance.
The source retains one owned replication worker with a capacity-one output queue
and continues heartbeats under backpressure. Cancellation or timed-out stop keeps
that worker owned until a subsequent awaited cleanup completes. Failure requires
cleanup/reconstruction; there is no reconnect-at-a-newer-cursor fallback.

Stable identity is the committed LSN and source binding, independent of transport
sequence. The binding includes the actual PostgreSQL system/database, slot,
publication semantics, selected keys and column types, source and stream.
Snapshot and live rows share the `pg:v1:` key encoding and type mapping.
SQL NULL, unchanged TOAST and key-changing updates preserve the existing
complete-transaction rules. TRUNCATE, two-phase/streaming pgoutput transactions,
automatic schema/publication migration and failover/timeline transitions remain
unsupported and fail explicitly.

## Remaining ordinary-source compatibility boundaries

The actual-owner contract currently permits **one query owner**, not the
legacy source's one-source/many-query subscription model. Creating one source
per query is not a claim of fan-out parity. Safe native fan-out needs a host-owned
set of actual feedback owners, membership/startup fencing, minimum committed
positions, per-query resume filtering and ordered rewind/subscription changes.
Existing multi-query legacy consumers must remain legacy until that service is
available.

Gap-free initial PostgreSQL CDC bootstrap for a query with no checkpoint/atomic
state store is also not supplied by the standalone SELECT snapshot factory.
The exported-slot handover still requires actual atomic/persistent initialization
ownership. Ordinary memory streaming and independently ordered snapshots are
supported, but are not replacements for that stronger combined use case.

Ordinary row-mode reconnect defaults match the legacy five-second delay while
retaining explicit bounded retry and actual-cursor checks. Transaction-mode
restart remains graph-owned. Runtime database-schema introspection is not
silently reproduced by generic native factory configuration metadata; its
ordinary consumer/API integration remains a separate parity item.

### Consumer checklist

| Consumer | Current contract |
|---|---|
| GPU Lab `embedded/src/database.rs` | Uses the native transaction source and coordinated bootstrap factories with the actual persistent query owner. |
| PostgreSQL `native_transactions/ordinary.rs` qualification | Exercises native ordinary memory rows, persistent row bootstrap/restart, partial-transaction suffix replay, and standalone snapshots with a non-PostgreSQL ingress. |
| Server `examples/configs/02-sources/postgres-cdc-complete.yaml` | Still legacy: its shared source feeds three queries. Native one-owner instances are not a drop-in fan-out replacement. |
| Server Getting Started step 3-6/server configurations | Still legacy ordinary source/bootstrap consumers; their multi-query/lifecycle integration must be converted with the shared feedback/handover services. |
| Server Trading `trading-sources-only.yaml` | Still legacy, including queries created after source startup; safe late membership and per-query replay/filtering are prerequisites. |
| Server `order-exception-handling.yaml` | Still a mixed-family ordinary configuration, not a native PostgreSQL conversion. |
| Legacy PostgreSQL library/integration tests | Retained as compatibility assertions, not counted as native consumer migration. |

The first two rows do not establish full PostgreSQL family closure while the
remaining supported ordinary-use-case gaps above are open.

## Build and qualify

```sh
cargo build -p drasi-computation-source-postgres --features dynamic-plugin
make build-native-postgres-plugins
cargo test -p drasi-source-postgres --test native_transactions
```

Use `DRASI_NATIVE_PLUGIN_DIR` for a non-default library directory. The
`libraries` tests fail when a library is missing; they do not skip qualification.
They run real PostgreSQL/RocksDB source, bootstrap and stored-procedure components
through static definitions and actual dynamic entry points. Existing native
transaction, snapshot, TLS, backpressure and abrupt-exit assertions exercise the
same extracted implementation through the direct Rust host bindings.
