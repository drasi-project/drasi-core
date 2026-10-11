# Deterministic query timing for integration tests

Enable the non-default `drasi-lib/test-support` feature and attach
`drasi_lib::test_support::QueryTestControl` either to an ordinary builder query
or to a directly constructed native query in a `ComponentBatch`. Both use the
same native query wakeup, transaction and output-frontier implementation.
Neither creates another evaluator, runtime owner or old ComponentGraph manager.

## Ordinary builder queries

```rust,ignore
use drasi_lib::{DrasiLib, Query};
use drasi_lib::test_support::{DrainReport, QueryTestControl};
use std::time::Duration;

let clock = QueryTestControl::new(1_000);
let core = DrasiLib::builder()
    .with_source(source)
    .with_query(
        Query::cypher("expiration")
            .query("MATCH (n:Item) WHERE drasi.trueLater(true, n.deadline) RETURN n.id")
            .from_source("input")
            .enable_bootstrap(false)
            .build(),
    )
    .with_query_test_control("expiration", clock.clone())
    .build()
    .await?;
core.start().await?;

// Await query readiness and establish query-applied input ordering first.
// Merely sending an input or receiving a source WAL position is insufficient.
let report: DrainReport = clock.advance_to(2_000, Duration::from_secs(5)).await?;
assert_eq!(report.physical_time_ms, 2_000);
let unchanged = clock.wake(Duration::from_secs(5)).await?;
assert_eq!(unchanged.output_sequence, report.output_sequence);
core.shutdown().await?;
```

`lib/tests/query_test_control.rs` is an executable public-consumer example,
including a replayable test source and a real RocksDB provider.

## Directly assembled native queries

For a host using one `.with_components(batch)` library, attach to the actual
`ContinuousQueryTransformer` or its existing `TransactionTransformer` query
body before adding it to the batch. No `.with_query()` or private API is needed.
For example, this assembly fragment uses the query-body wrapper for persistent
unconfirmed-output replay:

```rust,ignore
use drasi_lib::computation::v1::*;
use drasi_lib::test_support::QueryTestControl;
use std::sync::Arc;

let clock = QueryTestControl::new(1_000);
let catalog = QueryResultsCatalog::new(instance_id)?;
let mut outlet_events = catalog.subscribe();
let applied = Arc::new(QuerySourceProgress::new(instance_id, query_id.clone())?);
let native_query = ContinuousQueryTransformer::new(definition, indexes)
    .await?
    .with_result_catalog(&catalog)?
    .with_source_progress(applied.clone())?;
let results = native_query.results();
let query = TransactionTransformer::from_query(native_query)
    .with_test_control(clock.clone())?;
let batch = ComponentBatch::builder()
    .source(Box::new(input_source))
    .query(Box::new(query))
    .sink(Box::new(QueryResultsOutlet::new(outlet_id, catalog)))
    .bind_stream(source_output, input_stream)
    .bind_stream(query_output, output_stream)
    .connect(input_edge, Box::new(BoundedPipeConfig { capacity: 8 }))
    .connect(output_edge, Box::new(BoundedPipeConfig { capacity: 8 }))
    .build()?;
let core = drasi_lib::DrasiLib::builder()
    .with_id(instance_id)
    .with_components(batch)
    .build()
    .await?;
core.start().await?;
let handle = core.computation_control()?.component_handle(&query_id)?;
handle.wait_started().await?;
results.wait_ready().await?;
// Await source/outlet readiness and the query-applied input fence as well.
// Drain with clock.advance_to(...), then separately await outlet observation.
```

For the direct transformer without handoff replay, replace the wrapper line
with `let query = native_query.with_test_control(clock.clone())?;`.
Wrapping an already controlled native query with
`TransactionTransformer::from_query(query)` also retains its control.
The wrapper contains the **same query/evaluator**, not another query graph.
Only query bodies accept the hook; linear transaction/domain transformers do not.
Neither attachment is compatible with an external `QuerySchedulingResource`.

`lib/tests/query_test_control/native.rs` is the executable public-consumer
fixture for this assembly. It uses the native `RocksDbComputationProvider`,
an `EnvelopeSource`, bounded pipes, an actual `QueryResultsOutlet` (with a
test-only handling gate), and retained `QueryResults` read handles. Its two
independent fences are:

- `Host::insert`: after enqueueing, await the actual query's
  `QuerySourceProgress::subscribe()` checkpoint for the input stream/sequence.
  This checkpoint is query-applied progress, not source acceptance.
- `Host::observe`: await the previously subscribed catalog event for the
  reported logical sequence, validating its query generation. This proves the
  outlet published that event. It still does not prove any other subscriber or
  external effect completed. Subscription lag/closure and deadlines are errors.

The fixture demonstrates a successful drain while the outlet is paused, then
requires the separate observation fence. Filling the immediate bounded pipe
instead blocks the drain. Do not replace either fence with a sleep.

## API and exact frontier

| API | Meaning |
|---|---|
| `QueryTestControl::new(epoch_ms: u64)` | Create an unbound local physical eligibility clock |
| `now() -> u64` | Read that clock |
| `advance_to(epoch_ms, timeout) -> Result<DrainReport>` | Monotonically advance, wake the query without a source event, and await its frontier |
| `wake(timeout) -> Result<DrainReport>` | Request the same frontier without changing time, including for an empty/not-due queue |
| `DrasiLibBuilder::with_query_test_control(id, control)` | Attach to exactly one builder query; unknown IDs, duplicate attachments and shared controls are rejected |
| `ContinuousQueryTransformer::with_test_control(control) -> anyhow::Result<Self>` | Attach before activation to the actual native query owned by the graph |
| `TransactionTransformer::with_test_control(control) -> anyhow::Result<Self>` | Delegate to the same query body; reject linear transaction bodies |

Each successful drain means:

1. The graph-owned query has finished all scheduled work eligible at the
   requested time and rechecked its actual future queue: nothing due remains.
2. Each selected future, its evaluator state, and its query output have passed
   the configured query transaction/publication path. The control does not
   bypass the native transaction owner, output persistence, replay or failure
   fencing.
3. Every output produced or replayed during the drain has been accepted by the
   query's **immediate outgoing pipes**, and its delivery-confirmation hook has
   completed. The native serialized query also finishes forwarding preceding
   work in the same uninterrupted run before handling the request. A stalled
   immediate pipe or commit prevents success.

The returned fields describe that single query lifetime:

| Field | Meaning |
|---|---|
| `physical_time_ms: u64` | Eligibility time used for the drain |
| `output_sequence: u64` | Committed logical query result sequence, not replay transport sequence |
| `output_generation: u64` | Reset generation for that sequence |
| `output_persistent: bool` | Whether this query actually persists its output |
| `publication: QueryPublicationMode` | Actual `Atomic` or `NonAtomic` publication mode |

A quiet drain can return the same sequence. Query reset and recovery retain
their existing sequence/generation rules; compare the pair within the same
query/storage identity.

The sequence is a committed-output frontier, **not a historical delivery
ledger across interrupted runs**. In particular, restarting a volatile query
cannot certify handoff of output lost by its preceding cancelled run. Persistent
unconfirmed query output follows the existing replay/confirmation protocol
**when delivery tracking is enabled**. Ordinary builder queries already use
that path. A direct `ContinuousQueryTransformer` alone persists its query
state/output but does not enable handoff replay; use
`TransactionTransformer::from_query` when replay of interrupted publication is
required. The test hook deliberately does not change this production choice.

**This is not a downstream handling barrier.** Ordinary queries feed a bounded
pipe to the query-results outlet. Pipe acceptance may precede that outlet's
publication to subscriptions. Neither subscriber receipt, reaction callbacks,
destination persistence, external effects, browser updates nor whole-graph
completion is covered. Even an outlet declared `Handled` is not awaited by
the bounded pipe's acceptance boundary. Await the consumer's own
receipt/handling contract separately. Errors that occur downstream *after* the
frontier cannot retroactively fail an already returned report.

## Ordering and clock semantics

The hook does not flush ingress, source WALs, adapters, other queries, or
middleware input queues. Before advancing, establish that relevant source
changes were **applied by the query**, for example by observing their expected
query output or the actual query's committed source-progress boundary. Do not
substitute source acceptance or a source WAL position for this.

The clock changes physical timer eligibility only. Source timestamps and
`datetime.realtime()`'s existing query evaluation clocks are unchanged.
Scheduled evaluation still uses native scheduled logical times, last-applied
clocks and stale-hint suppression. Repeated unchanged checks do not acquire
a new deadline, and stale/duplicate wakes cannot pop a not-yet-due future.
No process-global clock or Tokio clock is changed.

Uncontrolled queries retain the system clock and normal scheduling cadence,
even when the feature is enabled. Controlled queries park the ordinary
scheduled source and use explicit native wakeups/continuations; they do not
poll or sleep to emulate completion. Time advancement alone is not a domain
timer implementation.

## Cancellation, failure and lifecycle

Clones serialize commands. The deadline bounds the entire call, including
waiting for a prior command and output backpressure. A caller timeout or a
dropped future **does not undo clock advancement or cancel submitted work**.
The graph retains ownership and serialization until that work finishes or
the query stops/fails; later commands cannot advance the clock in the meantime.

Graph-observed query, scheduling, source-adapter and output failures fail
pending requests and retain the original graph cause. Later calls fail rather
than returning a success-shaped report. Backwards/out-of-range time is rejected
without changing the clock. A stopped, unstarted or shut-down query rejects
requests. Successful persistent recovery/restart opens a fresh endpoint;
stale failure publications cannot fail that new endpoint.

Ordinary controls observe their private QueryGraph. A direct native attachment
observes the owning query generation and its immediate connected peers, not
unrelated siblings in the shared instance. Errors farther downstream are only
covered if propagated to this boundary. Attach before activation; controls are
single-query bindings and are not configuration/management persistence.
Standalone manual `query.start()` without graph ownership is rejected for a
direct test-controlled query.

For controlled **ordinary builder queries**, `stop_query`/shutdown uses the
graph's existing **abort-and-await stop**, rather than graceful output-edge draining. This lets
tests stop a deliberately blocked output consumer. It is an intentional
test-only lifecycle difference: buffered volatile delivery can be lost;
committed persistent pending output uses the existing replay path. Await
stop/shutdown before dropping the library or reusing storage. Cleanup failures
remain errors, and uncertain storage operations retain their normal cleanup
ownership; this hook cannot force an arbitrary external provider to finish.
Graceful quiescence/reconfiguration is not a cancellation command.

For **native batch queries**, use the public `ComponentHandle::stop()` obtained
through `core.computation_control()?.component_handle(&query_id)?`, or
`core.shutdown()`. These already provide graph-owned abort-and-await and need no
new stop implementation. `handle.start().await?` reopens the same control after
successful query recovery. Do not use the facade's graceful native
`stop_query`/reconfiguration path as a blocked-output cancellation fence.
Always bound test readiness, progress, outlet and lifecycle waits too; the
control's timeout bounds only its own request.
Native cleanup retries retain the existing error contract: a successful
disposal can still return the failed driver's original cause before subsequent
shutdown calls become idempotent. The fixture exercises both component-stop and
library-shutdown failures without erasing them.

## Persistence scope and migration

Supported scope is ordinary builder queries hosted by their native QueryGraph
and explicitly constructed `ContinuousQueryTransformer`/query-body
`TransactionTransformer` components in the instance's native graph. The
existing inline-memory and injected index-provider paths remain intact.
Persistent atomic output is exercised with the RocksDB plugin for ordinary
queries and `RocksDbComputationProvider` for native batches. The native fixture
also interrupts a real output commit before publication, reopens storage and
fences replay through the existing query-body wrapper. Other providers retain
their actual declared publication and durability contracts. `NonAtomic` does
not become atomic and volatile output does not become durable.

Neither the test clock nor requests/reports are persisted. After reconstructing
a library, create a new control at the intended physical time and preserve the
existing instance ID, query ID/configuration, storage identity and recovery
policy. Call `wake` after readiness to fence recovered output and currently due
timers. Pending output replays without recomputing the timer. No new storage
format is introduced. Persistent desired-definition/management restoration,
arbitrary domain transformers and distributed/whole-graph clocks are not
attachment surfaces for this hook.

Migration from the removed manager hook:

- Keep the `test-support` feature, `QueryTestControl`/`DrainReport` imports,
  `new`/`now`/`advance_to`/`wake` calls and `with_query_test_control` builder calls.
- A native `with_components` host attaches with `with_test_control` on its
  actual native query instead of adding a builder query. Preserve or select
  the existing query-body wrapper for durable handoff replay independently of
  enabling the test feature.
- Existing reads of `physical_time_ms` and `output_sequence` still work.
  Struct literals or exhaustive destructuring need the new generation and
  publication fields (or `..` when destructuring).
- Replace any assumption that a drain also dispatched to every subscriber.
  The native guarantee ends at immediate pipe acceptance. Retain a separate
  consumer receipt/handling wait where required.
- Use separate controls for separate query IDs or library instances; create a
  fresh control after rebuilding a library, rather than rebinding a used one.
- Timer outputs retain native metadata/timestamp behavior. The hook does not
  reinstate removed manager-specific output formatting or clocks.

Focused commands (no Docker or full runtime matrix required):

```bash
RUST_LOG=error cargo test -p drasi-lib --features test-support --test query_test_control
RUST_LOG=error cargo test -p drasi-lib --features test-support,computation-rocksdb-tests \
  --test query_test_control
cargo clippy -p drasi-lib --lib --test query_test_control \
  --features test-support,computation-rocksdb-tests -- -D warnings
cargo check -p drasi-lib --no-default-features --lib
RUST_LOG=error cargo test -p drasi-lib --no-default-features \
  --test computation_queries --test computation_temporal_retractions --test ranked_query_order
cargo fmt -- --check
```
