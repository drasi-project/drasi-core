# Pipe QoS and durable handoffs

[Design](computation-graph-design.md) |
[Transactions](computation-graph-transactions.md) |
[Managed configuration](managed-configuration.md)

Pipe QoS describes transport acceptance, buffering, replay and completion.
It does not make arbitrary consumer state or external effects transactional.
Stateful processing uses the query or linear `TransactionTransformer` body.

Optional `SharedStorageGroup` connects one built-in producer and its outgoing
QoS journals to one actual storage transaction. Ordinary persistent channels
keep their separate transactions and bounded replay receipts. Neither option
makes an external effect transactional.

## Profiles

| Profile | Implementation | Guarantee |
|---|---|---|
| Volatile unicast | `BoundedPipeConfig` | FIFO and backpressure; no crash recovery |
| Volatile broadcast | `BroadcastPipeConfig` | Bounded broadcast with explicit lag policy |
| Blocking multicast | Volatile `QosChannel`, `Backpressure` | One shared journal; slow required consumers backpressure the producer |
| Lossy multicast | Volatile `QosChannel`, `PruneOldest` | Producer can evict history; lagging consumers get a gap or explicitly skip |
| Durable multicast/unicast | Persistent `QosChannel` | Atomic acceptance, independent durable consumer cursors, replay and configured retention |
| Shared producer/output commit | `SharedStorageGroup` plus `QosPipeConfig::with_shared_storage` | Producer input progress, state, retained output and participating journal appends commit together; handling remains separate |

`RetainedPipe` remains a lower-level single-consumer retained transport.
Do not share its one progress cursor across independent consumers.
Its persistent `IndexedEnvelopeStore` loads and validates retained bytes and
progress on first use, then keeps that window in memory. Append, next-item lookup
and acknowledgement no longer reread the full journal. Cached state changes only
after a confirmed commit; interrupted writes still require reconstruction.
Reopening with smaller capacity keeps older obligations until the retention
policy permits removing them. Do not mutate the owned journal/checkpoint through
another writer while the store is live.

## Explicit recovery configuration

For separate-store journals, hosts can select an explicit
`QosRecoveryOptions::{Disabled, Admission, Replay}` with
`QosChannel::persistent_with_recovery`. Initial metadata and the selected
service are committed together. Reopen validates the mode, identity and bounds
before changing membership or trimming history. Changing or omitting a
persisted service rejects without rewriting its metadata. An empty plain
journal may establish a new tracked boundary; existing messages cannot
retroactively acquire admission/replay receipts.

The older `persistent` constructor retains its programmatic reopen semantics
and existing explicit `enable_admission`/`enable_replay` APIs. Host SDK and
Server recipes use the stricter constructor so omitted configuration cannot
silently activate persisted services. This distinction does not add work to
volatile pipes or to per-message processing.

## Shared producer/output commits

Supply a dedicated, proven persistent `ComputationIndexes` bundle to
`SharedStorageGroup::new(graph_id, producer_id, indexes)`. Its `resource()` is
the producer's graph-owned `IndexBackend`. The continuous-query, durable
middleware and linear-transaction factories all accept it through their existing
index dependency.

Create each outgoing journal with `group.channel(definition, journal_name,
codec, replay_options).await`. Use stable journal names on reconstruction.
Bind the returned channel as its own `StateStore` resource. Each edge names both:

```rust,ignore
let pipe = channel.definition()
    .pipe(journal_resource_id, "subscriber")
    .with_shared_storage(group_resource_id);
```

The serialized pipe setting is `shared_storage`. Declaring this dependency on
the pipe as well as the producer keeps resource lifetime and reconciliation
honest. Missing dependencies, another group's resources, untracked producers
and lossy settings reject; matching directory names are not proof.
For graph-owned dependent construction, declare the journal's resource dependency
on that same group. The graph creates the group first and releases it only after
journal cleanup completes. Direct builders use `resource_dependency`; Host and
Server definitions use `resource_dependencies`.

Both hosts provide `sharedStorage` and `sharedQos` recipes. `sharedStorage` names
its producer `component` and uses a registered `provider` in Host SDK or a
RocksDB `path` in Server. `sharedQos` supplies its channel `definition` and
`recovery: {kind: replay, failureScope: processRestart, receiptCapacity: 64}`.
Its single `IndexBackend` dependency selects the actual group; there is no
second provider open or hidden host cache. The producer still binds that
group as its indexes, and each pipe retains its `shared_storage` setting.
Both resources must be graph-owned. Shared QoS does not accept client admission.
These recipes do not add a plugin-SDK transaction service or migrate existing
standalone storage.

Existing native transaction participants also work inside the host's linear
container. Their state requests already use its revocable transaction mailbox,
so shared journal appends need no new ABI or plugin-side storage handle. This
does not give standalone native transformers tracked output.

Before processing, the producer reserves space in all participating journals
in a consistent order. Waiting does not hold a storage transaction, so consumers
can acknowledge and free capacity. Multicast reserves/appends once, not once per
subscriber. Processing then stages state, input progress, retained output and
the outgoing appends together. Ordinary graph forwarding finds their existing
receipts instead of appending again.

One producer may also have separate-store output branches. Only participating
shared journals join its transaction; separate branches still use retained
output and replay receipts. Interruption can make branches visible at different
times, but reconstruction does not repeat an accepted shared append.

Reconstruction reads journal metadata and events under the same storage gate.
Metadata lives in each journal's private outbox, separate from producer
checkpoints and clearing. Pending acknowledgement or retirement prevents a new
subscriber binding from observing an unfinished progress change.

Quiesce before stopping a healthy producer: its journals can continue draining.
Aborting an active transaction, losing its commit response, or interrupting
committed cache publication fences the **whole group** until cleanup and
reconstruction. Blocked publishers and receivers wake with an error.
Queued storage operations and inspections also fail promptly, without waiting
for the interrupted owner's cleanup or rolling back that owner's work.
External effects still need their own completion/idempotency contract.
Live producer replacement reuses the group after the old instance is released.
Removing drained components does not remove independently declared resources.
Removing a still-referenced owner rejects; explicitly removing an unused owner
fences its remaining journal handles.

There is one processor per group and at most 256 active journals. Current
built-in operations emit at most one batch per journal; a no-output query may
conservatively wait for capacity before discovering it has nothing to emit.
Replay settings and subscriber definitions cannot silently change on reopen.
There is no automatic standalone-store migration, cross-group transaction,
unbounded receipt history, or general `ExactlyOnce`/`Transactions` capability.
Fast configurations do not allocate these reservations, journals or workers.

## Opt-in operation completion

`DeliveryRunner` supplies one ordered delivery loop for Rust sinks. It uses a
dedicated atomic `ComputationIndexes` bundle for progress, not another input
journal. Keep the incoming envelope in its lossless upstream pipe until the
runner succeeds.

```rust,ignore
let runner = DeliveryRunner::new(
    DeliveryScope::new(
        construction_scope,
        graph_id,
        ComponentId::try_new("consumer")?,
    )?,
    indexes,
    codec,
    DeliveryOptions {
        scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
        max_streams: NonZeroUsize::new(16).unwrap(),
        receipts_per_stream: NonZeroUsize::new(64).unwrap(),
        retry: DeliveryRetryPolicy::default(), // One attempt, no automatic retry.
    },
)?;
```

Implement `DeliveryHandler::handle` so success means **the operation completed**,
not that it entered a queue. Call `runner.deliver(&input, &mut handler)` from the
sink's handling path and propagate failures. Only a sink actually following this
contract may declare `Handled`. An acceptance-only legacy adapter cannot be
upgraded by wrapping its enqueue call.

For a consumer whose business state belongs in the graph's storage, instead use
`DeliveryRunner::new_transactional` and implement
`TransactionalDeliveryHandler::handle(&self, item, &TransactionContext)`.
Call `deliver_transactional(&input, &handler)`. The borrowed context is the existing
transaction-state API, isolated under this consumer's identity. Its state changes
and the operation's completion record commit in **one actual transaction**.
Do not perform external effects, start nested transactions, use independent writers
or workers, or keep commit-sensitive business state in handler fields. Atomicity is
per operation; this does not make an entire source transaction visible at once.

Callback failure rolls back both state and completion. Only a confirmed rollback
allows retry classification; cancellation and uncertain commit/rollback fence the
owner until cleanup and reconstruction. Rollback failure preserves both the
original callback error and the storage error. A real connected QoS case proves
partial state reconstructs before the remaining operations run and the whole
envelope is acknowledged. Required process exits before and after the actual commit
recover state and completion together.

The handling mode is persisted. External v1 records keep their original shape;
transactional records explicitly identify their mode. Wrong call types,
reconstruction with the opposite mode, and damaged mode metadata reject before
handling. There is no implicit migration between the two.

Before effects, the runner saves the producer identity and exact batch digest.
It handles operations in order and commits the completed prefix after each one.
A failed operation leaves the rest pending, and later input on that same
port/stream cannot skip it. Other port/stream cursors are independent. A handled
batch retry performs no effects or progress writes. Empty batches still advance
their logical progress. `progress()` distinguishes the last fully handled
sequence from the latest batch and its completed operation count.

`DeliveryItem::id` is a stable `drasi-delivery-v1-...` key covering construction
scope, graph, consumer, input port, immediate persistent producer, logical sequence
and operation ordinal. Retry transport IDs/sequences and replacement configuration
under the same consumer ID do not change it. The runner reuses output replay's
content comparison; only immediate-query post-commit timings are excluded, not
inherited context. Unidentified/volatile producers, query snapshots/skips, changed
payloads, producer changes and expired receipts reject before effects.

`DeliveryItem::position` adds a stable stream key, logical batch sequence,
zero-based operation **vector index** and batch size. The index is not the
operation ordinal, which may be non-contiguous. A destination can retain the last
batch's digest and completed prefix per stream rather than an ever-growing set of
operation keys. It must reject older batches after rollover, not treat them as new.

The opt-in [`PostgresDeliveryHandler`](../../components/reactions/storedproc-postgres/README.md#opt-in-computationgraph-transactional-delivery)
implements this contract by committing an effect and its cursor in one database
transaction. Seven real database cases cover failures, partial progress,
concurrent duplicates and database crash recovery. This is a separate Rust service;
it does not upgrade the legacy stored-procedure reaction or ordinary HTTP sinks.

The separate [HTTP completion service](../../components/computation-plugins/network/README.md#opt-in-http-completion-service)
sends a whole batch once and confirms its exact `DeliveryBatchIdentity`, which
includes consumer scope, producer identity, sequence, operation count and canonical
content. The endpoint derives that identity independently before effects and
responds only after all operations finish. `runner.batch_identity(&input)` performs
validation/identification without loading or changing progress. HTTP confirmation
does not make an arbitrary handler's effects atomic; the PostgreSQL adapter's
transactional contract remains necessary for once-only database effects.

Automatic retries require the handler to classify the error as retryable and an
explicit retry policy. There are at most 32 attempts per operation per call and
at most 60 seconds between attempts. Cancelling a call also cancels its delay.
Exhaustion returns the original handling cause; there is no implicit skip.
An unknown progress commit fences the owner until cleanup and reconstruction.
Await `shutdown()` before releasing its storage or opening a replacement.

Limits are 256 port/stream pairs, 1,024 receipts per pair, 4,096 receipts total,
256 bytes per identity segment and four MiB of stored metadata. Reopening with
limits smaller than retained state rejects rather than deleting history.
Receipt expiry rejects old replay instead of treating it as a new effect.

This is currently a Rust service, not packaged Server configuration or a native/
legacy plugin completion service. An external destination can see the same operation again
if it completed before local progress committed. It must atomically deduplicate
the supplied ID with its effect for a once-only claim; ordinary destinations
remain at-least-once. Reference HTTP/database adapters are qualified as separate
Rust services; stronger factory/plugin exposure remains W3 work. The
[bundled reaction inventory](../../components/reactions/README.md#bundled-completion-boundaries)
records unsupported stronger boundaries and existing failure/skip policies.
Ordinary sinks do not construct this runner or
acquire its hashes, progress writes, retries or workers.

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

Awaited channel shutdown releases its storage even when a caller retains a
closed channel or endpoint handle. Those old handles remain fenced and cannot
read, publish or acknowledge work in a replacement journal. Managed same-path
replacement preserves the committed journal and subscriber cursors; it does not
copy the data into a new empty channel.

## Opt-in receipts for replayed component output

A component can save its output, deliver it, and crash before saving the fact
that delivery succeeded. Its recovered output has the same logical identity but
can have a new transport ID. Ordinary transport-based retry checks do not cover
that case.

Enable logical output receipts on the existing persistent channel before its
first event **or pipe binding**, and repeat the call on reconstruction to check
the saved settings:

```rust,ignore
channel.enable_replay(ReplayOptions {
    failure_scope: FailureMode::ProcessRestart,
    receipt_capacity: NonZeroUsize::new(128).unwrap(),
}).await?;
```

Normal `publish` and graph-connected sends then recognize retained retries by
the immediate producer's persistent identity and logical sequence. They return
the original journal position without another append or subscriber delivery.
New logical and transport sequences must increase, but need not be consecutive. The original
envelope is preserved; only retry comparison ignores its transport ID/sequence
and the immediate query's post-commit completion timings. Payload, source
position, timestamp, lineage, inherited timings and other context still count.
Query output must carry its own persistent recovery identity and explicit
sequence; inherited markers, snapshots and recovery-skip controls cannot stand
in for live query output.

The receipt and subscriber obligations commit with the event in the same
existing channel transaction. There is no second journal or recovery worker.
The window holds 1-1,024 receipts independently of handled payload retention.
Changed content, expired receipts and a different producer/query generation
reject explicitly. Identifiers are limited to 256 bytes and serialized receipt
metadata to 4 MiB. The actual storage must guarantee the selected failure scope.
Options and channel membership settings cannot silently change on reopen.
Replay journals also retain a UUID identifying the actual stored journal, not
its graph resource name. Reopening old version-1 receipt records upgrades them
transactionally to version 2 with that UUID, preserving receipts and progress.
Pipe binding cannot race an unfinished configuration commit.

This mode and client admission are mutually exclusive: client admission
assigns source identities, while output replay preserves a component's existing
identity. Both are off unless selected; ordinary channels do not calculate
these fingerprints or maintain these receipts.

**Limits:** receipts do not make separate stores commit as one transaction.
A producer must retain unconfirmed output and retry it unchanged. Size the
receipt window for its unconfirmed output batch; an expired retry fails rather
than silently appending again. Automatic recovery after receipt expiry remains
unfinished. These are acceptance receipts, not a promise of once-only external
effects.

### Remembering where pending output must go

The graph now connects those receipts to saved destination lists for its built-in
durable middleware, linear transactions and tracked persistent queries. Before
processing, the producer saves every outgoing replay-enabled QoS destination:
output port, receiving component and input port, journal UUID and subscriber.
It uses its existing state/progress/output transaction, not another owner.

That list cannot change while any output remains unconfirmed. The graph checks
changes before mutation and again after pausing processing, because output could
commit while the pause is being requested. Reopening under a different journal,
removing a destination or disabling tracking cannot silently drop pending work.
After all pending output is accepted, the list may change.

**Replacement policy:** pending output follows the replacement's new configuration
when its component ID, ports, journal and subscriber stay the same. For example,
a changed destination URL takes effect for pending output. The saved list does
not pin the destination's old configuration or generation.

After a pause or restart, retained output is queued again without rerunning the
committed computation. All required branches may be retried: their receipts
prevent duplicate appends within the configured window. The producer confirms
the whole batch only after forwarding finishes; there is no separate per-branch
completion journal.

Membership is bounded to 256 destinations, with 256-byte identifiers and a 1 MiB
record limit. A cached SHA-256 fingerprint is saved alongside existing producer
progress in the same transaction. Missing or mismatched records fail recovery;
the hash is calculated at binding/reconstruction, not per envelope, and adds no
envelope annotations. Ordinary memory paths have no membership journal or hash.
Queries validate saved membership before automatic reset and reject reset while
destinations remain bound. Complete pending output and explicitly unbind first;
damaged storage requires repair, not automatic clearing.

This support is built into the participating Rust producers. The default
transformer hook rejects nonempty bindings, and native/legacy plugin SDKs do not
yet expose this shared service. Ordinary untracked branches are not upgraded,
same-domain publication participation remains unfinished, and the broad
`Transactions`/`ExactlyOnce` flags remain disabled.

## Opt-in producer admission (Rust service)

`QosChannel` can also own durable client acceptance. Rust sources delegate
publication to the graph using `SourceAdmission`. Native HTTP/gRPC sources can
bind the same service through the optional native service-v1 interface and their
[separate durable protocols](../../components/computation-plugins/network/README.md#optional-durable-admission).
Enabling the channel alone does not upgrade an arbitrary connector's responses;
legacy source adoption remains unfinished.

Configure it before the channel's first event:

```rust,ignore
channel.enable_admission(AdmissionOptions {
    construction_scope: "my-instance".into(),
    graph_id: "my-graph".into(),
    component_id: ComponentId::try_new("source")?,
    failure_scope: FailureMode::ProcessRestart,
    max_producers: NonZeroUsize::new(64).unwrap(),
    receipts_per_producer: NonZeroUsize::new(128).unwrap(),
}).await?;
let session = channel.register_producer(ComponentId::try_new("client-a")?).await?;
let receipt = channel.admit(&session, 1, &input).await?;
```

The channel must be persistent, lossless and backed by a provider explicitly
guaranteeing the chosen failure scope. Acceptance uses the existing atomic
transaction and journal, not a second WAL. The receipt confirms durable
acceptance for that scope, **not** downstream processing or an external effect.

Registering an active name returns the same saved session. Each client submits
consecutive sequence numbers starting at one. Retrying a retained number with
the identical encoded input returns the original receipt without writing again.
A changed payload, including changed input identity/metadata/context, is a
conflict; adapters must normalize client input deterministically. The journal
assigns its own stable identity and global position, while preserving the
input's timestamp and upstream metadata in lineage.

`admission_receipt` resolves a lost response. It returns `None` only for the
session's next unaccepted sequence; a future gap, expired receipt or invalid
session is an explicit error. `producer_status` exposes the next sequence and
earliest retained receipt. After an uncertain write or cancellation during
storage work, await shutdown and reconstruct before trusting a lookup. This
also applies to lost registration or retirement responses.

Limits are at most 1,024 active producers, 1,024 receipts per producer and
16,384 receipts in total. Admission names/scopes are at most 256 bytes, and
serialized channel metadata is capped at 4 MiB. Journal capacity and codec
message limits separately bound retained input. Receipts can outlive handled
payload eviction, but numbers outside the configured receipt window remain
expired forever within that session. There is no time-based expiry or background
pruning worker.

### Graph-owned source binding

Construct `SourceAdmission::new(channel.clone(), output_port).await` after
enabling admission, and return the same handle from `EnvelopeSource::admission`.
The graph drives this source's admission requests instead of calling `next`.
The component must not maintain a second queue or emit accepted inputs again.

Every outgoing edge must use that actual QoS channel, with strict replay and
the same output port/stream. Mixed memory/QoS edges, a different channel with
identical settings, untracked subscriptions and missing outputs are rejected.
Component and graph identities must match the persisted admission identity.
Activation also checks the construction scope: standalone graphs use their
graph ID; instance factories receive the owning DrasiLib's scope. Incomplete
graph assembly can defer connecting outputs, but cannot activate without them.
Live reconciliation cannot replace these edges with weaker transports.

The graph derives acceptance/replay evidence from the bound service, rather than
requiring a plugin to make its own durability claim. It validates the final
output schema, stream and sequence **before** the journal commit. Validation,
capacity and busy failures do not consume a client or graph sequence. Accepted
entries already belong to every QoS subscriber, so there is no second send.

The handle exposes `register_producer`, `retire_producer`, `producer_status`,
`admission_receipt` and `admit`. All use one mailbox with 16 waiting requests and
at most one executing request, driven by the existing graph task. Full mailboxes
return `Busy`; admission can also return `Busy` while a subscriber commits its
progress. Clients retry with the same session, sequence and unchanged input.
Only one graph admission owner can bind a channel, even through different handles.
No admission worker is spawned.

Before activation and after stop, requests return `Closed`. Cancelling a request
before the graph takes it prevents that request from executing. Stop/cancellation
closes queued requests. An interrupted executing write returns an uncertain
outcome: `AcceptanceUnknown` for input, `AcknowledgementUnknown` for producer
metadata. Await channel cleanup and reconstruct before resolving its receipt or
session. A normal idle stop permits rebinding; it does not reset saved sessions,
accepted input or subscriber obligations. A cancelled transaction does **not**
become safe merely by restarting the source.

New input returns `CapacityExhausted` while a full journal has required unhandled
work. Concurrent admission returns typed `AdmissionRejection::Busy`, not another
internal queue; callers must bound their own requests and retries. Invalid
session/sequence, conflict and expiry are also typed `AdmissionRejection` causes
inside `PipeError::Backend`.

`retire_producer` succeeds only after every active subscriber has passed that
producer's last input. Reusing the name then gets a new persisted epoch. A token
from the retired session or another journal cannot authorize new input.
Changing admission settings or the channel definition on reopen is rejected
until explicit transition policies exist. Raw `publish` cannot bypass sessions
to append new input on an admission-enabled channel.

Fast channels do not enable this service: they create no producer ledger, hash
no payloads and gain no admission worker. Existing metadata without admission
keeps its prior serialized shape.

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

Wrapped source input also has two positions: the original source/WAL position
used to suppress repeated computation, and the consumed transport sequence used
by the pipe. The transaction stores these separately. Replayed logical input can
advance transport progress without evaluating the query or transactional steps
again. A `WalReplaySource` connected to its owning `QuerySourceProgress` restores
both positions so reconstruction does not reuse a transport sequence already
accepted by a durable pipe.

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
