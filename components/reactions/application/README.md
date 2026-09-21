# Application reaction

The application reaction delivers query changes to Rust code through an
in-process channel. It works with both ComponentGraph and ComputationGraph.

For a complete source/query/reaction program, run this from the drasi-core
repository root:

```bash
cargo run --locked -p drasi-lib --features computation --example computation_runtime
```

See the [example source](../../../lib/examples/computation_runtime.rs) and
[ComputationGraph usage guide](../../../lib/docs/computation-graph-usage.md).

## Create the reaction

This excerpt assumes `drasi` is running and the `users` query exists:

```rust,ignore
use drasi_reaction_application::ApplicationReaction;

let (reaction, handle) = ApplicationReaction::builder("user-results")
    .with_query("users")
    .with_priority_queue_capacity(5_000)
    .with_auto_start(true)
    .build();

let mut subscription = handle.subscribe_with_options(Default::default()).await?;
drasi.add_reaction(reaction).await?;
```

`ApplicationReaction::new("user-results", vec!["users".into()])` is the shorter
constructor with default settings. The builder returns the reaction and its
handle directly, not a `Result`.

DrasiLib owns the reaction after addition and supplies its query subscriptions.
The handle is how your application receives the results.

## Wait for readiness in ComputationGraph mode

Adding a reaction in ComputationGraph mode accepts its node before startup is
finished. If you are about to send events that the reaction must receive,
replace the `add_reaction` call above with:

```rust,ignore
let component = drasi.add_reaction_with_handle(reaction).await?;
tokio::time::timeout(
    std::time::Duration::from_secs(30),
    component.wait_started(),
).await??;
```

This uses the example's `auto_start = true` on a running instance.
For a reaction with automatic startup disabled, call `component.start()` instead.
The constructor's handle receives data; the addition's component handle tracks
creation/startup. A fresh trigger reaction does not replay changes from before
its query subscription was attached.

## What a result contains

The actual types are defined in
[`drasi_lib::channels`](../../../lib/src/channels/events.rs).

| `QueryResult` field | Meaning |
|---|---|
| `query_id: String` | Query that produced the message |
| `sequence: u64` | Increasing result-emission number for that query |
| `timestamp: DateTime<Utc>` | Result timestamp |
| `results: Vec<ResultDiff>` | Ordered list of changes, not a list of plain JSON rows |
| `metadata: HashMap<String, Value>` | Additional query/source information |
| `profiling: Option<ProfilingMetadata>` | Timing information when present |

One message can contain several changes:

| `ResultDiff` variant | Fields |
|---|---|
| `Add` | `data`, `row_signature` |
| `Delete` | `data`, `row_signature` |
| `Update` | `data`, `before`, `after`, optional `grouping_keys`, `row_signature` |
| `Aggregation` | optional `before`, `after`, `row_signature` |
| `Noop` | No row change |

`row_signature` identifies a row within its query. Use the query ID as well when
combining results from multiple queries. An Update's `data` is the current row;
`before` and `after` make the change explicit.

For example, serializing an unprofiled added result produces this shape.
The timestamp, sequence and row signature here are illustrative:

```json
{
  "query_id": "users",
  "sequence": 42,
  "timestamp": "2026-09-21T12:00:00Z",
  "results": [
    {
      "type": "ADD",
      "data": { "id": 123, "name": "Alice" },
      "row_signature": 7
    }
  ],
  "metadata": {}
}
```

The serialized type tags are exactly `ADD`, `DELETE`, `UPDATE`, `aggregation`
and `noop`. `profiling` is omitted when absent. This is raw `QueryResult`
serialization, not the formatted output envelope used by some network reactions.

## Receive and handle changes

This function handles every change variant:

```rust
use drasi_lib::channels::ResultDiff;
use drasi_reaction_application::ApplicationReactionHandle;

async fn print_results(handle: ApplicationReactionHandle) -> anyhow::Result<()> {
    let mut subscription = handle.subscribe_with_options(Default::default()).await?;
    while let Some(message) = subscription.recv().await {
        for change in message.results {
            match change {
                ResultDiff::Add { data, .. } => println!("Added: {data}"),
                ResultDiff::Delete { data, .. } => println!("Deleted: {data}"),
                ResultDiff::Update { before, after, .. } => {
                    println!("Changed: {before} -> {after}");
                }
                ResultDiff::Aggregation { before, after, .. } => {
                    println!("Aggregate changed: {before:?} -> {after}");
                }
                ResultDiff::Noop => {}
            }
        }
    }
    Ok(())
}
```

Choose **one** way to consume a handle:

| Method | Result |
|---|---|
| `subscribe_with_options(options)` | `Result<Subscription>` for async, non-blocking or batch receives |
| `take_receiver()` | `Option<mpsc::Receiver<QueryResult>>` for direct channel access |
| `as_stream()` | `Option<ResultStream>` with `next()` / `try_next()` |
| `subscribe(callback)` | Starts a callback task; callback must be `Send + 'static` |
| `subscribe_filtered(query_ids, callback)` | Callback task that filters by query ID |

The first consumption method takes the receiver. A second attempt fails or
returns `None`, according to the method. Cloning the handle does not create a
second receiver. Create separate reactions for independent consumers.

`Subscription` and the stream wrappers can be moved into an owning task; calls
on one receiver require mutable access. Keep callback/consumer tasks alive while
you need results and manage their shutdown in your application.

## Buffering, timeouts and batches

| Setting | Default / actual behaviour |
|---|---|
| Builder `with_queries` / `with_query` | Empty until supplied; chooses the query subscriptions |
| Builder `with_priority_queue_capacity` | 10,000 messages; reaction input queue |
| Builder `with_auto_start` | `true` |
| Channel from reaction to application | Fixed capacity of 1,000 messages |
| `SubscriptionOptions::timeout` | None; `recv()` waits until a result arrives or the channel closes |
| `SubscriptionOptions::batch_size` | None; `recv_batch()` uses 10 |
| `SubscriptionOptions::buffer_size` | Stored option, default 1,000; currently does not resize the application channel |
| `SubscriptionOptions::query_filter` | Stored option; currently not applied by `Subscription` receives |

For filtering, use the reaction's query list, `subscribe_filtered`, or explicitly
check `message.query_id` in your receive loop. Do not rely on the stored
`SubscriptionOptions::query_filter` field to filter results.

Results pass through a timestamp-priority input queue. That orders available
messages, not earlier messages that have yet to arrive. The reaction clones an
owned result for application delivery; this is not a zero-copy guarantee.

When the application channel fills, the reaction waits for space. A larger input
queue can absorb a burst but does not make the consumer faster.

For example, with an unused `handle`:

```rust,ignore
use drasi_reaction_application::subscription::SubscriptionOptions;
use std::time::Duration;

let options = SubscriptionOptions::default()
    .with_timeout(Duration::from_secs(30))
    .with_batch_size(100);
let mut subscription = handle.subscribe_with_options(options).await?;

let batch = subscription.recv_batch().await;
for message in batch {
    println!("{}: {} changes", message.query_id, message.results.len());
}
```

`recv_batch()` waits for the first message, then takes immediately available
messages up to the batch size. Use a positive batch size. An empty batch means
the first receive timed out or the channel closed. Similarly, `recv()` returns
`None` for either timeout or closure; `try_recv()` returns `None` for empty or
closed. Use your own cancellation/timeout if you need to stop a receive loop.

## Profiling and recovery limits

When profiling is present, the reaction preserves source/query timestamps and
adds receipt after dequeue and completion after waiting for application-channel
space, immediately before handoff. This includes channel backpressure, **not**
your application's later processing. Unprofiled results stay unprofiled.

The application reaction is not durable, does not request a fresh-start snapshot
and uses Strict recovery by default. Receiving a message is not an atomic
checkpoint of your application's side effects.

Await `drasi.shutdown()` when finished. Do not assume dropping a handle or
stopping a reaction is equivalent to finishing all application-side work.

## Checks and API details

From the drasi-core repository root:

```bash
cargo test --locked -p drasi-reaction-application
```

The complete implementations are in [`application.rs`](src/application.rs) and
[`subscription.rs`](src/subscription.rs). For writing a different reaction, see
the [reaction developer guide](../README.md).

## License

Copyright 2025 The Drasi Authors. Licensed under the Apache License, Version 2.0.
