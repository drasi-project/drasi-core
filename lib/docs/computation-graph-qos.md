# Pipe QoS and durable handoffs

[Design](computation-graph-design.md) |
[Transactions](computation-graph-transactions.md) |
[Managed configuration](managed-configuration.md)

Pipe QoS describes transport acceptance, buffering, replay and completion.
It does not make arbitrary consumer state or external effects transactional.
Stateful processing uses the query or linear `TransactionTransformer` body.

## Profiles

| Profile | Implementation | Guarantee |
|---|---|---|
| Volatile unicast | `BoundedPipeConfig` | FIFO and backpressure; no crash recovery |
| Volatile broadcast | `BroadcastPipeConfig` | Bounded broadcast with explicit lag policy |
| Blocking multicast | Volatile `QosChannel`, `Backpressure` | One shared journal; slow required consumers backpressure the producer |
| Lossy multicast | Volatile `QosChannel`, `PruneOldest` | Producer can evict history; lagging consumers get a gap or explicitly skip |
| Durable multicast/unicast | Persistent `QosChannel` | Atomic acceptance, independent durable consumer cursors, replay and configured retention |

`RetainedPipe` remains a lower-level single-consumer retained transport.
Do not share its one progress cursor across independent consumers.

## One append, multiple subscribers

```rust,ignore
let definition = QosChannelDefinition {
    stream: StreamId::try_new("producer/out")?,
    capacity: NonZeroUsize::new(128).unwrap(),
    durable: false,
    retention: RetentionPolicy::Backpressure,
    subscribers: BTreeMap::from([
        ("first".into(), SubscriptionStart::Earliest),
        ("second".into(), SubscriptionStart::Earliest),
    ]),
};
let channel = QosChannel::volatile(definition.clone())?;
let resource = ResourceId::try_new("output-channel")?;

// Declare/provide channel.resource() with ResourceRole::StateStore.
// Each edge has a distinct subscriber, but shares the producer and resource.
let first = definition.pipe(resource.clone(), "first");
let second = definition.pipe(resource, "second");
```

Connect both configurations from the same producer output. The graph validates
the shared output and distinct subscribers, then calls the channel once per
emission rather than publishing one copy per edge. The event and source-position
metadata are retained together. Each consumer receives its own one-shot delivery
acknowledgement.

The resource definition includes subscribers even while their components are
stopped or not yet constructed. Disconnection, cancellation and failed handling
do not release their retention obligations. Old binding handles cannot complete
deliveries for a replacement subscriber.

The slowest required consumer determines when old entries can be evicted under
`Backpressure`. Bounds count envelopes, not bytes; use an appropriate capacity
and codec message limit. No finite buffer promises unlimited ingestion while a
required consumer remains unavailable.

## Persistent storage

Storage implementations remain external. Obtain an isolated `ComputationIndexes`
bundle from a `ComputationIndexProvider`, then construct the channel:

```rust,ignore
let mut definition = definition;
definition.durable = true;
let indexes = provider.create_indexes("my-graph", "output-channel").await?;
let codec = factories.envelope_codec(NonZeroUsize::new(64 * 1024 * 1024).unwrap())?;
let channel = QosChannel::persistent(definition, indexes, codec, "journal").await?;
```

The bundle must provide persistent checkpoints and a complete atomic transaction.
The channel uses the existing outbox and checkpoint interfaces; it does not
introduce a mandatory database dependency into drasi-lib. Registered factories
can supply executable record validators through `record_schemas()`. Standard
graph and query schemas are included by `FactoryRegistry::envelope_codec`.

`publish(&envelope)` returns only after acceptance. Its `EnqueueReceipt::position`
is a journal position, separate from the event's producer sequence. Source
positions remain opaque bytes. The producer must serialize its stream and keep
an immutable event while retrying. An identical event still in retained history
returns its prior receipt; changed or expired/regressed retry data is rejected.
`progress()` exposes accepted and per-subscriber processed positions, plus the
last accepted producer sequence and source position.

Interrupted or uncertain storage operations fence the channel. Await shutdown,
reopen the same store and resolve/retry against committed state; an uncertain
write is never reported as definite nonacceptance. The configured backend
determines the crash/power-loss boundary.

## Completion and membership

Receiving or dropping a delivery is not completion. The consumer/graph explicitly
completes it with `HandlingOutcome::Handled` after the negotiated boundary.
Failed or abandoned acknowledgements leave the record replayable. A single
subscriber has one outstanding delivery, so acknowledging a later item cannot
skip an unfinished earlier item.

For stateful processing, a transaction must commit its input checkpoint, state
and recoverable output before upstream retirement. A pipe acknowledgement is a
separate short transaction, not a database transaction held open during business
code. Replayed input is deduplicated by the processing transaction.

An acceptance-only reaction queue cannot masquerade as a handled sink.
`CheckpointedSink` wraps actual handling and persists its logical query progress
after the side effect. Arbitrary external effects remain at-least-once unless the
destination supplies idempotency or an appropriate transaction.

On persistent reopen, compatible channel-definition changes preserve existing
consumer progress. New consumers start at the retained beginning, current head,
or an explicitly available position using `Earliest`, `Latest`, or `After`.
Removed members are retired atomically with the new definition. A retired
identity cannot be reintroduced as a different consumer.

`retire(subscriber)` explicitly abandons a disconnected subscriber's outstanding
obligation. Merely removing a graph edge while retaining the subscriber in the
channel definition does not do so. Capacity reductions that would discard
required history fail under `Backpressure`.

`PruneOldest` permits loss. A lagging endpoint defaults to
`ReplayGapPolicy::Strict`; `SkipWithNotification` must be explicitly selected
and logs the skipped interval. Strict gap detection is not a substitute for
lossless retention.

## Declarative hosts

`DesiredPipe::Qos` stores the resource, subscriber, channel definition and gap
policy. The resource and pipe definitions must agree.

`HostManagementResources` accepts:

```json
{
  "kind": "qos",
  "definition": {
    "stream": "producer/out",
    "capacity": 128,
    "durable": true,
    "retention": "Backpressure",
    "subscribers": {"first": "Earliest", "second": "Earliest"}
  },
  "provider": "processing-storage"
}
```

Register `processing-storage` with `HostManagementResources::with_index_provider`.
For volatile channels omit `provider`. Host-created channels require graph
cleanup ownership. Drasi Server supports the same `qos` resource kind with a
`path` for a RocksDB-backed channel instead of a registered provider name;
volatile channels omit `path`.

Configuration-store persistence remains separate from processing storage.
Neither persistent configuration nor a durable pipe upgrades a connector that
acknowledges upstream before reaching the durable pipe. Use the connector's WAL
or an ingestion API that returns the pipe's actual durable acceptance.

## Query and timer handoff

Query factories now construct `TransactionTransformer` query bodies using the
shared core evaluator. Persistent output records are retained until forwarding
is confirmed, then remain available within the query's ordinary replay window.
The query retains its result snapshot and logical result sequence; a replay gets
a fresh transport sequence without applying query state changes again.

`QueryScheduledSource` is independently reusable through
`QuerySchedulingResource`. The transaction-bound query exposes only committed
schedule observations to it. A due notification never removes work; the query
transaction atomically pops the due item, uses the existing reevaluation logic
and records its output. Schedule insertion, replacement and cancellation use
the same transaction as the query state that caused them.

The query processing identity includes the current execution-format version.
Earlier ComputationGraph processing formats are not migrated. Use a fresh
processing namespace or an explicitly supported reset/rebootstrap; do not assume
old prototype data can be resumed under a new evaluation/ordering contract.
