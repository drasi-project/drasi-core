# Timed queries: deadlines, rescheduling, and recovery

DrasiLib polls each query's future queue and sends `FuturesDue` control signals
through the normal priority queue. A signal is only a wakeup hint: it may be
duplicated, delayed, or refer to a deadline that has since moved or been removed.
The processor selects only futures due at its current physical time. It does not
drain later deadlines just because one earlier deadline was due.

## Core API and backend contract

`ContinuousQuery::process_due_futures()` processes one item due at the current
system time. `process_due_futures_at(now)` does the same with an explicit physical
clock, in milliseconds since the Unix epoch. Custom consumers using a test or
virtual clock must pass that clock to the latter method; `AutoFutureQueueConsumer`
does this for `with_now_override`.

Both operations hold the query's change lock while selecting, evaluating, and
committing the future. `FutureQueue::pop_due(now)` checks eligibility and removes
the selected item in the same backend lock/session view, including uncommitted
overwrites. Cached or external peeks cannot authorize removal. The shadow queue
delegates selection to the underlying backend rather than trusting its peek cache.
Existing single-writer, per-query session ownership still applies; this is not a
multi-worker claim or distributed lease protocol.

**Compatibility:** `process_due_futures()` and unconditional `FutureQueue::pop()`
keep their signatures. Code that intentionally advanced a simulated clock by
calling `process_due_futures()` must now use `process_due_futures_at(now)`.
Backend implementors must implement the new required `pop_due` trait method;
there is deliberately no racy peek-then-pop default. In-memory, RocksDB, Garnet,
and the shadow wrapper implement it. Rebuild custom index plugins with Core.
There is no on-disk schema or timer-key change.

Physical time determines eligibility only. During evaluation,
`datetime.realtime()` remains the selected timer's **scheduled due time**, and
`datetime.transaction()` remains its original source-event time. A late wakeup
does not rewrite either timestamp.

## Deterministic external integration tests

Enable the opt-in `test-support` Cargo feature in the **test dependency**, not in
production configuration:

```toml
[dev-dependencies]
drasi-lib = { path = "../drasi-core/lib", features = ["test-support"] }
```

`drasi_lib::test_support::QueryTestControl` works when DrasiLib is compiled as a
normal dependency (without `cfg(test)`). Attach it before starting a library:

```rust
use drasi_lib::{DrasiLib, Query, test_support::QueryTestControl};
use std::time::Duration;

let clock = QueryTestControl::new(0); // absolute epoch milliseconds
let core = DrasiLib::builder()
    .with_source(test_source)       // implements the public Source trait
    .with_reaction(test_reaction)   // implements the public Reaction trait
    .with_query(Query::cypher("checks")
        .query("MATCH (r:Request) WHERE drasi.trueLater(r.pending, r.nextCheckAt)
                RETURN r.id AS id, r.generation AS generation")
        .from_source("requests")
        .enable_bootstrap(false)
        .auto_start(true)
        .build())
    .with_query_test_control("checks", clock.clone())
    .build().await?;
core.start().await?;

// Publish inputs and await the Source's confirmed-position handle first.
let drained = clock.advance_to(100, Duration::from_secs(10)).await?;
assert_eq!(drained.physical_time_ms, 100);
// Await the test Reaction's own receipt/effect hook separately.
let duplicate = clock.wake(Duration::from_secs(10)).await?;
assert_eq!(duplicate.output_sequence, drained.output_sequence);
core.shutdown().await?;
```

The public methods are `new(u64)`, `now() -> u64`,
`advance_to(u64, Duration) -> Result<DrainReport>` and
`wake(Duration) -> Result<DrainReport>` (the latter two are async and use
`drasi_lib::error::Result`). `DrainReport` contains `physical_time_ms: u64` and
`output_sequence: u64`.

**Scope and ordering.** Each control binds to exactly one query in one library;
clones share that query's control, while independent controls never share clocks.
Duplicate bindings and unknown query IDs are builder errors. Controlled queries
disable autonomous timer polling: only explicit commands wake their actual
`FutureQueueSource`, dispatcher, forwarder, priority queue, and production
manager drain. Uncontrolled queries, including those in a feature-enabled build,
keep ordinary system time and polling. The feature does not alter serialized
query configuration, its hash, timer identities, or storage formats.

Commands serialize applied clock changes **through manager completion**, not
merely through enqueue. Time advances monotonically; equal time is allowed,
backward time is rejected. The report identifies the command's applied time.
`wake` leaves time unchanged and deliberately sends a signal even with no due
work, so empty queues and stale wakes finish deterministically. To exercise a
stale signal, commit a source reschedule to a later deadline, then wake at the
old physical time. Neither the signal nor its delivery timestamp authorizes a
pop; the current queue entry and controlled physical clock decide eligibility.
Timer evaluation still uses the scheduled **logical** due time.

**Exact fence.** Success means the manager reached a successful no-more-due
queue operation, all selected timer evaluations committed, and their output
outbox/live-snapshot/sequence-marker writes and result-dispatch calls finished
without reported errors. The returned output sequence is sampled afterward.
Without configured persistent writers, this only promises in-memory output.
It is not a source-ingress flush, a reaction callback/effect acknowledgement,
an atomic timer/publication transaction, or an exactly-once guarantee. Failed
publication is an error, not a successful empty report; a latched unhealthy
persistence state also fails later fences. The recovery gap below remains.

Before waking, use the Source's confirmed-position handle (advanced after
successful Core commit), not checkpoint reads, which may expose staged writes.
The manager finishes that source event's output work before processing the
subsequent wake. After draining, await the test Reaction's own callback/effect
receipt if needed. Concurrent unrelated ingress is not included in the fence.

**Timeout and cleanup.** Each command's timeout includes serialization,
backpressure, drain and publication. A timeout or dropped waiter does not
rewind the clock or cancel work already queued. Its serialization guard remains
owned by the manager request until completion or source shutdown, preventing
later commands from changing an unfinished drain's clock. A subsequent bounded
command may also time out, but cannot report unfinished work as successful.
Stopping the query fails outstanding requests; commands before startup or after
stop fail explicitly. Restarting that same query reconnects the same control.
Always use bounded waits and shut down the library, especially after a failed
test or blocked provider operation.

The complete public Source/Reaction fixture and test-owned RocksDB outbox gate
are in `tests/controlled_timing.rs`. The gate proves that wake delivery and timer
commit cannot complete the fence before output publication, without introducing
a library fault-injection API:

```sh
cargo test -p drasi-lib --features test-support --test controlled_timing
cargo test -p drasi-lib --no-default-features --lib future_queue_tests
cargo test -p drasi-lib --features test-support --lib future_queue_tests
cargo check -p drasi-lib --no-default-features --all-targets
```

## Use a fact to rearm a check

For a pending request with a movable next-check deadline:

```cypher
MATCH (r:Request)
WHERE drasi.trueLater(r.pending, r.nextCheckAt)
RETURN r.id AS id, r.generation AS generation,
       r.completedCheck AS completedCheck, r.remoteState AS remoteState,
       r.responseDeadline AS responseDeadline
```

Store deadlines as absolute epoch milliseconds (or supported temporal values).
After completing a check, publish a new source observation with an advanced
`generation`/`completedCheck` and `nextCheckAt`, even if `remoteState` is unchanged.
That update retracts the due row and rearms the next check. A stable true result
is a live row, **not a recurring event**. Include durable request/generation
identity in results and deduplicate effects using that identity, not wakeup count.

Keep the response-expiry deadline separate and frozen for that request:

```cypher
MATCH (r:Request)
WHERE drasi.trueLater(r.pending, r.responseDeadline)
RETURN r.id AS id, r.responseDeadline AS responseDeadline
```

Completing a status check must not extend that deadline. Correlate any response,
cancellation, or new attempt with the appropriate durable request identity.

### Function semantics

| Function | Before the deadline | At/after the deadline |
| --- | --- | --- |
| `drasi.trueLater(condition, absoluteDeadline)` | Overwrites the row/group's schedule and returns `Awaiting`, even when the condition is false | Returns the current condition |
| `drasi.trueUntil(condition, absoluteDeadline)` | False removes the schedule and returns false; true schedules only if absent and returns `Awaiting` | Returns the current condition and removes the schedule |
| `drasi.trueFor(condition, duration)` | Tracks the first true time; false clears it and cancels | True becomes eligible after the duration |
| `drasi.trueNowOrLater(condition, absoluteDeadline)` | True is immediate; false schedules/overwrites and returns `Awaiting` | Returns the current condition |

`trueUntil` does **not** mean "true until this deadline." Null arguments generally
return null before cancellation logic; do not use null to cancel. `Awaiting` and
null do not pass `WHERE`. With `trueLater`, setting the condition false prevents
activation but is not an eager queue deletion. A deleted source element cannot
activate when its queued timer is later consumed.

Timer identity depends on expression position and row/group signature. Do not
change a query's shape and assume old timers retain the same meaning.

## Recovery: persistent timers are not durable notifications

In-memory indexes disappear on restart and do not roll back failed evaluations.
RocksDB and Garnet persist queue/index state using their session mechanisms.
DrasiLib resumes compatible persistent queries; a changed or missing stored
configuration hash can clear state and require bootstrap.

**There is currently a timer-commit/publication gap.** Core commits the timer pop
and evaluation indexes before returning results. DrasiLib subsequently writes
the outbox, live snapshot, and output sequence marker. These operations are not
one atomic transaction.

| Interruption point | Deterministically demonstrated with RocksDB close/reopen |
| --- | --- |
| Timer popped/evaluated, before session commit | Cancelling the processor rolls back the pop and index updates. Reopening retains the timer, which fires at its deadline without a source event. |
| Session committed, before outbox append | The timer is absent, the outbox and snapshot are empty, and no timer notification reappears on restart. This may leave no output-sequence gap for strict recovery to detect. |
| Outbox append completed, before live snapshot/marker update | Restart replays the durable outbox to repair the snapshot. The timer does not refire; replay consumers must still deduplicate effects. |

These tests cancel the task at explicit fault gates, release all database owners,
and reopen the actual database with fresh manager/index instances. They do not
simulate machine power loss or guarantee fsync durability. Garnet has the same
due-pop contract tests, but its live-service execution/restart guarantees must
be validated separately in a dedicated test environment.

Until publication is atomic with timer consumption, downstream startup recovery
must reconcile **durable pending requests against durable completed-effect
receipts**, including requests for which no notification was recorded. Recreate
a fresh check observation/generation and next deadline when work remains.
Replaying an identical source sequence may be deduplicated, and an unchanged
true row need not emit again; neither is a recovery contract. The tests prove
that a fresh generation can retract/rearm the committed-but-unpublished row.
No generic retry/sleep loop can resolve an unknown publication outcome safely.

The minimum stronger Core/library contract would atomically commit the timer
pop, evaluation state, and a durable publication intent (or retained replayable
timer claim). Recovery must replay that intent until acknowledged, with stable
effect identities for idempotent delivery. Merely moving the existing outbox
call earlier, or persisting only the queue, is insufficient. This change does
not claim exactly-once publication or exactly-once external effects.

## Regression coverage

The manager tests use the production source dispatcher, source forwarder,
priority queue, timer signaler, evaluator, result dispatcher, and recovery code.
Only the physical clock and persistence fault gates are controlled. Assertions
use processed-event fences and explicit gates rather than sleep-based absence
checks; timeouts only bound hangs.

```sh
cargo test -p drasi-core --lib due_future_tests
cargo test -p drasi-lib --lib future_queue_tests
cargo test -p shared-tests --lib future_queue_pop_due
cargo test -p drasi-index-rocksdb --test scenario_tests future_queue_pop_due
# Requires a dedicated Redis/Garnet test service:
cargo test -p drasi-index-garnet --test scenario_tests future_queue_pop_due
```

Coverage includes two deadlines, stale/duplicate signals, a source reschedule
held at its commit boundary, the Core change-lock boundary, past/equal/future
times, no-event expiry, logical clock preservation, repeated unchanged checks,
frozen response expiry, false/deleted cancellation, retractions, repeated
RocksDB reopen, and all three publication fault boundaries above.
