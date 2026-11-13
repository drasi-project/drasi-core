# Continuous queries as standard plugin transformers

## Outcome and current implementation

The query plugin runs the existing Core query evaluator as an ordinary
ComputationGraph Transformer. It does not introduce another query engine, graph,
timer runtime, projection or output cache. `drasi-query-common` contains the
extracted query definition, semantic fingerprint, execution settings, builder
configuration, canonical result state, readers and shared execution/recovery
helpers. Linked reference and native adapters call that implementation.

Ordinary query creation requires explicit registration:
`DrasiLibBuilder::with_query_factory`, `DrasiLib::register_query_factory`, or
`ComputationPipelineBuilder::query_factory`. An empty/source-only instance needs
no query implementation. Missing registration is a typed error, not a fallback
to the linked evaluator. Generic graph declarations retain their existing
dependency-wait behavior.

The Host SDK's native ordinary-query adapter binds the selected provider,
progress owner, bootstrap provider, result catalogue, scheduling resource and
transport counter. Server resolves the unique eligible loaded factory before
configured/API query creation. A programmatic host can select one explicitly.
No new YAML factory selector or automatic installation of an unpublished query
library is implied.

The implemented query profiles cover:

- Normal memory providers, retaining their evaluator/indexes across healthy
  stop/start without claiming atomicity or process-restart durability.
- Persistent atomic providers, including committed input, canonical rows, retained
  output, pending delivery, separate/shared journals and process-exit recovery.
- Explicit persistent non-atomic providers, reusing the existing publication/reset
  markers with independent writers, deferred input visibility and interrupted-reset recovery.
- Cypher/GQL, relationships, aggregates, synthetic joins, archives, configured
  input-local middleware, source aliases and complete source transactions where
  the actual provider supports the required atomic contract.
- Existing bootstrap providers, coordinated SQLite, composites, query chains,
  Strict/AutoReset, cancellation-safe cleanup, replacement and deletion.
- Core future evaluation and the existing scheduling/test-clock resources.
- Ordinary snapshots, outbox, generation, channel/broadcast subscriptions,
  reactions, coherent committed-progress metrics and canonical output gauges.
- Retained recovery views after completed cleanup, using the existing consumer
  recovery policies and handled-checkpoint sink.
- Paged native snapshot/history and snapshot/live attachment, including results
  larger than the former whole-response row/envelope/byte limits.

The profiles above are implemented and
[CW-43](computation-graph-remaining-work.md#cw-43) is closed. Remaining payload,
native-test, full-suite, performance and reference-retirement work is tracked
separately in the [canonical backlog](computation-graph-remaining-work.md#cw-76).
The linked reference constructors still exist; removing implicit selection does
not mean they have been deleted. General non-query scheduling is a separate
implemented capability with its own atomic-persistent profile, not missing query
temporal-function support.

For runnable registration and profile details, see the
[query plugin guide](../../components/computation-plugins/transformers/query/README.md).

## Ownership and reuse

| Concern | Owner |
|---|---|
| Parsing, matching, aggregation, temporal functions and query evaluation | Existing Core evaluator |
| Definition/fingerprint, configuration, result projection/history and read/recovery helpers | `drasi-query-common` |
| Evaluator lifetime and query-specific processing | Selected query component |
| Provider lifetime, transaction gate, shared-domain identity, commit/rollback and cleanup | Actual Host/Core resource owner |
| Component tasks, routing, input/output binding and lifecycle | Existing ComputationGraph |
| Provider preparation and per-input snapshot/live handover | Existing bootstrap/provider coordination |
| Committed deadline observation and physical-clock controls | Existing query scheduling resource and source |
| Ordinary subscriptions, reaction delivery and discovery | Existing graph-owned catalogue, outlet and API facades |
| Loading, native handles, bounded requests and serialization | Host SDK and native SDK/ABI |

The factory owns stable index adapters. Activation creates one Core evaluator
through `QueryBuilder`; bootstrap, live input and due work reuse it. Each
processing operation temporarily binds its revocable Host context. Completion,
failure and cancellation revoke that access, and an old lazy reader cannot
follow a later operation. No `evaluate_query` Host RPC is permitted.

Persistent stop releases the evaluator after cleanup; activation rebuilds it
against recovered storage, not once per input. Healthy volatile quiescence
retains the actual evaluator and indexes. Failed state cannot silently restart
as an empty query.

Plugin-owned middleware registries remain inside the selected library. The
stock plugin reuses the existing middleware implementations. A linked definition
can receive an injected registry; a host-process Rust registry cannot cross a
cdylib boundary.

`drasi-lib` must not depend on the Host SDK, which already depends on the library.
The query plugin must not depend on `drasi-lib` or the legacy plugin SDK. Query
authoring is a factory capability, not a privileged graph execution role.

## Processing and recovery contracts

### Input, state and output

The actual provider determines the supported guarantees. Matching names,
database paths or backend labels do not establish transaction participation.
Atomic publication commits input checkpoints, indexes, future work, canonical
rows, retained output and the logical query sequence in one actual domain.
Shared journal capacity is reserved before its commit gate, and publication
occurs after the physical commit while that gate is still held.

The native adapter delegates live input admission to the existing
`GraphInputProgress` rules. Producer epochs, logical versus transport sequence,
opaque positions, duplicate input and source-transaction framing are not
reimplemented. A prepared input must be staged before successful completion.

Producer incarnation, logical query sequence and transport emission sequence
are distinct. Replaying committed output never reevaluates input or reapplies
rows. Ordinary replacement retains the actual catalogue transport counter, so
new instances cannot reuse an old stream sequence. Delivery confirmation is a
separate retained obligation; unconfirmed output cannot be overwritten.

Persistent non-atomic support reuses the existing
`query_recovery.rs::begin_non_atomic` / `finish_non_atomic` protocol through the
shared `QueryPublicationCheckpoint` and `QueryResetMarker`. Preserve its
evaluation marker, pending publication, staged input and recovery behavior.
Do not grant an atomic token to independent writers, publish consumed-input
progress before output completion, or silently downgrade an incomplete provider.

### Bootstrap and cleanup

Bootstrap is a configurable source/transformer capability, not a query-only
execution phase. Starting empty does not acquire a provider merely because a
component supports initialization.

Select one initialization plan for each input: an upstream output adapter or an
explicit provider, including composite plans. Bind that plan to the actual live
producer/port and its cursor/epoch. Prepare lossless retention before capture;
bounded buffering or durable replay must preserve the uncovered suffix.

For `source -> transformer -> query`, initial data must pass through the
transformer's behavior before evaluation. Do not bypass it by independently
reading the source snapshot, or consume both that snapshot and the transformed
copy. Multiple inputs keep their own frontiers; these are not distributed
snapshots unless the actual providers establish that guarantee.

Recover progress, prepare the provider, apply initial records, commit completion
state/watermarks, then admit uncovered live input. Partial bootstrap is not
readable completed output, and snapshot ordinals are not source checkpoints.
Provider intent must survive interruption and supported reset.

Strict/AutoReset, requested refresh and explicit deletion retain the existing
ownership rules. An I/O error is not permission to erase state. Pending output,
durable destinations and completed provider handovers may require explicit
retirement. Stop/deletion must await owned cleanup, retain timed-out work and
block unsafe restart; no detached worker is a cleanup success.

Identified malformed rows or retained output use the same typed AutoReset
classification as the linked query. The first attempt remains read-only until
validation completes; unknown storage failures and foreign ownership remain
errors. Authorized rebuilding retains the logical high-water mark in a persisted
reset baseline, independently of transport emissions, so existing consumer
checkpoints can accept the replacement snapshot. Baseline initialization is a
one-use Host grant within the actual bootstrap reset operation, never a way to
acknowledge or bypass pending live delivery.

### Scheduling

Query scheduling uses Core's existing future queue and reevaluation. Queue
mutation belongs to the query's processing transaction; outside processing,
the existing scheduling source observes committed deadlines without deleting
work. The owner consumes/evaluates a due item and commits its result. Duplicate
wakeups cannot reapply completed work.

Preserve `FutureElementRef`, cancellation, original source provenance, logical
due time and production timestamps. The existing physical-clock/test-control
frontier waits through actual due-work and delivery confirmation, not merely
queue admission or a wall-clock sleep.

Reusable non-query scheduled work is implemented in
[CW-81](computation-graph-remaining-work.md#cw-81). Request/replace/cancel,
claim and explicit completion enlist in the requesting owner's actual
transaction, never a competing session on the same database path.
It shares `CommittedSchedule`/`FutureWakeup` in computation-contracts and preserves
query-common re-exports. Its standalone RocksDB process-restart cases qualify
that contract; no-op memory and non-atomic bundles reject the capability.
[CW-82](computation-graph-remaining-work.md#cw-82) owns the mixed query/non-query
proof; query scheduling alone does not establish that broader contract.

### Results, subscriptions and diagnostics

`QueryOutputOwner` mutates the canonical state; `QueryResults` is read-only.
Native readers capture one coherent observation. Snapshot rows and retained
history must have the same head, scope, configuration and generation.

`output_reads_v2` and `output_follow_v2` transfer that observation in bounded
pages. Individual records/envelopes still have the native frame bound.
Page order, counts, schemas and identities are checked; a failed transfer never
becomes partial success. Stop/replacement revokes captured transfers and releases
provider retention. Paging introduces no Host row mirror or polling task.

Ordinary indefinite subscriptions continue through the existing catalogue and
outlet, not renewing short native follow leases. Preserve channel backpressure,
broadcast replay/gaps, receiver-before-snapshot attachment, cancellation,
bootstrap readiness and replacement fencing. Reaction recovery uses one
coherent view rather than sampling rows and history separately.

Committed-progress metrics retain only actual input/output metadata. No-op
input advances its input frontier without inventing output. Canonical row/outbox
gauges initialize from bootstrap or recovered state and update in query-output
replies; replay does not advance the logical-result counter. Operation timing and
snapshot-fetch counting use the existing library metrics.

An optional recovery reader exposes positively retained canonical state after
both plugin stop and actual Host cleanup complete. It never enables ordinary
inactive snapshots or live leases, and failed/cancelled cleanup remains fenced.
`QueryReplayTransformer` accepts the same read-only contract for shared and native
query state and retains its existing recovery policies and checkpointed sinks.
Contiguous live delivery uses metadata-only heads, not full row transfers.
Notification-driven waits survive clean stop/start; old transfers and removed
owners remain fenced.

<a id="work-packages-and-completion-gates"></a>

## Implementation status and follow-ups

[CW-43](computation-graph-remaining-work.md#cw-43) records completed native query
integration and the exact scope of its qualification evidence. Evaluator reuse,
ordinary factory selection, supported storage/publication profiles, bootstrap,
scheduling, readers, metrics and retained recovery are implemented. Architecture
artifacts have been regenerated and checked; rerunning their generator after a
future source change is maintenance, not an outstanding query implementation step.

This guide describes contracts, not a second work checklist. Open items and
completion criteria live only in the remaining-work document:

| Follow-up | Canonical item |
|---|---|
| Fragment individually oversized native records/envelopes | [CW-76](computation-graph-remaining-work.md#cw-76) |
| Fill missing native behavioral-parity coverage | [CW-77](computation-graph-remaining-work.md#cw-77) |
| Execute the complete existing current-code test matrix | [CW-78](computation-graph-remaining-work.md#cw-78) |
| Measure native-query overhead against the linked baseline | [CW-79](computation-graph-remaining-work.md#cw-79) |
| Retire remaining linked query runtime entry points | [CW-80](computation-graph-remaining-work.md#cw-80) |

The explicit reference remains preservation evidence, not an automatic
production fallback. Some successful preservation tests still use it. Their
assertions must be retained, and their results must not be labeled native
acceptance without an actual native execution path.

### F0: combined proof

The mixed source/bootstrap, non-query transformer, query and capture-consumer
proof is [CW-82](computation-graph-remaining-work.md#cw-82). It depends on the
implemented reusable non-query scheduling contract in
[CW-81](computation-graph-remaining-work.md#cw-81). The remaining mixed ownership, rollback,
clock and real-process failure checks qualify that new combined profile; they
do not keep the implemented query plugin open or imply that query timers are
unavailable.

### Preservation matrix

| Behavior | Existing targets to reuse | Required evidence |
|---|---|---|
| Configuration/evaluation | `computation_query_configuration`, `computation_queries`, native joins/middleware/archive cases | Same scopes, mutation order, source interests, typed keys and error timing |
| Atomic faults | `computation_query_faults`, `query_output_atomicity` | Indexes, checkpoints, outbox, rows and sequence agree at each boundary |
| Replay/shared journals | `computation_transaction_query`, `computation_qos`, native destinations | No reevaluation or lost fanout; actual group identity and retained obligations |
| Source transactions | `computation_source_transactions` | Event versus net-transaction output, limits and logical/transport separation |
| Bootstrap/reset | `computation_query_recovery`, `query_output_wipe`, native bootstrap/crash cases | Completion/cursors, incomplete markers, new generations and no erased obligations |
| Timers | `query_test_control`, `computation_temporal_retractions`, native scheduling | Actual commit/delivery frontier, independent clocks and no stale retraction |
| Reads | `query_result_subscriptions`, Host read/follow and native large-result cases | Complete data, coherent heads, no snapshot/live gaps and cleanup on cancellation |
| Ordinary APIs | `component_api_parity`, `computation_api_mutations`, native ordinary cases, Server creation tests | Explicit selected factory, absent-plugin errors, add/update/clone/remove and retained configuration |

## Focused execution and final qualification

Use the existing checkouts and branches. Do not create sessions, worktrees,
clones or branches for this effort. Serialize Cargo commands sharing a target.
Implement one failing native case, then run its focused group and the directly
affected preservation targets; do not rerun every connector after each edit.

`make build-test-plugins` rebuilds and stages matching shared libraries.
`cargo test --locked -p drasi-host-sdk --test native_query_plugin -- --test-threads=1`
runs the native suite, including ordinary APIs, large reads and test clocks.
Use Cargo's substring filters for focused cases; the plugin README lists useful
filters. Run the affected library preservation targets separately. Linked
reference tests do not establish loaded-plugin parity. These commands do not
install dependencies, start containers or run Server.

Use [CW-77](computation-graph-remaining-work.md#cw-77) for missing native cases
and [CW-78](computation-graph-remaining-work.md#cw-78) for full-suite execution.
Record default/no-default and relevant middleware, test-support and backend
profiles, both runtimes, linked native definitions and actual cdylibs. Preserve
process-exit drivers and the supported Garnet/RocksDB, atomic/non-atomic,
shared-journal and cleanup contracts. Unavailable external backends are blocked
gates, not passes or new ignored tests.

Performance comparisons and their operating bounds belong to
[CW-79](computation-graph-remaining-work.md#cw-79). Do not invent a performance
budget, compare different storage guarantees or optimize around a correctness
failure.

Direct Server registration is implemented because ordinary creation requires
it. Broader Server composition, deployment and unrelated connector migrations
remain separate work in the canonical backlog. Published package availability
and migration documentation remain
[CW-73](computation-graph-remaining-work.md#cw-73); a successful matched local
build is not evidence that the plugin is already distributed in a release.
