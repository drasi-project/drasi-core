# Native PostgreSQL sources

`drasi-computation-source-postgres` provides four explicit factories through native
ComputationGraph ABI `1.0.0`, each at implementation/configuration version `1`.

| Factory | Output | Query requirements |
|---|---|---|
| `drasi/postgres-transactions` | `SourceTransactionCodec`: complete committed transactions | Atomic persistent query with `source_transactions` enabled |
| `drasi.postgres/changes` | `GraphChangeCodec`: individual committed row changes | Ordinary query, including memory storage; an actual progress-owner binding is still required |
| `drasi.postgres/shared-changes` | `GraphChangeCodec`: individual committed row changes | A fixed set of actual ordinary-query owners, including memory storage; independent input-position filtering |
| `drasi.postgres/shared-transactions` | `SourceTransactionCodec`: complete committed transactions | A fixed set of actual atomic persistent query owners, each with `source_transactions` enabled |

Factory identity selects the contract; none silently changes another factory's
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
output stream) and `settings` (`PostgresTransactionConfig`). The single-owner
factories also accept `coordinated_snapshot` (default `false`). `settings`
contains credentials, so a graph specification must use a secret
`ConfigurationResolverResource` reference, not a literal.

For the single-owner transaction factory, the required `source_progress` dependency is a real
`QuerySourceProgressResource` for the immediate atomic, persistent query owner.
The query uses that same actual owner and enables `source_transactions` with
appropriate limits. Equal graph/query names are not ownership evidence. Every
branch must lead to that owner. The ordinary single-owner factory also uses
`source_progress`; the shared-row and shared-transaction factories use
`consumer_progress`, never both bindings together.

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

## Shared ordinary row queries

Select `drasi.postgres/shared-changes` for one PostgreSQL source feeding multiple
ordinary queries in the same DrasiLib ComputationGraph. Its required Checkpoint
dependency, `consumer_progress`, must contain the real `ConsumerProgressSet`
bound to this source and every downstream input-progress owner. The graph
validates actual ownership and branch coverage; matching names, a
`QuerySourceProgressResource`, or an aggregate checkpoint are not substitutes.
Construction retains the read capability and its immutable membership without
reading or subscribing before start.

The configuration uses the ordinary-row fields above, **except
`coordinated_snapshot` is not accepted**. `persistent=false` makes no
process-restart claim, even if some members happen to be persistent.
`persistent=true` requires every member to publish persistent progress and
requires the existing retained slot and explicit initial LSN.

Replay starts from an actual member's minimum **WAL position**, comparing commit
LSNs and then offsets within a commit, not logical sequence numbers from
independently initialized queries. Feedback is the minimum complete-commit floor
of every member; a partial transaction never acknowledges its unhandled suffix.
Actual snapshot boundaries between visible commits are carried into later row
cursors, so moving from a snapshot into a partial transaction does not regress
the already established coverage floor.
The native source supplies one stable input-position policy to the host. Each
real query uses it to skip physically covered replay while still advancing
transport progress. Replayed or already queued rows therefore do not repeat
query effects when assigned newer logical and transport counters.

Both emitted counters stay above the maximum of all relevant consumer counters
and this component's prior allocations, including reconnect and reader
replacement. One existing owned worker closes its old connection before
reconnecting, preserves admitted bounds for queued work, and revalidates slot
retention and database/schema binding. The shared implementation uses the same
connection, TLS, decoder and row-expansion logic as the single-owner modes.

An entirely fresh, ready group can begin streaming from its configured initial
slot boundary. Missing first cursors hold feedback at that original boundary;
the same component can replay it while those original members catch up.
This is not enrollment of a new consumer. A newly constructed group mixing
handled and missing cursors fails explicitly rather than starting the missing
member at `read_lsn`. Failed, reset, removed, rebound or cursor-losing members
also fence progress. A reset may be adopted on an awaited restart only after the
actual owner publishes a newer generation's completed, same-binding configured
snapshot. Unready members cannot advance feedback, and a subscription that
observed unready state cannot silently resume without an ordered restart.
Cleanup closes the owned reader and
subscriptions before restart; only the expected revoked progress capability is
accepted as shutdown after the worker has actually joined.

For initial data, attach `drasi.postgres/shared-row-bootstrap` from the
[bootstrap library](../../bootstrappers/postgres) to each actual query, using
the same source, stream and semantic settings. It supports memory queries
without claiming persistent state. Each query imports its own temporary exported
snapshot while the configured live slot remains untouched. The source waits
for every required owner to complete initialization, then replays from the
earliest real boundary and filters already covered input independently.

Membership is immutable **per binding**, not limited to the graph's initial
queries. Late enrollment uses explicit Core reconciliation: stop the prior
source, drain accepted inputs, replace the actual consumer group, initialize
the new member, and only then resume the single source. Re-enrolling an existing
memory query likewise requires the ordered source stop/rewind/resubscribe
boundary. Directly editing a subscription or dropping a slow owner from a
snapshot is not supported. Neither factory adopts managed snapshot-slot
ownership or resets/drops/recreates the configured live slot.

Reconstruction with undrained persistent input-pipe frames is not part of this
qualification. Rebinding uses Core's drain barrier, which preserves accepted
rows and exposes their handled counter floors before the new source starts. The
consumer capability reports handled progress, not counters of still-queued
frames. This component retains its own allocation bounds across stop/start and
reconnect, but does not invent that missing host state on cold reconstruction.

## Shared complete transactions

Select `drasi.postgres/shared-transactions` for one source feeding multiple
transaction-enabled queries. Supply only `stream` and secret `settings`, plus
the required Checkpoint dependency `consumer_progress` containing the actual
`Arc<ConsumerProgressSet>`. Every member must be an atomic persistent query;
membership alone, volatile `input_progress`, or a row-mode configuration does
not establish that guarantee. Each query enables `source_transactions` with
limits sufficient for the source's complete groups.

`PostgresTransactionSource::shared_transactions` retains the actual native
consumer capability, its fixed `consumer_ids`, and one immutable input-position
policy without reading or subscribing during construction. The implementation
reuses the existing replication worker, connection, strict transaction decoder,
mapping, limits, timestamps and `SourceTransactionCodec`; it does not run a
source or live slot per query. A committed transaction is one input and one
atomic query evaluation, including multiple updates or insert/delete pairs
within that transaction. Rejected or incomplete groups expose no row prefix.

The new JSON position contract is deliberately distinct:

| Field | Contract |
|---|---|
| `version` | Exactly `1`. |
| `mode` | `whole_commit` for live transactions; `configured_snapshot` for completed initialization. |
| `binding` | 32-byte SHA-256 of the domain `drasi.postgres/shared-transactions/v1` followed by a NUL byte and the existing physical source binding. |
| `commit_lsn` | Positive physical whole-commit LSN or the real exported snapshot boundary. |
| `sequence` | Positive logical emission counter for a live transaction; zero only for a configured snapshot. |

Unknown fields, managed-slot fields, mismatched bindings, invalid sequences,
and old row/single-owner transaction formats are rejected. Position bytes retain
the common 64-KiB limit. The transaction identity remains the actual commit LSN,
not the independently allocated logical or transport counter.

Replay and feedback use the minimum physical boundary of **every** actual
member. The selected cursor remains that member's cursor; no aggregate progress
snapshot or durability is manufactured. Logical and transport emission floors
include every member's counters and prior allocations retained by this source.
Each query filters physically covered transactions independently, including a
later snapshot with unrelated sequence counters. Readiness, persistence, failure
and reset generation are checked separately for each member.

Attach `drasi.postgres/shared-transaction-bootstrap` to each query when initial
data is needed. It uses the existing configured slot and separately owned
temporary snapshot exports, with the matching transaction position contract.
The row and managed-slot bootstrap factories are not interchangeable with it.
Persistent incomplete initialization is fenced under `Strict`; an explicitly
configured `AutoReset` reinitializes the query without modifying the live slot.

Transaction restart is graph-owned: there are no row-mode auto-create,
start-at-latest or reconnect settings. Stop must join the real worker before
rewind or membership replacement. The same explicit reconciliation/drain
boundary described above applies to late enrollment. Qualification includes
same-instance replay with queued transactions and drained persistent graph
reconstruction, **not cold reconstruction of undrained persistent input pipes**.
Old factory identities and their persisted formats remain unchanged.

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

Replay-only mode requires a pre-existing, exclusively used persistent `pgoutput`
slot, PostgreSQL 13+, `max_slot_wal_keep_size=-1`, and
`idle_replication_slot_timeout=0` where supported. Startup rejects unavailable
positions, temporary slots, changed source bindings and unsafe retention settings.
Monitor retained WAL disk usage; administrative deletion or storage loss is not
recoverable through this contract.

The single-owner factories' `coordinated_snapshot=true` uses the separate
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

The shared source/bootstrap pair supplies native one-source/many-query
streaming, unequal initial snapshots, persistent-cursor replay, and ordered
late enrollment/re-enrollment. The existing single-owner factories retain
their original guards and contracts; one source per query is not the shared
implementation. Legacy application configurations still need conversion to
the actual native owner/group and lifecycle bindings.

Gap-free memory initialization uses `drasi.postgres/shared-row-bootstrap`,
not the standalone SELECT snapshot factory. Memory-query loss requires a new
ordered snapshot; neither a volatile progress cursor nor an exported snapshot
is advertised as persistent recovery state.

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
| PostgreSQL `native_transactions/shared.rs` qualification | Static and rebuilt dynamic libraries run one source with two real queries in DrasiLib; covers both memory runtimes, different persistent row positions/counters, conservative feedback, and reconnect with queued duplicates. |
| PostgreSQL `native_transactions/shared_bootstrap.rs` qualification | Covers concurrent writes across unequal snapshots, a third query joining through explicit group replacement, queued-input draining/counter preservation, memory re-enrollment, failed/cancelled snapshot cleanup, and configured-slot feedback fences. |
| PostgreSQL `native_transactions/shared_transactions.rs` qualification | Both runtimes and static/real dynamic libraries exercise whole-transaction fan-out, unrelated physical/logical counters, unequal snapshots, slow-member feedback, queued replay, drained restart, membership replacement, persistent interruption/rebootstrap and actual-owner rejection. |
| Server `examples/configs/02-sources/postgres-cdc-complete.yaml` | Still legacy: its shared source feeds three queries. The native shared source/bootstrap contract is available; this example's consumer conversion remains separate. |
| Server Getting Started step 3-6/server configurations | Still legacy ordinary source/bootstrap consumers; their multi-query/lifecycle integration must be converted with the shared feedback/handover services. |
| Server Trading `trading-sources-only.yaml` | Still legacy, including queries created after source startup; conversion must use explicit native group reconciliation rather than uncoordinated subscription changes. |
| Server `order-exception-handling.yaml` | Still a mixed-family ordinary configuration, not a native PostgreSQL conversion. |
| Legacy PostgreSQL library/integration tests | Retained as compatibility assertions, not counted as native consumer migration. |

The qualified rows do not establish full PostgreSQL family closure while the
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
