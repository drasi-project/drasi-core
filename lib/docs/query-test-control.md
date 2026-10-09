# Deterministic query timing for integration tests

Enable the non-default `drasi-lib/test-support` feature and attach
`drasi_lib::test_support::QueryTestControl` to a query supplied to
`DrasiLib::builder()`. This controls the **native QueryGraph**, not a second
evaluator or the removed ComponentGraph query manager.

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

## API and exact frontier

| API | Meaning |
|---|---|
| `QueryTestControl::new(epoch_ms: u64)` | Create an unbound local physical eligibility clock |
| `now() -> u64` | Read that clock |
| `advance_to(epoch_ms, timeout) -> Result<DrainReport>` | Monotonically advance, wake the query without a source event, and await its frontier |
| `wake(timeout) -> Result<DrainReport>` | Request the same frontier without changing time, including for an empty/not-due queue |
| `DrasiLibBuilder::with_query_test_control(id, control)` | Attach to exactly one builder query; unknown IDs, duplicate attachments and shared controls are rejected |

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
unconfirmed query output follows the existing replay/confirmation protocol.

**This is not a downstream handling barrier.** Ordinary queries feed a bounded
pipe to the query-results outlet. Pipe acceptance may precede that outlet's
publication to subscriptions. Neither subscriber receipt, reaction callbacks,
destination persistence, external effects, browser updates nor whole-graph
completion is covered. Even an outlet declared `Handled` is not awaited by
the ordinary bounded pipe's acceptance boundary. Await the consumer's own
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

For controlled queries only, `stop_query`/shutdown uses the graph's existing
**abort-and-await stop**, rather than graceful output-edge draining. This lets
tests stop a deliberately blocked output consumer. It is an intentional
test-only lifecycle difference: buffered volatile delivery can be lost;
committed persistent pending output uses the existing replay path. Await
stop/shutdown before dropping the library or reusing storage. Cleanup failures
remain errors, and uncertain storage operations retain their normal cleanup
ownership; this hook cannot force an arbitrary external provider to finish.
Graceful quiescence/reconfiguration is not a cancellation command.

## Persistence scope and migration

Supported scope is ordinary builder queries hosted by the native DrasiLib
QueryGraph, using the existing inline-memory or injected index-provider path.
Persistent atomic output is exercised with the RocksDB plugin; other providers
retain their actual declared publication and durability contracts. `NonAtomic`
does not become atomic and volatile output does not become durable.

Neither the test clock nor requests/reports are persisted. After reconstructing
a library, create a new control at the intended physical time and preserve the
existing instance ID, query ID/configuration, storage identity and recovery
policy. Call `wake` after readiness to fence recovered output and currently due
timers. Pending output replays without recomputing the timer. No new storage
format is introduced. Persistent desired-definition/management restoration
and arbitrary user-assembled native components are not attachment surfaces
for this builder hook.

Migration from the removed manager hook:

- Keep the `test-support` feature, `QueryTestControl`/`DrainReport` imports,
  `new`/`now`/`advance_to`/`wake` calls and `with_query_test_control` builder calls.
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
cargo clippy -p drasi-lib --lib --test query_test_control --features test-support -- -D warnings
RUST_LOG=error cargo test -p drasi-lib --no-default-features \
  --test computation_queries --test computation_temporal_retractions --test ranked_query_order
cargo fmt -- --check
```
