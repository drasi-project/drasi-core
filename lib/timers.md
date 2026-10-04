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
