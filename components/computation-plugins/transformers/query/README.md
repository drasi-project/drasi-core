# Native query transformer

This plugin runs ordinary Cypher and GQL queries using the existing Drasi Core
evaluator and shared query implementation. Register its indexed factory to use
it through `DrasiLib`, computation pipelines and Server query APIs. There is no
implicit query factory or fallback evaluator. The optional `language` is
`cypher` (default) or `gql`.

- `drasi.experimental/volatile-query` requires `query` and `volatile: true`.
  It exposes a current-result snapshot on `out`. Its indexes, result rows and
  output sequence are discarded on stop; restart uses a new volatile output epoch.
- `drasi.experimental/indexed-query` requires `query` and a host-provided
  `processing` resource. The ordinary-query adapter supplies the actual selected
  index provider. In-memory providers use the explicitly volatile profile and
  retain their evaluator and state across healthy stop/start. Persistent providers
  use atomic publication by default, requiring a proven atomic result domain and
  process-restart durability. Explicit non-atomic publication is described below.
  The persistent profile commits indexes, source progress, current
  result rows and retained output together. It restores the result snapshot
  and replays unconfirmed output after restart. With `bootstrap: true`, bind an
  existing provider as `bootstrap`; initialization is durable and remains hidden
  until the provider finishes and its completion state and watermarks commit.
  Simple, grouped, minimum-aggregate, GQL and relationship queries run through
  the loaded library. Archive queries use the actual provider's archive index.
- `drasi.experimental/provider-query` requires `query`, `volatile: true` and a
  host-bound `bootstrap` resource containing an existing bootstrap provider.
  It is the quiescent file-provider integration path described below.

The factories use the query definition, Core builder configuration, canonical
result state and readers extracted from the built-in implementation into
`drasi-query-common`. They do not maintain a separate projection/history engine.
The native adapter retains the actual host-owned storage and progress boundary.

The indexed profile supports ordinary query creation/replacement/removal,
bootstrap, archives, synthetic joins, input-local middleware, scheduled work,
test clocks, snapshots, retained history, subscriptions and reactions.
Persistent non-atomic publication uses the existing pending-publication and
reset protocol, without claiming atomicity for independent writers.
Output gauges, logical sequence counters and operation timings use the existing
library metrics. Recovery consumers can read the canonical state after completed
stop, without restarting the evaluator or reopening ordinary read services.

## Registering ordinary queries

The host explicitly chooses the implementation. For a separately built stock
query library:

```rust
use drasi_host_sdk::computation;
use drasi_lib::DrasiLib;

async fn build(path: &std::path::Path) -> anyhow::Result<DrasiLib> {
    let plugin = computation::load(path)?;
    Ok(DrasiLib::builder()
        .with_query_factory(plugin.query_factory()?)
        .build()
        .await?)
}
```

Add sources and queries with the normal builder or runtime APIs. An empty or
source-only instance needs no query factory; creating an ordinary query without
one returns `QueryFactoryNotRegistered`. An existing instance can register its
factory once with `register_query_factory`. Explicit computation pipelines use
`query_factory`; `DrasiLib::computation_pipeline()` inherits the instance's choice.
The ordinary wrapper has the distinct implementation name
`drasi.experimental/indexed-query/ordinary`, because its authoring schema differs
from the raw plugin factory's schema.

`NativePlugin::register_factories` and
`PluginRegistry::computation_factory_registry` include that supported authoring
identity as well as the raw native factories. Saved specifications can therefore
be reconstructed and inspected through the same registry. Registration alone
does not select an ordinary-query default; the explicit instance/pipeline
selection above is still required.

`PluginRegistry::query_factory()` resolves one eligible loaded factory and rejects
ambiguity. Server uses it before adding configured queries and for API-created
queries on instances without a selected factory. No new YAML query-factory field
or automatic installation of an unpublished query plugin is implied. Programmatic
Server hosts can supply `DrasiServerBuilder::with_query_factory`.

Statically linked hosts can use `NativePlugin::from_definition(plugin()?)?` and
register the same factory through the validated native ABI. `drasi-lib` itself
does not depend on the Host SDK or bundle a query plugin.

All factories accept optional `stream`, `result: {"label": "..."}` and positive
`outbox_capacity` settings. The default stream is `<component>/out`; native
retention defaults to one output. Result datasets use the existing
instance/graph/component/output identity calculation. Retention capacity is an
operational setting and does not change the query's semantic fingerprint.

## Output metrics and retained recovery

The plugin supplies metadata-only observations of its actual row count, retained
outbox size/bounds and committed generation. Initialization sets the gauges before
live input, including bootstrap and restored state. Transform replies carry the
updated observation without an extra per-input read RPC. Logical sequence advances
exclude replay and no-op input; timers and snapshot-fetch counts reuse the
library's existing metrics. A query provider without this capability exposes no
output metrics rather than meaningful-looking zero gauges.

`QueryApi::recovery_reader()` exposes the existing recovery reader contract.
`QueryReplayTransformer::with_reader` and `QueryResultsResource(reader)` accept
that reader and reuse the same Strict, AutoReset and AutoSkipGap consumer policy
and checkpointed sink implementation. Contiguous live delivery reads only
metadata; it does not transfer a complete result snapshot for each input.

Recovery views remain readable after both plugin stop and actual Host resource
cleanup complete. Incomplete/cancelled cleanup and uncertain query state remain
unreadable. Ordinary snapshots, history reads and live leases stay fenced while
stopped. Restart cancels earlier transfers, and replacement or deletion retires
old readers. Waiting consumers use publication/lifecycle notifications, not a
polling task or a Host-side row mirror.

Use `bash lib/tests/run-query-plugin.sh retained-recovery` for these native
boundaries and the existing consumer-policy preservation baseline.

## Evaluator lifetime

The indexed factory creates one query instance with stable index adapters.
Activation builds and retains one `drasi-core::query::QueryEvaluator` through
Core's existing `QueryBuilder`. Bootstrap batches and live changes use that same
evaluator, parser-built plan, function registry and join configuration. They do
not rebuild the query or introduce separate evaluation algorithms.

Each host-owned processing operation temporarily binds its revocable context to
the adapters. Completion, failure and cancellation release that binding. Lazy
slot readers remain tied to the operation that opened them, and cannot follow a
later binding. The retained evaluator keeps no completed processing context alive.

Persistent stop releases the evaluator after cleanup; the next activation
initializes it against recovered storage. A healthy volatile stop retains the
same actual evaluator and indexes for the instance's lifetime. Failed state
cannot silently restart as an empty query. A provider-requested AutoReset during
activation reuses the evaluator and clears Core's shadow future queue with the
owned storage reset.
Scheduled work uses the same Core evaluator and the same scoped indexes.

From the Core checkout, `bash lib/tests/run-query-plugin.sh evaluator` exercises
scoped access, actual loaded Core setup counts, SQLite bootstrap, live changes,
restart, prepared refresh and setup/processing failures.

## Scheduling and historical reads

Queries using Core temporal functions run through the existing graph wakeup and
continuation hooks without needing another source event. Core owns the future
queue and evaluates each due reference; there is no replacement timer engine.
The native adapter observes the next deadline only after the actual queue
transaction commits. Pending output is delivered before more due work, and
persistent restart restores both queued futures and unconfirmed scheduled output.
Cancelled/deleted inputs cannot emit stale results, and future work does not run
before its deadline.

`drasi.getVersionByTimestamp` and `drasi.getVersionsByTimeRange` use the host's
actual archive provider. Enable archive storage on that provider (for example
`RocksIndexOptions::new(true, ...)`); a disabled archive returns an error, not
empty history. Ranged reads use the existing bounded element-stream transport
and cannot outlive or switch their processing scope.

Use the `scheduling` and `archives` runner groups for these loaded-library paths.

## Input-local middleware

`inputs` and `middleware` use the existing query execution settings. Core builds
and runs its existing middleware pipelines; the plugin does not implement their
transformations separately. For example:

```json
{
  "query": "MATCH (n:Selected) RETURN n.value AS value",
  "inputs": [{"component": "source", "pipeline": ["labels"]}],
  "middleware": [{
    "name": "labels",
    "kind": "relabel",
    "config": {"labelMappings": {"Item": "Selected"}}
  }]
}
```

Graph input bindings select the pipeline by its actual producer, including
ordinary source aliases and upstream query outputs whose record namespace is
different from their component name. Bootstrap uses the provider's declared
input origin and restores live bindings afterward; element identities are not
rewritten to select a pipeline.

The default query-plugin build includes map, relabel, unwind, promote, parse-json
and decoder middleware. Enable `middleware-jq` for JQ, or
`middleware-bundled-jq` when bundling it. A custom query library can pass its Core
registry to `plugin_with_middleware_registry`; factories and pipeline instances
are retained rather than recreated per input. Host-process Rust registry entries
are not automatically transferred across a dynamically loaded library boundary.
Unknown kinds fail construction before indexes are opened.

`bash lib/tests/run-query-plugin.sh middleware` compares loaded and linked query
behavior and covers aliases, derived query input, bootstrap origins and an actual
custom query cdylib whose middleware reads the query's scoped indexes.

## Configured synthetic joins

All three factories accept an optional `joins` array using the existing
`QueryJoinConfig` format. A synthetic join matches properties on nodes without
requiring relationship records from their sources:

```json
{
  "query": "MATCH (o:Order)-[:PLACED_BY]->(c:Customer) RETURN o.id AS order_id, c.name AS customer",
  "volatile": true,
  "joins": [
    {
      "id": "PLACED_BY",
      "keys": [
        { "label": "Order", "property": "customer_id" },
        { "label": "Customer", "property": "id" }
      ]
    }
  ]
}
```

For the indexed factory, omit `volatile` and bind its processing resource as
usual. Provider and upstream-snapshot bootstrap use the same configured joins
as live processing. The factory uses the shared ordinary-query validation:
unique join IDs, at least two keys, and nonempty identifiers without whitespace
or control characters. Invalid joins fail construction before opening storage.

The plugin calls Core's `QueryBuilder::with_joins` when constructing its retained
`QueryEvaluator`; Core supplies all join behavior.
No matching, relationship-identity or index algorithm is reimplemented in the
plugin. Join and key order do not affect retained configuration identity;
omitting `joins` and supplying an empty array are equivalent. Changed join IDs,
membership, labels or properties are configuration changes: Strict recovery
rejects retained state. Where the provider permits refresh, configured AutoReset
rebuilds through it and starts a new output epoch. The currently integrated
coordinated SQLite profile requires explicit retirement of a completed handover;
AutoReset does not bypass that ownership requirement.

`bash lib/tests/run-query-plugin.sh joins` covers the shared settings, comparison
with the built-in query, real SQLite bootstrap/live transactions, restart,
equivalent reordered settings and incompatible configuration changes.

## Indexed restart and output handoff

The indexed factory uses the existing evaluator and host-owned transaction.
Its first persistence profile accepts ordinary raw-source batches, with
restart-stable, monotonic sequence numbers per input port/stream. The caller must
resume those sources correctly; a source that restarts its counter cannot safely
be reattached to retained checkpoints. Complete source transactions require
`source_transactions` limits (`max_changes`, `max_bytes`, `max_duration_ms`).
They use the existing evaluator's net-transaction projection, preserve opaque
source positions, and track logical source sequence separately from replay
transport sequence. Persistent producer input is bound to its exact epoch and
logical cursor. Missing/changing producer identity or a changed source/stream
progress protocol is an error. Scheduled work uses the actual query future queue.

Each successful operation commits index mutations and consumed-input progress
with its live-result row mutations, row count, encoded output and result sequence.
This is the atomic profile's contract; non-atomic publication uses the separate
phases below.
Only one output may await graph
handoff. No subsequent input can overwrite that obligation. The existing
`delivery_completed` hook records confirmation; direct callers must invoke it
after handling the returned output, just as the graph does.

Startup validates the stored configuration and scope, restores the persistent
producer identity and counters, and schedules replay before new input. A replay
has the same logical query sequence, rows and producer epoch, but a new persisted
transport emission number. A committed duplicate input does not run the evaluator
again. Missing/corrupt state is never adopted as an empty result. Recovery defaults
to `strict`. With `bootstrap: true` and a bound provider, `recovery: "auto_reset"`
can restart interrupted initialization, refresh a requested snapshot, or rebuild
identified malformed rows/output, a retained-history gap or changed configuration.
It reuses the shared typed recovery classification and ownership checks; an I/O
failure or unidentified state is not permission to clear storage. It preserves
unfinished provider intent, advances the output epoch and retains the logical
and transport high-water marks separately. A persisted reset baseline allows
healthy restart before any new output and prevents consumer checkpoint regression.
Unconfirmed output, bound durable destinations and foreign ownership block
reset. Provider-specific retirement requirements still apply. Recovery policy
itself does not change the query's stored identity.
Resetting a projection does not acknowledge pending delivery. The Host admits
baseline initialization only inside the operation that performed the authorized
bootstrap reset, before readiness and without bound destinations.
Stored configuration includes the state format. An incompatible store is
rejected rather than attempting to reconstruct missing rows from incomplete
output history; use fresh storage when changing that format.

The indexed factory exposes the same `out` snapshot contract as the volatile
factory. Startup reads the existing live-result store in bounded pages, validates
row identities and the committed row count, and publishes the restored map only
after the host operation succeeds. The Host does not keep another projection.
Snapshot cursors use the persistent producer and logical query sequence, not
the transport emission counter. Replaying a committed output therefore does not
change the snapshot or reapply its rows.

Storage restoration and public native snapshot/history transfers are paged.
Backends must implement bounded
`LiveResultsWriter::read_page`; an unsupported method is an error, not an empty
snapshot. The RocksDB implementation is qualified here.

The [durability cases](../../../host-sdk/tests/native_query_plugin/durable.rs)
cover clean recreation, duplicate input, rejected configuration/confirmation,
and an actual process exit after commit without delivery or cleanup. On both
Tokio runtimes, the graph replays that pending output even when no new input
arrives, confirms it, and continues from the retained aggregate state. Host
boundary cases separately prove that indexes, checkpoints, live rows,
result-sequence and outbox changes roll back together.

The [snapshot cases](../../../host-sdk/tests/native_query_plugin/durable_snapshots.rs)
create 260 rows, update and delete rows, then exit without cleanup or confirming
the last output. Both runtimes restore the complete multi-page projection even
though only the last output remains in the outbox. They also cover clean reopen,
unchanged snapshots during replay, logical versus transport sequence, missing or
corrupt rows, and snapshot unavailability after a failed mutation. Shared codec
helpers keep built-in and plugin row persistence/rehydration identical.

`out` exposes retained replay and bounded snapshot/follow leases through the
same result implementation as the built-in query. Requests outside retained
history return an explicit gap. A coherent snapshot/follow handoff includes
only later changes; cancellation does not consume an item, and stop retires
old leases. This is not a replacement for the ordinary library's indefinite
channel/broadcast subscription contract.

Native query read metadata also connects these components to ordinary library
result APIs: query configuration/results inspection and `Query::fetch_snapshot`,
`fetch_outbox` and `output_generation` read the plugin's own state. The Host
adapts the result exchange types; it neither runs an evaluator nor mirrors rows.
Ordinary query creation uses its explicitly registered factory; no linked
reference implementation is silently substituted.

Snapshot, replay, coherent query-view and snapshot/follow reads transfer one
captured observation in bounded pages. Total results are not limited to 16,384
rows, 1,024 retained envelopes or a 16 MiB response. Each page and individual
record/envelope must still fit the native 16 MiB frame budget. Pages normally
target 1 MiB and contain at most 1,024 items. Counts, identities, page order and
the common snapshot/history head are validated; failures never return a partial
success. The host assembles only the caller's response, not another result store.
Cancellation and stop release the captured transfer, and an old transfer cannot
continue after restart or replacement.

### Ordinary live subscriptions and reactions

The Host can bind the existing graph-owned `QueryResultsCatalog` before
registration/activation with
`NativeComponentProxy::bind_query_catalog(catalog, dispatch_mode, capacity)`.
Register that same catalogue as the resource of a `QueryResultsOutletFactory`
component and connect the query's `out` port to the outlet. Merely exposing
native read providers does not create a subscription path.

With that actual connection, `Query::subscribe`, `subscribe_query_results` and
ordinary reactions use the existing library delivery implementation. Channel
subscribers backpressure publication; broadcast subscribers recover lag from
retained history or report an explicit gap. Snapshot attachment registers the
receiver before awaiting its native head, so intervening output cannot be lost.
Cancellation removes the provisional receiver. Snapshot-only reads do not
transfer retained history; reaction recovery uses a separately bounded coherent
view of rows and history from one plugin publication.
Ordinary snapshot/outbox reads and new subscriptions wait for native bootstrap
activation within the configured overall read deadline, rather than issuing an
inactive native read or exposing partial initialization.

Subscriptions can wait through stop/start without reopening native read services
while stopped. A correctly coordinated persistent restart preserves the same
query epoch; replacement, reset or shutdown invalidates stale readers.
The coordinated SQLite source and its sole query-progress owner still need their
existing paired lifecycle; the result adapter does not take over source ownership.

`bash lib/tests/run-query-plugin.sh subscriptions` exercises the existing ordinary
contracts and actual loaded-query channel/broadcast, bootstrap waits, restart, snapshot/live races
and HTTP-reaction delivery on both Tokio runtimes. It also checks cancellation,
read deadlines and malformed native replies. No renewing follow lease,
forwarding worker or Host-side row cache implements these subscriptions.

The current native state format is **8**. Earlier experimental stores must not
be adopted as the new scoped result representation; use fresh storage or the
explicitly supported provider recovery path. No automatic state migration is
provided.

The Host persists actual receipt-tracked output destinations and their
fingerprint with delivery progress. Pending output cannot be redirected by
changing a consumer, port, subscriber or journal UUID. Matching names do not
authorize a replacement journal. After restart the actual destinations must be
rebound before processing/replay. Normal graph binding does this automatically.

With an actual `SharedStorageGroup`, query state and all shared journal appends
use the same transaction. Capacity is reserved before acquiring its commit gate;
committed progress and journal caches publish under that gate. This is actual
group membership, not matching database paths. Separate persistent journals use
retained output and replay receipts instead. Both paths preserve pending fanout
across process exit; neither promises exactly-once external effects.

Explicit `delete_owned_resources` after stop removes owned indexes, results, checkpoints,
outbox and provider handover state. Durable destinations must first be drained
and unbound. Failed deletion retains cleanup and can be retried; a deleted
instance cannot reactivate. Disposal alone does not delete persistent state.
A recreated instance receives a new producer epoch.
Scheduled-work processing remains separate work.

### Explicit non-atomic publication

Ordinary authoring passes the selected provider/publication mode to the native
factory. A raw indexed query uses `publication: "non_atomic"` together with
`NativeFactory::create_component_with_processing_publication(...,
QueryPublicationMode::NonAtomic)`. A declared graph binds
`NativeProcessingResource::new(provider).with_publication(QueryPublicationMode::NonAtomic)`.
The default remains atomic; incomplete or mismatched bindings are errors, never
automatic downgrades.

The plugin reuses `QueryPublicationCheckpoint` and `QueryResetMarker`, extracted
from the linked implementation. It validates input before marking evaluation,
commits that marker, commits Core evaluation/input state with the prepared
output sequence, then publishes output and clears the marker. Only the final
successful phase makes consumed-input progress visible. Duplicate input advances
transport progress without reevaluation; no-op input does not invent output.

Independent writes can survive a failed publication. Strict recovery reports the
pending publication instead of adopting partial rows/history. AutoReset requires
an existing bootstrap provider and retains a reset-intent record outside the
checkpoint namespace being cleared. Reset/deletion keep numeric generations
monotonic; failed or interrupted clearing cannot become a fresh empty query.
Checkpoint stores must support staged clearing. Both RocksDB and Garnet adapters
forward this through their existing session mechanisms.

Memory, atomic and non-atomic profiles use the same evaluator, canonical result
state and scheduling behavior. Non-atomic publication cannot bind shared
transaction-group destinations or claim complete source-transaction atomicity.
Provider-specific bootstrap/retirement requirements still apply.

`bash lib/tests/run-query-plugin.sh non-atomic` covers ordinary APIs, declared
resources, independent persistent writers, partial publication, actual process
exit, provider-backed reset, numeric generation, timers, replay and deletion.

## Committed input-progress view

The indexed factory opts into the Host's `committed_input_progress` capability.
Embedded native callers obtain its `SourceProgressView` from
`NativeComponentProxy::committed_input_progress()`. This is a read of the actual
processing owner's checkpoints, not the graph's volatile handled-input tracker
or a same-named substitute.

During startup, `wait_recovered()` can observe restored checkpoints before
readiness. `wait_ready()` waits for successful initialization. Empty checkpoint
maps mean no input has committed; a real sequence-zero checkpoint and its opaque
position remain distinguishable from absence. Persistent updates appear only
after the atomic transaction or explicit non-atomic publication protocol completes.
The volatile profile publishes successful work without claiming rollback or
process-restart durability. The view
supports independent subscriptions; cancellation does not consume an update.

Failure or cancellation leaves the last committed positions unchanged and
marks the view failed/not ready. Stop revokes old views and subscriptions;
obtain a fresh view for the new activation. View handles keep no index/store
owner alive. The raw `volatile-query` and `provider-query` factories do not allocate this publisher.
The indexed factory does allocate it for ordinary memory queries, using the
actual graph-provided progress owner. Server progress metrics observe that
committed input/output pair, including no-op input advances.

The view itself remains read-only. Host construction can separately obtain
`processing_progress_owner()` and bind that exact local owner to an existing
source/coordinator/provider. Both interfaces share one canonical publication;
matching component names never substitute for owner identity.
Factory-created indexed components prepare actual resources before the graph
captures recovery evidence. Direct constructors can call `prepare_processing()`
before adding a component to a graph with asserted recovery requirements;
preparation does not admit native processing or publish recovered data.
The [loaded progress cases](../../../host-sdk/tests/native_query_plugin/durable_progress.rs)
and Host storage-boundary cases cover publication after commit, pre-ready
recovery, independent streams, real zero positions, rollback/cancellation,
readiness changes during work, and reader revocation on both runtimes.

## Durable provider bootstrap

The indexed factory declares `durable_bootstrap`. The Host supplies the actual
processing owner's `BootstrapState` to the existing provider's
`prepare_with_state` and `snapshot_with_state` methods. Provider intent is durable
before external initialization. Streamed rows commit incrementally but remain
unreadable; only the final atomic completion makes the snapshot available.
Snapshot row ordinals are never promoted to consumed source positions.

The completed marker, real watermarks and bounded provider completion state
commit together after awaited snapshot cleanup. A completed restart prepares the
provider against recovered progress but does not request another snapshot.
Incomplete initialization is rejected under Strict recovery. AutoReset clears
partial rows/indexes atomically while retaining real provider intent; neither
partial rows nor snapshot item ordinals become published progress.

The loaded [file bootstrap](../../../host-sdk/tests/native_query_plugin/durable_bootstrap.rs)
and [coordinated SQLite](../../../host-sdk/tests/native_query_plugin/durable_sqlite.rs)
cases use the existing native provider libraries. The coordinated case binds
`drasi.sqlite/replay-transactions` and `drasi.sqlite/coordinated-bootstrap` to the
indexed query's actual local progress owner. It proves source watermark zero
with opaque journal identity, a real SQL batch through the source-owned API,
net aggregate output, completed reopen and another live transaction on both
Tokio runtimes. Additional cases interrupt loading after a committed row, hold
cleanup across a caller deadline, and exit real processes during incomplete or
completed initialization. Retried coordinated loading retains the original
provider epoch rather than inventing a new owner.

Existing `CompositeBootstrapProvider` combines providers without inventing
watermarks or handover state for recordless providers. `OutputBootstrapProvider`
initializes durable query chains, waits for upstream readiness, and supplies
retained input replay before buffered live input. Covered logical positions are
not evaluated twice; AutoReset can recover a history gap with a fresh snapshot.
These are per-input cuts, not a globally atomic distributed snapshot.

The standard native middleware transformer also accepts optional quiescent
bootstrap. Its initial and live records pass through the same map/relabel
pipeline before a downstream query snapshots its output.

## Upstream snapshot bootstrap

The volatile factory can initialize its evaluator from configured upstream
output snapshots before accepting live changes:

```json
{
  "query": "MATCH (o:Order), (c:Customer) WHERE o.customerId = c.id RETURN o.id AS orderId, c.name AS customer, o.amount AS amount",
  "volatile": true,
  "bootstrap": [
    { "component": "orders", "port": "data" },
    { "component": "customers", "port": "data" }
  ]
}
```

Bind a host `NativeOutputSnapshotsResource` as the `output_snapshots` dependency
and allow each configured output. The nonempty bootstrap list must cover every
declared incoming edge exactly once. Inputs can share the `in` port but must
have distinct streams there. Unknown, duplicate, missing and ambiguous bindings
are rejected before initialization. The previous experimental single-object
form is rejected; use a one-element array for one upstream.

The same generic snapshot service streams each input's full-image records into
one evaluator, in configured order. Initial results stay private until every
snapshot finishes. A later snapshot failure does not start the query or publish
the partial initialization.

Each input port/stream has its own captured producer identity and logical
position. Live input must carry matching `GraphProducerProgress`. Covered
positions are ignored independently; missing progress, an undeclared stream or
a changed epoch is an error. The host must retain each live input before
capturing its snapshot: this read service does not create atomic snapshot/live
subscriptions itself.

This remains deliberately volatile. Initial evaluation does not emit live
result events or consume output sequence numbers.
Instead, `out` exposes the completed initial result snapshot at logical sequence
zero. Later completed changes update those same rows and the snapshot cursor;
live envelopes carry that cursor's producer identity and logical sequence.
Failed or stopped processing does not expose a ready snapshot.

A second volatile query can select the first query's `out` as its bootstrap
endpoint, then consume its live output through the graph. Initial aggregate
results contain the final rows, not every intermediate aggregate update during
snapshot loading.

The running-upstream case is covered with an already-bound, lossless live edge.
The test holds a real native snapshot after capture while the upstream query
continues processing, fills the bounded queues, then completes initialization.
Queued positions covered by the snapshot are skipped, and all later inserts,
updates and deletes match an uninterrupted reference query. The same case runs
on both Tokio flavors. The existing handover logic passes without a separate
runtime implementation for this scenario.

The two-input proof uses snapshot positions 100 and 3, then live changes on
both streams, including customer position 4 after order position 101. It also
holds the second snapshot while the first input produces live changes, verifies
that no partial result is readable, and checks those changes after initialization.
Both runtime flavors are covered; reversed provider order and failed-second-
snapshot cleanup are checked. These are independent per-input cuts, not a
globally atomic snapshot across upstream systems.

This does not make an unbound or lossy live path safe. This volatile endpoint-array
profile does not retain restart history; use the indexed provider binding above
for coordinated handover and durable recovery. Wider fan-in and other
topology/provider combinations are not yet qualified. The existing native snapshot
frame limits still apply; large native snapshot transfer is not solved here.
The indexed factory exposes result snapshots and uses the separate boolean
`bootstrap` option for an explicitly bound provider; the endpoint-array shorthand
above remains specific to the volatile factory.

## Existing bootstrap-provider integration

`drasi.experimental/provider-query` consumes an existing
`ComputationBootstrapProvider` through the Host SDK's
`NativeBootstrapResource`. This is separate from the upstream-output snapshot
API: provider records are not wrapped in a fake producer snapshot, and snapshot
ordinals are not adopted as live-source watermarks.

The actual native [`drasi.sqlite/file-bootstrap`](../../bootstrappers/sqlite/README.md)
library is covered end to end with the actual query library and a real SQLite
database, inside a `drasi-lib` graph. SQLite schema discovery, row/key mapping,
streaming and worker cleanup remain in that existing provider.
The existing [`drasi.sqlite/changes`](../../sources/sqlite/README.md) library can
then supply live changes to the same query. All three are independently loaded
native libraries, not source/query adapters.

For embedded construction:

- Load the two libraries and select their existing file-bootstrap and
  provider-query factories.
- Create the provider with `NativeBootstrapFactory::create_query_provider`,
  using the target query's component ID and actual instance/graph scope.
- Put `Arc<NativeBootstrapResource>` in a `ResourceRole::Bootstrap` handle and
  bind the query specification's `bootstrap` dependency to it. Graph-owned
  native providers also retain their existing resource-cleanup implementation.
- Use query configuration such as
  `{"query":"MATCH (n:items) RETURN sum(n.amount) AS total","volatile":true}`.
  This factory does not take the upstream `bootstrap` endpoint array.
- For SQLite live capture, also load `drasi.sqlite/changes`. Use the same file,
  table/key mapping, source component ID and stream as the file provider. Connect
  the source's `out` to the query's `in` before starting either component. Start
  the source to establish capture, await query startup, then admit writes.
  Starting capture alone is not permission to write during this file snapshot.

The host calls the provider's `prepare`, streams its `snapshot` envelopes into
the native transformer, checks `complete_snapshot`/`completion_state`, and awaits
provider `stop` before enabling query processing and result reads. Initial
results appear at output sequence zero without live result emissions. Failed
initialization stops the provider and transformer; failed cleanup remains an
error and prevents unsafe restart. An accepted cleanup future is retained if its
waiter is cancelled.

**Keep file writers quiescent until startup finishes.** This mode rejects
progress-bound providers, non-ready preparation, source watermarks and durable
completion state. It does not silently discard those requirements or upgrade
file snapshots to coordinated/recoverable bootstrap. SQLite coordinated/shared
providers and their durable handover remain unconnected. Server configuration
for this new resource/factory is outside this increment.

The [provider integration cases](../../../host-sdk/tests/native_query_plugin/provider.rs)
show the complete graph binding. They verify initial results and rereading a
changed file on restart on both runtimes, missing-file errors, scope validation,
and stopping a real bounded SQLite stream after the consumer rejects a row.

The [live SQLite cases](../../../host-sdk/tests/native_query_plugin/sqlite_live.rs)
connect the actual source, file provider and query in one graph on both Tokio
runtime flavors. One `items` table uses its primary key, source ID `sqlite` and
stream `sqlite/out`. Initial rows come only from the file provider. Subsequent
insert/update/delete requests use the source's existing REST endpoints, which
execute SQL on its capture-owned connection; an independent read-only connection
verifies the database contents. The cases await actual query output, compare
exact live before/after values and snapshots, and verify that updates/deletes
address the original bootstrapped result identities. Arbitrary external
connection writes are not captured by that connection-local SQLite hook.
No Drasi Server API or hand-built change envelopes are involved. Writers remain
quiescent until initialization completes; this is not overlapping snapshot/live
handover or durable recovery.

Interrupted startup is covered by the same real integration. A child process
holds an exclusive SQLite lock, preventing the provider's first snapshot read.
Cancelling the startup wait leaves the accepted initialization owned by the
graph; request query stop to cancel it. Stop remains pending while the read is
blocked, result reads remain unavailable, and the provider cannot be reused or
retired before cleanup finishes. Releasing the lock allows awaited stop, a fresh
bootstrap on the same query, and a captured SQL update without stopping the
independent source. This passes on both runtime flavors without production
lifecycle changes. It does not qualify caller cancellation after partial row
loading, expiry of the provider's cleanup deadline, or other provider types.

## Development

From the Core checkout, run `bash lib/tests/run-query-plugin.sh smoke` to rebuild
the actual query, SQLite source and SQLite bootstrap libraries and run the
loaded-plugin cases.
Use `bash lib/tests/run-query-plugin.sh provider` for only the real file-provider
integration, live capture and unsupported-handover guards. Use
`bash lib/tests/run-query-plugin.sh sqlite-live` for the four real-source cases:
ordinary bootstrap/mutations and interrupted-startup restart on both runtimes.
The ignored `sqlite_read_lock_holder` entry is a subprocess fixture launched by
the interruption cases, not a skipped regression case. Use
`bash lib/tests/run-query-plugin.sh bootstrap` for snapshot initialization,
result-snapshot behavior, query chaining and running-upstream handover. During
focused development, use the `bootstrap::live_handover::` or
`bootstrap::two_inputs::` test filter after rebuilding the actual query library:

```sh
cargo test -p drasi-host-sdk --test native_query_plugin bootstrap::two_inputs:: -- --test-threads=1
```

For only the interrupted-startup scenario after building the three libraries:

```sh
cargo test -p drasi-host-sdk --test native_query_plugin interrupted_sqlite_bootstrap -- --test-threads=1
```

Use `bash lib/tests/run-query-plugin.sh durability` for the indexed restart,
process-exit, persistent snapshot and atomic state-boundary cases. Its process
children are ignored drivers launched by their parent cases, not skipped
regressions. After rebuilding the libraries, select only the snapshot cases with:

```sh
cargo test -p drasi-host-sdk --test native_query_plugin durable::snapshots:: -- --test-threads=1
```

Use `bash lib/tests/run-query-plugin.sh progress` for only the loaded committed
progress and its storage/publication lifecycle cases.

The runner also offers separate boundary, semantics, recovery, timer and API
groups; use `--help` to choose the relevant one. Existing built-in query
regression groups do not prove plugin parity.

The [library/plugin completion plan](../../../../lib/docs/computation-graph-query-transformer-design.md)
records the concrete gaps, implementation order and required preservation gates.
