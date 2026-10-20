# Maintaining ComputationGraph

**Implementation baseline: 20 October 2026 development checkout, `agentofreality-parallel-computation-graph`.**

The current-code descriptions were checked against Core
[`54735c05`](https://github.com/drasi-project/drasi-core/commit/54735c056ca45bd784462cd462c14f0a514de442),
with matching Server integration at
[`15dafa2`](https://github.com/drasi-project/drasi-server/commit/15dafa225ac76ba9868452a7082f693027621ad7).

This guide is for engineers changing Drasi itself: its runtime, query execution,
storage integration, component lifecycle or plugin hosting. For building a
solution *with* Drasi, use the [application developer guide](developer-guide/README.md).
For exact signatures and configuration fields, use the
[implementation reference](computation-graph-reference.md).

For current migration status and completion criteria, see the
[cleanup plan](computation-graph-cleanup-plan.md). The
[remaining-work list](computation-graph-remaining-work.md) puts unfinished work
in priority order, with a description, effort estimate and completion condition
for each item.

ComputationGraph makes the processing topology an executable, owned structure.
It decides which component instances exist, which connections they use, when
their work runs, and what must finish before they can be replaced. That common
foundation supports both ordinary source-query-reaction solutions and native
pipelines containing other transformers, sinks and services.

The main benefit is not another way to draw the same dependencies. It is a
common place to enforce execution, connection and ownership rules that would
otherwise have to be coordinated separately by each component category.
Durability builds on those rules, but remains explicitly configured.

Two different meanings of "graph" matter here. The **ComputationGraph** is the
runtime topology, including ordinary queries and their helpers. It is not the
**data graph** of nodes and relationships evaluated by Cypher/GQL. Changing
runtime wiring does not require reimplementing graph matching.

## Reading map

1. [What changed from ComponentGraph](#what-changed-from-componentgraph)
2. [Ownership and the two assembly paths](#ownership-and-the-two-assembly-paths)
3. [Following one change through the runtime](#following-one-change-through-the-runtime)
4. [Scheduling, readiness and lifecycle](#scheduling-readiness-and-lifecycle)
5. [Data contracts, identity and ordering](#data-contracts-identity-and-ordering)
6. [Query execution, bootstrap and scheduled work](#query-execution-bootstrap-and-scheduled-work)
7. [Stateful processing outside a query](#stateful-processing-outside-a-query)
8. [Recovery is a chain of completion boundaries](#recovery-is-a-chain-of-completion-boundaries)
9. [Resource ownership and cancellation-safe cleanup](#resource-ownership-and-cancellation-safe-cleanup)
10. [Live changes and persisted desired state](#live-changes-and-persisted-desired-state)
11. [Plugin hosting without a second executor](#plugin-hosting-without-a-second-executor)
12. [Inspection and diagnosing a running system](#inspection-and-diagnosing-a-running-system)
13. [Making and validating a change](#making-and-validating-a-change)

Code links point into this checkout; named symbols identify useful entry points
without relying on line numbers that move with edits. Historical links are
pinned to the inspected ComponentGraph-based revision.

## What changed from ComponentGraph

The old ComponentGraph was already a shared source of component metadata,
relationships, runtime-instance references and lifecycle history. Its managers
already used registry-first registration. Do not attribute those ideas, or
continuous queries and recovery in general, exclusively to ComputationGraph.
Ordinary query workers were already independently spawned; multicore execution
is not itself new. The [capability comparison](componentgraph-vs-computationgraph.md#new-built-in-capabilities)
distinguishes new native contracts from preserved or generalized behavior.

The structural change is **who coordinates the work**:

| Concern | ComponentGraph-based arrangement | ComputationGraph arrangement |
|---|---|---|
| Execution | Source, Query and Reaction managers coordinated their respective runtime paths around the shared graph | A graph controller owns component operations and connection lifecycles; ordinary APIs adapt to it |
| Composition | Runtime paths were specialized around sources, continuous queries and reactions | Typed ports and pipes compose sources, arbitrary transformers, queries, sinks and portless services |
| Status | Managers and components sent updates to an asynchronous graph-update task | The controller publishes execution state; compatibility status/events are derived at that publication boundary |
| Connection guarantees | Subscription and queue behavior was implemented within the specialized paths | Connections declare and negotiate schema, flow-control and completion requirements |
| Replacement | Manager-specific lifecycle orchestration | Revision-, generation- and operation-checked changes coordinated with affected components and resources |
| Recovery reasoning | Primarily source/query/reaction-specific recovery contracts | Those paths remain supported; native components can also expose processing, publication, replay and destination obligations |

This enables reusable processing without a query, transactional transformer
sequences, durable multicast, explicit source-time merging and capability-checked
recovery paths. It also gives maintainers one ownership model to extend instead
of adding a new manager/executor for each processing shape.

The latest native fan-out work closes a migration restriction, not a missing
requirement in the old product. A source can feed several transformers/queries;
a transformer can feed several transformers and sinks. One producer is reused,
while each required consumer keeps its own progress owner and recovery boundary.
This is separate from choosing a multicast journal for the connecting pipes.

For the previous design, read the pinned
[ComponentGraph module](https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/component_graph/mod.rs),
[manager-owned query path](https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/queries/manager.rs)
and [status-update loop in `lib_core.rs`](https://github.com/drasi-project/drasi-core/blob/261b8d40d35ce203a7f4aaae0028956d76a8def4/lib/src/lib_core.rs).
The current [`component_graph/graph.rs`](../src/component_graph/graph.rs) is
deliberately different: its `ComponentGraphView` holds a weak runtime reference
and provides read-only views. It is not a second mutable runtime.
The former `ComponentGraph` alias has been removed.

Snapshot names also distinguish what is being captured:
`config::QueryConfigurationSnapshot` contains a query's configuration and status;
`computation::v1::QueryResultSnapshot` contains its result rows, output sequence
and generation. The former `QuerySnapshot` aliases have been removed; import
the explicit type for the boundary being inspected.

## Ownership and the two assembly paths

### One instance owner, directly hosted queries

Each DrasiLib instance has one root ComputationGraph. Ordinary component
registrations and native component batches share that instance namespace.
Adding a batch does not create a competing runtime.

Ordinary Source and Reaction objects appear as graph-owned service
adapters at the root. Their semantic kinds remain Source and Reaction;
"service" describes their execution interface, not how users should see them.
Ordinary query APIs assemble a `ContinuousQueryTransformer` and its subscription,
timer and output helpers directly in the same graph. The query uses the existing
Core evaluator. There is no separate `QueryInstance` execution host or private
query graph.

```mermaid
flowchart TB
    D[DrasiLib] -->|owns| R[Root ComputationGraph]
    R -->|owns| S[Ordinary source adapter]
    R -->|owns| Q[ContinuousQueryTransformer]
    R -->|owns| A[Ordinary reaction adapter]
    R -->|owns| N[Explicit native components]
    Q -->|uses| EV[Existing drasi-core query engine]
    R --> SR[Source subscription readers]
    R --> TM[Scheduled-work source]
    R --> OUT[Query-results outlet]
    SR -. borrows source host .-> S
    A -. subscribes through catalog .-> OUT
```

Solid arrows here mean ownership, not event delivery. The source plugin can
serve multiple query subscriptions without each query becoming an owner of
that plugin's lifecycle. Query-local subscriptions and storage still have
their own cleanup obligations.

Read [`instance.rs`](../src/computation/instance.rs) for the root driver and
graph leases; [`runtime/component.rs`](../src/computation/runtime/component.rs)
for ordinary component realization; and
[`runtime/query_assembly.rs`](../src/computation/runtime/query_assembly.rs) for
ordinary query declarations and assembly. Generation-fenced lifecycle groups
start/stop the query and its exclusive helpers; the existing root task registry
retains all processing and cleanup when a caller stops waiting.
Stop decisions must inspect every group member: a stopped evaluator can still
have active subscription or output helpers. Ordinary stop APIs must finish those
helpers before reporting completion, without stopping a shared source.

Failed definitions remain inspectable and retryable. The declaration resource
contains configuration-related metadata and any typed preparation error; it does
not run the query. Ordinary and directly constructed queries now use the actual
graph ID and query ID, with the enclosing instance scope, for storage and producer
identity. The retired `lib-query/<hex ID>` namespace is not adopted or translated;
this is part of the breaking release and does not delete the old files.
Current-release reconstruction must keep these identities stable. Catalog
transport counters survive replacement so a new component generation cannot
reuse an already-published stream position.

There is no query execution compatibility switch or legacy hash reader. The
native configuration record and fingerprint are shared by both admission paths.
Source order and middleware order remain meaningful; label sets and join
declarations are canonicalized. Operational capacity/startup changes do not
invalidate processing identity.

Source-facing memory queries explicitly include volatile checkpoints alongside
native memory indexes. The minimal processing-only Core memory provider does
not acquire that extra capability. Persistent queries require persistent
checkpoints, an outbox and live results; those interfaces alone do not establish
atomicity without their actual shared transaction. Reset and deprovisioning
propagate unsupported cleanup errors. Current reset markers require their
generation, and stored output must already carry its producer identity.

The result timestamp is production time, including scheduled results; the
envelope retains logical event/due time separately. Ordinary result metadata is
projected when crossing the result API boundary, without adding those duplicate
fields to the native execution path.

Completed bootstrap is reused on clean restart. `has_pending_snapshot` requests
an additional prepared snapshot, for example a source subscription with no
checkpoint; it defaults to false in both Rust and native SDK contracts. Initial
bootstrap and explicit reset can still request a snapshot without that refresh
signal. Native preparation still permits only one snapshot and retains cleanup
ownership across cancellation.

### Ordinary APIs are assembly, not a fallback engine

The ordinary APIs preserve familiar configuration and plugin interfaces.
ComputationGraph is the only engine, including in `--no-default-features`
builds; there is no DrasiLib runtime-selection feature.
[`runtime/mod.rs`](../src/computation/runtime/mod.rs) translates those
registrations into graph declarations and retrieves current objects from
registry publications. The retained
[`sources/manager.rs`](../src/sources/manager.rs),
[`queries/manager.rs`](../src/queries/manager.rs) and
[`reactions/manager.rs`](../src/reactions/manager.rs) are facades.
Do not add authoritative component maps or new processing loops to them.

[`pipeline.rs`](../src/computation/v1/pipeline.rs),
`ComputationPipelineBuilder::build_with_subscriptions`, is the main wiring
point for the ordinary query path. It selects the actual index provider,
creates the ranked inbox and query resources, and connects subscription,
evaluation, timer and outlet components. A bug in how ordinary queries are
assembled usually belongs here or in their runtime adapter, not in the core
evaluator.

### Native assembly shares execution and ownership

Native components implement the graph contracts directly. Their data paths
can connect source to transformer to sink without an ordinary Query object.
Ordinary and explicitly assembled queries use the same native query component.
Explicit assembly can omit ordinary configuration metadata and API adapters;
neither path creates a private query graph or an additional execution host.
An explicitly supplied source-host resource need not have a same-named graph
component. A host borrowed from an actual instance source retains that source's
dependency edges; source names in query metadata alone do not prove ownership.

Factories describe reconstructible components and resolve declared resources;
supplied objects provide already-constructed instances. Both enter the same
graph ownership model. An opaque supplied object does not become reconstructible
merely because inspection can display it.

Start with [`component.rs`](../src/computation/v1/component.rs) for execution
boundaries and [`graph/specification.rs`](../src/computation/v1/graph/specification.rs)
for construction and resource contracts. The
[ordinary/native pipeline parity tests](../tests/computation_pipeline_parity.rs)
protect the shared behavior.

## Following one change through the runtime

Consider an ordinary source feeding a continuous query and reaction:

1. The source produces a `SourceEventWrapper`, including source-change time,
   sequence and any resumable source position. Its owned subscription supplies
   the event to a query-local reader.
2. The reader converts it to a native graph-change envelope. A ranked pipe
   admits it to that query's shared bounded inbox, applying the source's
   backpressure or drop policy.
3. The graph selects an available input and invokes the query transformer.
   The transformer interprets source/progress metadata and calls the shared
   evaluator through its actual storage owner.
4. With atomic publication, index mutations, input progress, result projection
   and output history are committed together. No downstream send is part of
   that transaction unless explicitly using shared output storage.
5. The graph forwards emitted envelopes through the selected output pipes.
   When every required branch has accepted a batch, it invokes the transformer's
   delivery-confirmation hook and drains any bounded continuations.
6. The query-results outlet/catalog exposes committed results to ordinary
   subscriptions. The reaction adapter applies its declared handling/recovery
   contract. Acceptance into a reaction's own queue is not proof that an
   external effect completed.

For a fully native pipeline, replace steps 1-2 and 6 with the source and sink's
direct envelope contracts. Graph-owned processing tasks enforce routing,
validation and confirmation; the controller coordinates their lifecycle and
owned cleanup rather than polling their business futures inline.

[`graph.rs`](../src/computation/v1/graph.rs), `run_node`, is the best single
place to understand this loop. It keeps at most one pending receive per input
edge, validates output batches before forwarding, and completes an input's
acknowledgement only after the relevant processing/forwarding/continuation
work succeeds. A buffering transformer can return no output; if that return
acknowledges recoverable input, the transformer must already have persisted it.

The compatibility boundaries are in
[`plugin_source.rs`](../src/computation/v1/plugin_source.rs),
[`graph_codec.rs`](../../components/computation-contracts/src/graph_codec.rs),
[`query_catalog.rs`](../src/computation/v1/query_catalog.rs) and
[`runtime/reaction.rs`](../src/computation/runtime/reaction.rs).
Follow those when data arrives correctly at one interface but disappears or
changes meaning at another.

**Fan-out is not a transaction.** Independent branches are sent sequentially;
a slow branch backpressures the producer, and earlier branches may have
accepted an emission when a later send fails. Forwarding errors retain that
partial-acceptance information. The exception is one append shared by several
subscribers of the same QoS channel, not arbitrary graph-wide atomic delivery.

## Scheduling, readiness and lifecycle

### Owned native tasks and independent query work

Native processing, lifecycle/control and supported resource operations run in
graph-owned Tokio tasks. The controller observes completions rather than polling
their business futures inline. Graph-owned task/result registries outlive
cancelled waiters and the controller future until cleanup joins the actual work.
Mutable operations on an instance remain serialized.

Ordinary queries use these same per-component tasks, not private graph drivers.
On a multithreaded Tokio runtime, independent query and other native component
operations can use different workers.
Current-thread operation remains supported but is not CPU parallelism. A
transaction container's inner steps remain within its one transaction/runner.
Measured capacity still depends on contention, work size and transport.

See [`execution.rs`](../src/computation/v1/graph/execution.rs) and
[`resource_tasks.rs`](../src/computation/v1/graph/resource_tasks.rs) for retained
task/result ownership. Initial factory deployment is dependency-aware. Factory
and provider preflight, pipe construction, in-place reconfiguration, superseded
resource metadata and retired object destruction use retained task boundaries
too. A retirement failure fences the affected replacement before construction
or activation; cleanup success cannot clear generation damage.

Scoped force validates the complete actual resource domain before interruption
and joins the processing/control work it reports as interrupted. A selected
never-started or already-quiesced member can still have an idle graph-owned
peer-control observer. Force does not falsely report that observer as interrupted
or wait for its unrequested termination; it remains owned for later lifecycle
use and is joined when the graph is disposed. Both runtime flavors protect this
case in `computation_task_execution`.

Successful force cleanup does not repair a damaged generation. Recovery
retirement must still reject it. Reconstruct a fresh normal owner from the
accepted configuration and actually recover the producer before changing its
output bindings; construction alone is not recovery. The Server held-Handled
sink tests preserve pending output through that sequence instead of aborting
handling under a successful graceful-stop claim.

Awaited reconciliation phases service the same bounded command dispatcher.
Reservations reject conflicting structural changes while disjoint lifecycle
operations continue. Resource reservations include exact provider aliases and
use weak identities so they cannot keep retired storage alive. Resource/plugin
notifications are generation-fenced and deferred until reconciliation finishes,
keeping only the latest notification of each kind per registered component.
This prevents an inventory update from invalidating or being overwritten by
an owned preflight.

[`graph/controller.rs`](../src/computation/v1/graph/controller.rs) manages
active operations and their completions. `NodeWorkBudget` in
[`graph.rs`](../src/computation/v1/graph.rs) bounds continuously ready work
per poll and requeues the **node's** waker. Suspension resets the budget.
This prevents a ready producer or timer burst from monopolizing its data-runner
poll and executor worker without imposing periodic extra yields on paths already
waiting for I/O. It is not a budget around arbitrary service/plugin code and
cannot preempt a component that never yields.

The [query responsiveness tests](../tests/computation_query_responsiveness.rs)
and [graph dataflow tests](../tests/computation_graph_dataflow.rs) are useful
when changing fairness, continuation or fan-in behavior.

### Idleness is not exhaustion

A source with no current event must wait in `next`, not return `None`.
`None` means true exhaustion and allows downstream draining to finish. A
stopped or failed source is not automatically an exhausted source.

Transformers can request timer wakeups without receiving input, and pending
continuations run before another input is processed. This lets a merger drain
buffered events even after finite sources end. Keep those paths in `run_node`
when modifying its receive loop; "no input ready" does not always mean
"no work remains."

### Declaration, construction and readiness are different facts

An accepted declaration reserves an identity and can remain visible even when
construction fails. A constructed component may still be waiting for required
connections or readiness. A start hook can finish before an asynchronously
starting plugin is actually ready.

Consequently, the graph tracks creation state, execution state and health
separately. Do not treat "the graph is running" as "every component is healthy,"
or successful addition as successful startup. Caller-side failure to construct
a supplied object remains outside graph registration.

Strict construction rejects cycles, role/port/schema mismatches and unmet
connection capabilities before execution. Incremental assembly can retain
unresolved declarations until dependencies arrive. This distinction lets a
live system describe an incomplete intended topology without pretending that
it is already executable.

Readiness and neighbor notifications use separate bounded control channels.
They need not wait behind a full data pipe. Components can address connected
neighbors, not arbitrary graph members. Requiring readiness on both sides can
still create an application-level startup deadlock.

[`lifecycle.rs`](../src/computation/v1/lifecycle.rs) defines the state model;
[`control.rs`](../src/computation/v1/control.rs) owns control-message rules.
The controller's `begin_start`, `advance_start` and `complete` paths connect
them. Use [addition/control tests](../tests/computation_addition_control.rs)
when changing acceptance, readiness or reported failures.

### Returned failures belong to nodes, not the controller

`GraphError::failure_disposition` is the common classification policy.
`ComponentOperationError` carries an explicit disposition and the original
typed cause through `anyhow` context. Recognized transient I/O is retryable;
unclassified errors, invalid output/protocol, corruption, failed rollback,
uncertain acceptance/acknowledgement and partial fan-out are terminal.
Retryable is eligibility for recovery, not proof of rollback or permission to
reuse an owner whose transaction or cleanup is fenced.

A returned component error leaves its generation failed and inspectable.
Independent branches and management continue. Both `run()` and an automatically
started scope retain failed nodes; `start()` completes naturally only when the
finite work ends without remaining failures. Observe the failure or await a
lifecycle report rather than expecting `GraphRun` to terminate on a node error.
Explicit stop/start or reconstruction remains subject to cleanup and recovery
checks. No implicit retry restarts an unchanged terminal failure.

Managed reconciliation attempts a retryable member at most once per explicit
reconciliation request. There is no background reconstruction retry scheduler.
`DeliveryRunner` separately applies its configured attempt limit and delay
(one attempt by default); an exhausted handling budget becomes terminal instead
of implicitly resetting the budget through graph reconciliation.

Control-handler errors are recorded with `FailurePhase::Control` and degraded
health; the inbox is rearmed so a rejected custom message cannot disable later
requests. Graph-generated readiness/availability notices use a separate
latest-state slot per connected peer, direction and category, so a full custom
queue cannot suppress them. They are not an event history. Public command
fan-out still reserves every recipient atomically or accepts none. Rebind and
replacement invalidate both kinds of stale messages.

`ComponentPanic` marks damaged generations; protected operation and destruction
boundaries retain the instance for diagnosis/cleanup and refuse same-generation
restart. SDK `PANICKED` results preserve that marker through normal,
transactional, consumer and bootstrap Host calls. Start completion racing a
control panic must take the local quarantine path, never unwind the controller.
Native abort/memory-fault isolation and complete coverage of every synchronous
callback are not implied.

The separate [instance host](../../components/instance-host/README.md) runs a
normal DrasiLib per process; [bridge sources/sinks](../../components/computation-plugins/common/bridge/README.md)
carry data. This is composition, not a special remote-component execution mode.
Local child/group termination and inter-instance message recovery remain distinct.

### Stale work cannot act on a replacement

Several identities solve different races. A graph revision identifies the
declaration being changed; a component generation identifies the constructed
instance; an operation epoch identifies a particular operation on that instance.
Connections also have binding generations.

For example, a slow readiness callback from generation 7 must not mark a
replacement generation 8 ready. A completed stop from an earlier operation
must not finish a newer restart request. `Operations::complete` checks the
captured generation and epoch before publishing the completion.

Quiescence pauses processing without running a fresh start hook. Stop runs
cleanup needed for the component's supported restart behavior. Permanent
deprovisioning is a separate, explicit state-erasure operation. These are not
interchangeable implementations of "make it inactive."

Stopping the whole instance is a sweep, not a transaction. `DrasiLib::stop()`
stops consumers before producers and attempts every component even when one
fails, then reports all failures together. A record that a concurrent
configuration change replaced or removed is skipped, because that change now
owns the component's lifecycle. An incomplete stop is remembered, so calling
`stop()` again retries the remaining components instead of reporting "already
stopped." See `stop_all` in [`runtime/mod.rs`](../src/computation/runtime/mod.rs)
and [`lib_core.rs`](../src/lib_core.rs).

Component processing failures, invalid emissions and failed outgoing delivery
are recorded on the affected component; relationship policy determines the
impact on dependents. They do not by themselves terminate the graph controller
or unrelated runnable branches. Preserve partial-acceptance information when
one destination accepted before another failed. Actual graph-driver/internal
defects are a separate failure boundary and can end that run; report them as
such rather than conflating them with a plugin failure. Preserve phase,
generation and typed cause instead of converting all outcomes to one status string.

## Data contracts, identity and ordering

### Envelopes carry data; resource capabilities stay outside them

A native envelope contains an immutable change set, producer metadata,
annotations and lineage. A change set preserves operation order and distinguishes
full, partial and patch record images. Its schema includes exact identity,
version, encoding and definition bytes; executable validators check records.
A matching schema name or fingerprint is not sufficient compatibility proof.
Schema compatibility also does not convert arbitrary records into Cypher data:
the graph-change and query-result adapters provide that semantic mapping.

Immutable payloads are shared on in-process fan-out. Appending an annotation
creates branch-local history without mutating another branch. A transformer
derives a new envelope with its own stream/sequence and retains the input's
context and lineage. Lineage records ancestry; it is not a multi-input commit
protocol or a checkpoint authority.

This is not end-to-end zero-copy: compatibility conversion, decoding,
persistence and FFI still allocate or serialize. Keep transactions,
acknowledgements and storage handles out of annotations; those are local
capabilities with ownership rules, not ordinary immutable data.

The shared implementations live in `drasi-computation-contracts`; runtime
imports re-export the same types. Read
[`data.rs`](../../components/computation-contracts/src/data.rs) for validation,
[`envelope.rs`](../../components/computation-contracts/src/envelope.rs) for sharing/derivation and
[`ports.rs`](../../components/computation-contracts/src/ports.rs) for connection compatibility.
[`codec.rs`](../../components/computation-contracts/src/codec.rs) is the persisted JSON envelope
format; [`binary_codec.rs`](../../components/computation-contracts/src/binary_codec.rs) is the
full-fidelity binary boundary. Changing one does not silently migrate the other.
Tests in [computation_codec.rs](../tests/computation_codec.rs) and
[computation_graph_codec.rs](../tests/computation_graph_codec.rs) protect
round trips and meaning, not just successful deserialization.

### Event time and progress sequence answer different questions

**Source-change time determines temporal order; progress identity determines
what has been processed.** Neither should be replaced by wall-clock receipt
time or an arbitrary property in the record.

Every output stream has one producer and increasing transport emission
sequences. A replay can emit the same logical output again with a new transport
sequence. Persistent queries additionally carry query identity/generation and
logical result positions. Stateful graph producers carry their own logical
progress.

This distinction becomes essential after transformation. Suppose source events
1 and 2 become merger outputs 2 and 1 because event 2 has an earlier source
time. A downstream query must checkpoint the **merger's** output sequence,
not inherited raw source sequence 2 and then incorrectly suppress event 1.

[`producer_progress.rs`](../src/computation/v1/producer_progress.rs),
`GraphInputProgress::from_envelope`, distinguishes immediate producer progress
from inherited provenance. [`query_identity.rs`](../src/computation/v1/query_identity.rs)
handles query recovery identity.
[`output_identity.rs`](../src/computation/v1/output_identity.rs) canonicalizes
retry comparisons for replay and delivery. Reuse these rules rather than
inventing an identity from just a timestamp, record key or transport envelope ID.
The [producer-provenance tests](../tests/computation_producer_provenance.rs)
cover the distinction through derived streams.

For a validated stateless path to an actual consumer-progress owner,
`GraphInputProgress::with_consumer_boundary` can restore the original producer
boundary. `GraphProducerProgress::read_boundary` reads its actual metadata
from the envelope lineage; matching a source name or copying an annotation is
not proof. Retained paths preserve already queued input through supported
replacement.

Carry the actual producer role for normal and batched inputs, including the
validated original boundary. An ordinary transformer that expands one query
result into two envelopes must not label both outputs as the ancestor query's
same logical result and discard the second as a duplicate. Conversely, a
validated stateless relay must preserve an original query's logical result
position rather than substitute its transport counter. Original and immediate
transport progress advance independently. The
[handled-input regressions](../tests/computation_input_progress.rs) cover both
cases and completion after continuations.

### Ordinary ordering preserves each producer's progress

The ordinary query inbox is a `RankedInputQueue` shared across its sources.
It compares the earliest admitted sequence from each producer by source time,
source-list rank and sequence. It does not move a later sequence ahead of an
earlier outstanding event from that producer, even if timestamps go backwards.
Otherwise a simple high-water checkpoint could skip unprocessed input.

The source's dispatch mode chooses blocking versus drop-on-full admission to
this inbox; the query's dispatch mode governs its output subscribers. Changing
source-list order changes query configuration identity. Inspect
[`ranked_pipe.rs`](../src/computation/v1/ranked_pipe.rs) and the shared
[`channels/priority_queue.rs`](../src/channels/priority_queue.rs) before changing
these rules.

Direct native fan-in defaults to arrival selection. Its
`EventTimeAcrossStreams` policy compares currently available stream heads.
Neither mechanism waits for unseen events or establishes a cross-source
watermark.

### Explicit source-time merging owns the reorder problem

`SourceTimeMergeTransformer` is the opt-in component for actual buffering and
within-source reordering. Every input must carry a source-change timestamp.
The source must supply it; the generic envelope type being able to represent
an absent timestamp does not authorize a fallback in this transformer.

The merger keeps a time-ordered buffer and a separate residence-deadline index.
An event becomes eligible when every active source has progressed far enough
beyond it, or when a residence deadline expires. Optional idleness removes a
quiet source from the early-release calculation. The component emits the
earliest eligible buffered event, one per continuation, under downstream
backpressure.

An expired residence deadline makes the ordered prefix through that event
eligible; maximum wait is not a downstream delivery deadline. Idleness is
not proof of completeness: when all sources are idle, residence deadlines
still govern release. Local monotonic time controls waiting, never event order.

For example, with outputs already advanced to source time 100, an arriving
event at 90 cannot enter the main output without breaking order. The default
policy retains it and fails. Explicit alternatives route it to a separate
late port or discard it with a warning/counter. Equal times are allowed;
configured source rank breaks ties among events buffered together.

Count and binary-input-byte limits include buffered, held and unconfirmed
events. Full capacity rejects a new input rather than silently evicting one.
Durable mode saves buffer, progress, frontier and pending output in an atomic
bounded snapshot. Reopen conservatively restarts residence waits. Snapshot
writes grow with buffer size; this is not a paged large-window reorder engine.

The design is in [`time_merge.rs`](../src/computation/v1/time_merge.rs) and
[`time_merge/recovery.rs`](../src/computation/v1/time_merge/recovery.rs);
[time-merge tests](../tests/computation_time_merge.rs) cover both time behavior
and graph integration. The [feature guide](computation-graph-time-merge.md)
defines exact policies and administrative held-event resolution. There is no
explicit source-watermark protocol, retroactive query correction, or standard
Server/factory recipe for this component.

## Query execution, bootstrap and scheduled work

### The runtime wraps the evaluator; it does not replace it

`ContinuousQueryTransformer` handles graph input/output, query configuration,
progress, recovery and result publication. The standard `ContinuousQueryFactory`
constructs that component directly with output-delivery tracking enabled.
`TransactionTransformer` retains compatible query constructors for direct
assembly. Direct construction and factory construction must not be assumed
to have identical output-handoff configuration.

The bridge to the engine is
[`core/src/computation/query_adapter.rs`](../../core/src/computation/query_adapter.rs).
`ComputationQuery::try_build` binds the evaluator to the supplied resource
bundle. Evaluation uses
[`QueryEvaluator`](../../core/src/query/evaluator.rs), reached through the
shared [query builder](../../core/src/query/query_builder.rs). Cypher and GQL
still feed shared evaluation logic; the runtime does not have a private
implementation of matching, aggregation or change detection.

This gives a practical debugging split: wrong matching/aggregation with a
correct input belongs in the evaluator and its indexes; missing/repeated input,
wrong progress or bad publication belongs in the computation adapter/runtime.
Do not compensate for an evaluator bug by adding query-language logic to
`run_node`.

In [`query.rs`](../src/computation/v1/query.rs), follow `process` from input
progress validation through the evaluator's pre-commit hook. That hook stages
result changes, materialized result rows and source checkpoints while the
index transaction is still open. The local ready result view is updated after
successful commit. The projection answers "what are the current rows?";
the outbox answers "what result changes can a consumer replay?" They are not
interchangeable storage.

The query output's logical position also differs from its retry transport
position. [`query_delivery.rs`](../src/computation/v1/query_delivery.rs)
restores unconfirmed output, assigns replay transport sequences and confirms
handoff. A crash after commit must replay output rather than reevaluate the
same input against already-updated state.

Use [query fault tests](../tests/computation_query_faults.rs) and
[query recovery tests](../tests/computation_query_recovery.rs) when changing
these boundaries, not only happy-path result tests.

### Mutation integrity uses source time

The shared evaluator admits mutations by source-scoped element identity and
`ElementMetadata.effective_from` (epoch milliseconds). This is not receipt time,
transport sequence, a property, or the source-time merger's arrival policy.
Strictly older inserts, updates and deletes are skipped with a structured warning
containing the identity and incoming/stored timestamps. Processing continues and
the native query commits input progress even when no result changes.

Equal-time distinct changes follow admitted order; an identical full element is
a no-op. Insert against an existing identity is a full-row upsert: retract the old
contribution before adding the new one, removing omitted properties. Update
preserves sparse-property merge semantics. A live identity cannot change between
node and relationship; a typed error rejects the mutation. A deletion, including
deletion of an absent element, writes a source-time tombstone. Newer recreation
clears it in the same index transaction. Equal-time recreation follows the same
admitted-order rule.

Deletion frontiers use the appended `ResultOwner::ElementTombstone` namespace,
not a separate volatile map. They survive reconstruction, participate in actual
rollback and bypass caches that have no rollback hooks. There is no automatic
frontier garbage collection: one record per deleted identity remains until
recreation or explicit state reset. Safe pruning needs a source watermark/
retention contract; elapsed wall time is not sufficient.

Native query configuration identities are version 5 (ordinary change inputs)
and 6 (complete source transactions), including the shared mutation-semantics
version. Stored identities from the earlier semantics are intentionally rejected
as `ConfigurationChanged`, not silently reused or migrated. Plan an explicit
reset/rebootstrap or a separate migration while preserving pending delivery
obligations. Older stores cannot reconstruct missing deletion history from live
rows alone. Changing source clocks or correcting a value with an older timestamp
also requires an explicit upstream policy, not bypassing the guard.

[Mutation integrity tests](../tests/computation_mutation_integrity.rs) exercise
real RocksDB rollback/reopen, input checkpoints, independent result models and
rejection of incompatible stored identities.

### A source transaction is more than a vector of changes

An ordinary batch preserves its per-change evaluation behavior. An explicitly
complete source transaction carries framing, identity, a commit position and
admission limits. `SourceTransactionBuilder` cannot publish an unfinished or
failed assembly. The query must opt into this schema and atomic publication.

The engine evaluates the group under one transaction and consolidates its final
row changes. For example, moving value between two records need not expose an
intermediate total that exists only halfway through the source transaction.
This boundary stops at that query's publication; it does not atomically update
every downstream query or external consumer.

Read [`source_transaction.rs`](../src/computation/v1/source_transaction.rs) for
framing and limits, then `evaluate_source_transaction` in the core adapter and
[`query_results.rs`](../../core/src/computation/query_results.rs) for result
consolidation. [Source-transaction tests](../tests/computation_source_transactions.rs)
cover incomplete input and final-result semantics.

### Bootstrap establishes a cut between snapshot and live input

Bootstrap is not simply "insert some initial rows." The provider must establish
subscription readiness, load the snapshot, and identify the source positions
from which live/replayed input continues without a gap.

The query-scoped bootstrap contract separates preparation, snapshot streaming
and completion watermarks. Initial rows can be loaded incrementally; the
completed-bootstrap marker and final source boundaries are committed only
when the handover is complete. A partial load remains incomplete. No marker
for one source may invent a checkpoint for another source with no saved progress.

Some connectors need external initialization, such as creating a capture
resource. Borrowed `BootstrapState` stores bounded initialization intent in
the query's actual transaction domain before those effects begin. Final
handover state commits with the completion marker/watermarks. It survives
explicit query reset so unfinished connector work can be recognized, but
deprovisioning removes it.

[`query_bootstrap.rs`](../src/computation/v1/query_bootstrap.rs) defines this
boundary; [`query_recovery.rs`](../src/computation/v1/query_recovery.rs) applies
it during recovery. Dropping a snapshot stream requests cancellation, but
query cleanup must still await the provider's stop hook before releasing storage.
The rows are not one fictitious source transaction.

### Composite bootstrap retains the actual child lifecycles

[`CompositeBootstrapProvider`](../src/computation/v1/composite_bootstrap.rs)
combines 1 through 32 ordered, distinct provider resources for one real query.
Each child validates the instance/graph/query scope, and recovery-bearing
children must share the same actual local progress owner. Neither same-named
readers nor the same provider under two resource IDs are valid substitutes.

The composite streams children through the existing snapshot interface without
buffering every snapshot. Its version-1 record namespaces optional child state
by ordered resource ID within the existing 65,536-byte initialization bound.
Changed membership/order cannot reinterpret a retained record. Completion state
and at most 256 distinct watermarks enter the existing query transaction;
partial snapshots publish no completed handover.

Preparation aggregates `ResetRequired` before `RefreshVolatile` before `Ready`.
A non-Ready result stops and rearms **all** children before retry, including
siblings that returned Ready. Otherwise a memory PostgreSQL refresh combined
with an already-prepared HTTP child would violate that child's sealed native
lifecycle. The query's recovery policy still decides whether a persistent
incomplete initialization may be reset.

Cancelled streams and failed operations retain cleanup ownership until awaited
stop succeeds. Retirement holds the actual child leases, not sampled stopped
flags; partial acquisition must resume acquired children on rejection and
retain any unresolved cleanup. Do not replace this with a list of factory names
or release children when a caller stops waiting.

Host/Server `CompositeBootstrapConfig` supplies the canonical
`{kind: "compositeBootstrap", component: <query>, providers: [<ordered IDs>]}`
recipe. It must be a graph-owned Bootstrap resource with exactly those child
Bootstrap dependencies, bound to the query's unchanged single `bootstrap` slot.
See the [Host resolver and validators](../../components/host-sdk/src/management/native.rs)
and [real static/dynamic PG/HTTP composition](../../components/host-sdk/tests/native_postgres_http_bootstrap.rs).
This adds no execution ABI, envelope schema or extra executor, and does not
upgrade an HTTP snapshot to a durable or globally atomic snapshot protocol.

### Timer notifications do not own scheduled work

Temporal evaluation stores due work in the query's future queue.
`QueryScheduledSource` observes a read-only committed view and emits a hint
that work is due. It never removes the work itself.

The query transaction pops actual due work, evaluates it, and commits its
removal with resulting state/output. If the notification is lost, the work is
still stored; if duplicated, it cannot by itself repeat a committed removal.
If evaluation rolls back, the due work must remain recoverable.

Scheduled hints have their own kind and transport progress. They must not
advance live source checkpoints. Source time, timer due time and local time
used to wait are distinct. Timer sleeps periodically recheck the wall clock;
this is not a general deterministic clock-injection framework.

Start in [`query_scheduling.rs`](../src/computation/v1/query_scheduling.rs),
then the core adapter's `evaluate_due_future` and the query's `on_wakeup`.
[Temporal-retraction tests](../tests/computation_temporal_retractions.rs)
and the scheduling mutation cases in
[query fault tests](../tests/computation_query_faults.rs) protect this ownership.

## Stateful processing outside a query

### Standalone middleware retains the state needed to interpret changes

`MiddlewareTransformer` reuses the existing middleware registry and runner.
It is not a second implementation of unwind, mapping or relabeling. It can
retain previous elements so state-dependent transforms can produce the right
updates and deletions.

That state creates a recovery obligation. Running an input through middleware
again after a crash can produce a different answer if the previous-element
state already advanced. Durable mode therefore commits previous-element state,
input progress and transformed output together, then replays unconfirmed output
without executing middleware again.

Read [`middleware.rs`](../src/computation/v1/middleware.rs) alongside
[`middleware_recovery.rs`](../src/computation/v1/middleware_recovery.rs).
[Middleware recovery tests](../tests/computation_middleware_recovery.rs)
exercise storage faults as well as output shape.

### Linear transactions are one component, not a graph inside a transaction

A linear `TransactionTransformer` invokes opted-in transactional steps directly,
without intermediate pipes or independently scheduled child nodes. Each step
receives an isolated view of state through a borrowed `TransactionContext`.
Only the container commits. Filtering is an empty change set; expansion is
multiple operations in the one returned batch.

For example, a normalize step and a stateful expansion step can either both
commit their state/output or neither commit. Connecting two ordinary stateful
transformers with a durable pipe gives two recoverable commits, not that same
atomic boundary.

Participants cannot put mutable business state in fields on `self`, start
workers, perform external effects or independently commit. A step error can
roll back indexed state; it cannot roll back an already-sent HTTP request.
Ordinary transformers do not become safe participants through a configuration
flag alone.

[`transaction_transformer.rs`](../src/computation/v1/transaction_transformer.rs)
contains both the linear container and the distinct query-body arrangement;
[`transaction_state.rs`](../src/computation/v1/transaction_state.rs) scopes
step state. [Transaction-transformer tests](../tests/computation_transaction_transformer.rs)
cover rollback, output replay and interrupted replacement.

## Recovery is a chain of completion boundaries

### A storage provider must prove a usable transaction boundary

A `ComputationIndexes` bundle contains the evaluator indexes and optional
checkpoint, output-history and live-result writers. The provider explicitly
identifies which resources participate in its actual session transaction.
Putting unrelated writers behind similarly named resources does not make
their updates atomic.

[`core/src/computation/indexes.rs`](../../core/src/computation/indexes.rs)
validates participation and constructs the atomic-result capability.
[`operation.rs`](../../core/src/computation/operation.rs) owns serialized
transactions, rollback and fencing. Backend implementations remain responsible
for honoring their participation and storage-survival declarations.

An interrupted operation or failed commit acknowledgement may have changed
storage even though the caller did not receive success. **Fencing** means
rejecting further work on that owner until cleanup/reconstruction resolves
the uncertainty. It is safer than treating every error as confirmed rollback.
This is also why stateful components need cancellation guards, not just an
`if result.is_err()` branch after an awaited operation.

Default inline-memory queries avoid persistent machinery by using direct
memory indexes and a no-op session controller. Explicit non-atomic publication
cannot satisfy an atomic-output claim. Preserve this distinction when extending
[`pipeline.rs`](../src/computation/v1/pipeline.rs) or
[`legacy_index.rs`](../src/computation/v1/legacy_index.rs).

### Queue acceptance, processing and effects are separate

For one durable producer, the important crash cuts are:

| Interruption point | Owner that must retain enough information |
|---|---|
| Before processing commits | Upstream source/journal must replay unacknowledged input |
| After processing commits, before output acceptance | Producer's saved output must replay without recomputation |
| After one output branch accepts, before all are confirmed | Producer retains the batch; accepted branches need replay deduplication if duplicates are unacceptable |
| After an external effect, before consumer progress commits | Destination-side idempotency or a destination transaction must prevent repeating the effect |

`delivery_completed` is the graph's all-branches-accepted callback. It does not
mean all sinks finished their business work. A component that clears persistent
pending output there must require an appropriate durable acceptance boundary.

Likewise, a sink's completion declaration describes what its successful handler
actually proves. A retained pipe cannot upgrade an acceptance-only plugin to
completed handling. [Recovery-contract tests](../tests/computation_recovery_contracts.rs)
exercise those mismatches.

### A shared producer observes independent actual owners

[`ConsumerProgressSet`](../src/computation/v1/consumer_progress.rs) binds one
producer to 1 through 256 actual `Arc<QuerySourceProgress>` owners in the same
graph. It preserves each member's readiness, persistence, failure, reset
generation and checkpoint map. It does not synthesize an aggregate checkpoint,
sequence, source cursor or persistence flag. Read-only views cannot replace
local owner proof, and dropping a reader does not retire an obligation.

[`graph/consumer_progress.rs`](../src/computation/v1/graph/consumer_progress.rs)
checks the actual required downstream paths and exact owner resources.
Missing prerequisites remain dependency waits; invalid owners or uncovered
branches are errors. Capture contracts at construction/activation and supported
reconfiguration, not per envelope. Readiness is an observation, not a lease
authorizing a concurrent reset or member replacement.

`ComponentRecovery::with_consumer_progress` exposes the actual set;
`with_consumer_positions` additionally retains a source-defined
`InputPositionPolicy`. Source positions may describe a WAL commit, a row within
a commit, a binlog boundary or a journal epoch. Every input/checkpoint pair must
be comparable in that source's domain. Invalid or incomparable positions fail;
they never fall back to numerical query-sequence comparison.

Replay and retention use each owner's actual physical coverage, including
independently captured snapshots. Emission floors separately include the
relevant logical and transport counters and retained allocations. Taking the
minimum or maximum of all counters as one cursor is incorrect. Queries,
durable middleware and transaction transformers filter physically covered
input per owner while retaining truthful transport progress.

This remains one producer with independent consumers. It is not one source per
query, a shared query transaction, or an acknowledgement inferred from queue
acceptance. Source-specific history retention remains an additional contract;
the progress set alone neither retains WAL nor makes volatile owners durable.

### The graph can own volatile handled-input progress

[`graph/input_progress.rs`](../src/computation/v1/graph/input_progress.rs) owns
the optional `ComponentRecovery::with_volatile_input_progress` publication for
standalone transformers and ordinary Handled sinks. This is a separate field
from atomic `committed_progress`, so incompatible owner claims are rejected
rather than overwritten. Accepted sinks, sources, services, atomic participants
and consumer wrappers are ineligible. A factory's ability to create transactional
steps does not exclude its separately constructed standalone transformer.

Publish only after handler success, all output acceptances, continuations and
completion hooks. Batched preflight computes prospective duplicate decisions
without publishing; only completed handling confirms them. Covered replay may
advance transport without replacing the handled cursor. Failure/cancellation
fences the owner, and restart requires successful cleanup; an unclean drop is
not release of writer ownership.

Same-instance clean stop/start retains the volatile handled prefix. A replacement
writer advances the generation and clears it. This is not persistent replay or
an external-effect transaction. Keep the large optional tracking state boxed
and off the default path. The
[normal, batched and interrupted-input tests](../tests/computation_input_progress.rs)
protect these boundaries.

### Retained and multicast pipes make obligations explicit

A bounded FIFO releases queue capacity on receipt. A lossless retained journal
must keep data until handling is acknowledged. QoS multicast extends that idea
to one append with independent subscriber cursors: the slowest required
subscriber can keep old data retained.

Temporary disconnection is not subscriber retirement. Otherwise reconnecting a
slow consumer would silently lose its backlog. Lossy retention and gap skipping
are explicit policies; skip diagnostics must follow confirmed cursor advancement,
not a cancelled attempt to advance it.

Read [`retained_pipe.rs`](../src/computation/v1/retained_pipe.rs) and
[`retained_store.rs`](../src/computation/v1/retained_store.rs) for a single
consumer, and [`qos_pipe.rs`](../src/computation/v1/qos_pipe.rs) for multicast.
[`journal_budget.rs`](../src/computation/v1/journal_budget.rs) handles optional
full-envelope byte accounting. Persistent page limits bound the payload cache,
not total startup validation: the owner still checks all retained history.

With page limits, a journal's in-memory entries hold only the cached page.
Retention and capacity decisions must therefore come from the journal's
metadata (oldest retained position and retained count), never from the cached
entries; otherwise an append can trim records that a subscriber has not
acknowledged. `retain_from` in [`qos_pipe.rs`](../src/computation/v1/qos_pipe.rs)
is the single place that plans this, for both private and shared journals.

Byte quotas are not a universal RSS bound. Metadata, in-flight envelopes,
component state and backend buffers are additional costs. Some pipe budgets
permit a lone oversized event; the source-time merger does not. Check the
specific owner's admission rule rather than generalizing from another queue.
Capacity/backpressure is also not an events-per-second rate limiter.

### Admission receipts and output replay receipts solve different retries

Source admission records a client session, consecutive client sequence and
content with the journal append. A retry returns the same bounded receipt
instead of appending again. `SourceAdmission` delegates through the graph's
mailbox to the actual outgoing channel, not a source-private worker or queue.

Output replay tracking instead recognizes an existing persistent producer's
logical output. It must not assign a fresh source identity to a replayed query
result. Receipt windows are bounded; an expired or content-conflicting identity
fails explicitly rather than becoming new input.

The two services live in [`qos_pipe/admission.rs`](../src/computation/v1/qos_pipe/admission.rs),
[`qos_pipe/ingress.rs`](../src/computation/v1/qos_pipe/ingress.rs) and
[`qos_pipe/replay.rs`](../src/computation/v1/qos_pipe/replay.rs).
They are alternative channel modes. Use
[QoS integration tests](../../lib-integration-tests/tests/computation_qos.rs)
for persistent receipt and membership behavior.

Tracked producers also persist destination membership. While an output is
pending, silently changing a port, subscriber or journal would erase an
obligation. [`output_bindings.rs`](../src/computation/v1/output_bindings.rs)
pins those identities. Replacing consumer configuration under the same
destination identity does not create a new obligation or erase the old one.

### Shared storage removes one commit gap, not every gap

`SharedStorageGroup` places one processing owner and participating QoS journals
in one proven storage transaction. The producer reserves journal capacity
before entering the transaction, then commits state, input progress, saved
output and journal appends together.

Waiting for capacity must not hold the transaction gate: consumers may need
that gate to acknowledge and release capacity. Once processing begins, the
lock order is **group transaction gate, then channel state**, including paged
reads/refills. Reversing it introduces a deadlock that ordinary functional
tests may miss.

An uncertain shared transaction fences the group and wakes blocked users with
errors. Healthy producer quiescence is different: subscribers may still drain.
The group is not a transaction across arbitrary queries, providers or remote
systems; separately stored branches still have their separate handoff gap.

Read [`shared_storage.rs`](../src/computation/v1/shared_storage.rs),
[`qos_pipe/shared.rs`](../src/computation/v1/qos_pipe/shared.rs) and
[`core/src/computation/transaction_group.rs`](../../core/src/computation/transaction_group.rs).
[Journal-budget tests](../tests/computation_journal_budgets.rs) include
shared paging and recovery. The time merger currently requires a standalone
transaction owner rather than joining this group.

### Consumer completion closes a different boundary

`DeliveryRunner` remembers exact input identity/content and the completed
operation prefix. If a batch contains A, B and C and B fails, C must not run
and the upstream batch must not be acknowledged. On retry, previously
confirmed operations remain confirmed.

For external handlers, an effect can succeed before its progress commit;
the stable operation ID allows destination-side deduplication. Transactional
handlers instead commit local business state and one operation's completion
together through the borrowed transaction context. Neither mode implies
whole-batch remote atomicity.

The implementation is [`delivery.rs`](../src/computation/v1/delivery.rs),
with ownership protection in
[`delivery/retirement.rs`](../src/computation/v1/delivery/retirement.rs).
It shares output identity rules with journal replay. Do not put this ledger
on ordinary fast sinks that did not request it.

Finally, [`graph/recovery.rs`](../src/computation/v1/graph/recovery.rs),
`assess`, walks the contributing paths to a requested consumer and checks
actual contracts, transport policy, progress ownership and storage survival.
This validates a requested guarantee; it does not activate absent recovery
services. A durable index alone cannot repair an unreplayable upstream source.

## Resource ownership and cancellation-safe cleanup

Data edges describe event flow. Resource dependencies describe which owners
must remain alive for other owners to work. They are related but not the same
graph.

For a journal using a shared storage group, the storage must be created first
and remain alive until the journal and its users are cleaned up. Dependencies
must be declared so the controller can enforce that order. A host-side cache
holding a matching path is not a substitute.

[`graph/resources.rs`](../src/computation/v1/graph/resources.rs) calculates
construction and reverse cleanup order, validates ownership combinations and
includes transitive dependents in changes. Borrowed resources retain their
external owner; they cannot retain a graph-owned prerequisite whose lifetime
the external owner does not control.

Cancellation is the difficult case. Dropping a future requests no magic undo
of a socket bind, blocking write, nested task or external effect. Owners must
retain enough state to finish cleanup and prevent overlapping reuse.

In particular, do not remove a worker's join handle from its owner before an
await that can be cancelled. If the caller disappears at that await, there
must still be an owned handle to join on retry. Use
[`context/workers.rs`](../src/context/workers.rs): ownership is acquired
before spawn, and handles are removed after observed completion. An abort
request is not a completed join. Some I/O/draining workers must not be aborted
on timeout at all.

The same principle appears in graph instance leases, node-task registries,
storage I/O scopes and controller cleanup. A failed cleanup leaves an owned,
retryable obligation, not permission to open a replacement against the same
resources. `GraphRun` drop can mark cleanup required; only awaited async
cleanup can finish it. DrasiLib stop permits supported restart, whereas
shutdown is terminal.

Graceful graph/component stop first quiesces selected producers upstream to
downstream and drains selected edges. Once an input is admitted to a transformer
or sink, its call, continuations, forwarding and required acknowledgement finish
before stop hooks run. Graph-owned source admission similarly finishes a
submitted journal/registration transaction; quiescence cancels only the wait
for another request, then rejects queued requests without claiming acceptance.
Pending activations remain represented in their original start report until
the stop explicitly supersedes them.

A graceful boundary timeout is an explicit failure, not a forced abort or
successful stop. The in-flight operation remains owned; eligible paused members
are resumed, every resume failure retains its component identity, and a later
stop can retry after work completes. `GraphControl::force_cancel_components`
offers explicit selected-domain escalation. It requires every registered user
of the actual shared resource domain and required propagation boundaries;
unfinished resource construction/cleanup or deferred resource metadata rejects
the request before cancellation. `ForceCancellationReport` separates interrupted
work from its subsequently joined cleanup. Completed cleanup is not rollback,
and an interrupted generation requires explicit reconstruction with a changed
specification rather than stop/start reuse.

`GraphControl::cancel()` and dropping a run force the whole graph; they can
interrupt writes, fence owners and require reconstruction. Idle `next`/receive/service waits still have
their cancellation contract, and unfinished construction/start may be cancelled.
No graceful deadline can preempt a plugin blocking the executor thread.

Use [graceful-stop tests](../tests/computation_graceful_stop.rs) for blocked
handlers, real durable continuations and deadline/retry behavior. Native
admission tests also compare graceful drain with forced cancellation on both
sides of the actual journal commit.

Use [runtime lifecycle tests](../tests/computation_runtime_lifecycle.rs),
[resource dependency tests](../tests/computation_resource_dependencies.rs)
and [`core/src/computation/io_scope.rs`](../../core/src/computation/io_scope.rs)
when modifying ownership. Test cancellation *during* cleanup, not only before
cleanup starts.

## Live changes and persisted desired state

### Reconciliation changes an owned topology

A preview computes affected components, bindings and resources at a specific
revision and set of generations/epochs. Reconciliation rechecks that evidence,
quiesces or stops affected work, performs supported replacement/rebinding, and
retains unrelated instances.

The goal is controlled change, not an all-or-nothing deployment transaction.
If cleanup fails, already-stopped components can remain stopped. If a new
factory fails, its declaration/failure remains visible. An in-place update is
valid only when the factory supports it and the component contract permits it.

[`graph/reconcile.rs`](../src/computation/v1/graph/reconcile.rs) owns impact
calculation and reconciliation rules;
[`graph/controller.rs`](../src/computation/v1/graph/controller.rs) executes
the lifecycle work. [Reconciliation tests](../tests/computation_reconciliation.rs)
exercise stale previews, partial outcomes and unaffected components.

Committing the desired topology is the dividing line for errors. Counter,
slot-ownership, prepared ordering and peer-attachment checks finish before the
first live mutation. The commit holds every required slot lease and the actual
peer-topology lock, then mutates without a fallible return. Plugin-backed
transports, handlers and retired objects are released only after that lock is
released. One inspection publication contains the new desired/observed revision.
Precommit rejection preserves the old topology; this does not roll back earlier
draining, stop hooks or external cleanup.

After commit, `execute` always returns
the `ReconciliationReport` with `committed` set; only cancellation returns an
error. Realization, recovery-validation, settle and startup failures are
recorded in `report.failures`. Resumption attempts every eligible paused producer,
aggregates failures and leaves unsuccessful members visibly quiesced/unavailable.
If the graph has not settled, or another lifecycle operation owns the
controller, components marked for automatic start are queued in
`pending_auto_start` and the controller starts them once it is free. A caller
must therefore read the report, not just the `Result`, to learn whether the
change was applied in full.

Disjoint start, stop and drain groups can progress concurrently; a conflicting
stop may supersede a start. Single-component starts and stops from
`ComponentHandle` that conflict with an in-flight group are queued in
`handle_requests` rather than failed. Each component's requests begin in
arrival order, are revalidated against its current generation, and are
dropped if their caller has gone. Any stop that reaches the controller rejects
the same component's queued starts with `StartSuperseded`, so a completed stop
is never undone by an older start. Conflicting group, structural and
recovery-freeze commands still return `OperationInProgress`.

Consumer-progress membership and validated paths are immutable for a live
binding. A change that resets/replaces an owner or alters that required path
must await the producer's actual `stop`, not merely park graph polling:
source-owned I/O may otherwise acknowledge upstream data during the change.
Drain admitted input before replacing the set, retain unchanged actual owners,
initialize enrolled members, then restart with new subscriptions only after
all required boundaries are ready. A source-specific removal rule can be
stronger than graph drain; SQLite requires the departing owner to cover the
current journal head.

The shared-source qualification covers drained reconstruction and reconciliation
with the drain barrier. It does not establish cold reconstruction with undrained
persistent input-pipe frames: handled progress does not reveal every queued
transport allocation. Do not infer those counters, discard frames or reset
source state to hide that missing recovery evidence.

### Durable acceptance precedes realization

Optional management persists *what the instance should contain*. Its
configuration store is separate from query indexes, event journals and
consumer completion state.

```mermaid
flowchart LR
    R[Request plus expected revision and ID] --> V[Validate and protect affected recovery owners]
    V --> C[Commit desired definition and receipt]
    C --> A[Return durable acceptance]
    C --> E[Reconcile resources and components]
    E --> OK[Target realized]
    E --> F[Accepted target with visible failures]
    F -->|fix dependency and retry| E
```

A listener port may be unavailable after the definition is accepted. That is
a realization failure, not evidence that acceptance was rolled back. The
client resolves the same request ID and the runtime retries the retained
target instead of pretending it has an empty configuration.

The management driver in [`management/runtime.rs`](../src/management/runtime.rs)
serializes these requests and owns configuration acceptance/reconciliation.
[`management/store.rs`](../src/management/store.rs) defines the external store
contract. Resource resolution uses supplied resolvers and recipes; the library
does not become a plugin loader or depend on a particular encrypted store.

Persistently managed members reject configuration writes outside that path.
Unmanaged objects and externally supplied resources are not silently absorbed
or removed. A configuration snapshot records reconstructible definitions,
not a snapshot of processing state. Restoring it cannot rewind a database,
restore pruned events or undo an external effect.

### Removing durable state needs more than a stopped status

Replacing a persistent resource can destroy the only remaining path to
unconfirmed work. Before accepting such a change, supported retirement paths
stop the actual users, verify pending obligations under the real storage
gates, and hold those owners against restart/rebinding through the decision.

The race to prevent is: inspect "empty," release the lock, accept new work,
then replace the resource based on the stale empty observation. Retirement
holds the proof stable instead.

If configuration commit is uncertain, those protections remain held until
authoritative resolution. Confirmed rejection can resume old owners; accepted
retirement requires the affected generations to be reconstructed/removed.
Dropping an unresolved lease does not automatically resume them.

Query deprovisioning starts a new output incarnation, not a reused one. It
records an in-progress reset before clearing anything, so an interrupted wipe
is refused at recovery rather than resumed from partial state. The restarted
sequence numbers are published under a strictly newer output generation, so a
consumer that deduplicates on identity, generation and sequence cannot mistake
new results for old ones. Recovery treats only a readable, absent or different
configuration hash as a configuration change: a storage error while reading it
fails recovery instead of authorizing an automatic reset. See
[`query_recovery.rs`](../src/computation/v1/query_recovery.rs) and `recover` in
[`query.rs`](../src/computation/v1/query.rs).

Explicit data-loss authorization is revision-bound and names a complete
recovery domain. It permits removal, not in-place reuse, data deletion or
fabricated handling acknowledgements. It cannot excuse incomplete cleanup.

Read [`management/transitions.rs`](../src/management/transitions.rs) with
[`graph/retirement.rs`](../src/computation/v1/graph/retirement.rs) and the
relevant storage/delivery retirement owner.
[Managed-resource integration tests](../../lib-integration-tests/tests/computation_managed_resources.rs)
and [management tests](../../lib-integration-tests/tests/computation_management.rs)
cover failures that a successful configuration round trip would miss.

## Plugin hosting without a second executor

Ordinary Source/Reaction/Bootstrap plugins and native ComputationGraph plugins
are different, independently versioned families. The former enter through
graph-owned adapters preserving their existing interfaces. The latter expose
native sources, transformers, sinks and services directly.

Native hosting transports operations as opaque handles with poll, wake,
cancel and release behavior. The host drives those operations under the same
graph lifecycle rules. The SDK supplies an owned I/O runtime for timer/socket
drivers; it does not run another graph executor or spawn processing futures.

An ordinary source change event the host cannot decode ends that change stream
with `UndecodableSourceEvent` (see
[`proxies/change_receiver.rs`](../../components/host-sdk/src/proxies/change_receiver.rs)).
Skipping it would let the source confirm later positions past the lost change,
so a restart could never replay it. The source fails instead and resumes from
its last confirmed position.

Rust futures, trait objects, `Arc`s, allocator ownership and runtime internals
do not cross the C ABI. Producer-owned buffers are released by their producer.
Native envelope decoding reconstructs validated owned objects; a temporary
borrow of wire bytes must not escape its buffer owner. Libraries remain pinned
for process lifetime. These are trusted in-process plugins, not sandboxed
workers or hot-unloadable code.

Optional transaction, source admission, bootstrap/progress and consumer services
are mediated by host-owned, revocable capabilities. For example, a transactional
plugin step sends storage requests through a mailbox serviced within the real
borrowed transaction. The host does not extend a borrowed Rust context to an
invented static lifetime. Late callbacks after cancellation must fail rather
than reach a replacement's storage.

The separate `drasi_computation_plugin_consumer_progress_v1` symbol adds
producer consumer-progress inspection and an optional retained position
comparator. Base ABI `1.0.0`, wire `2` and recovery-v1 are unchanged.
Opt-in source/transformer factories require one `consumer_progress` Checkpoint
dependency containing the actual `Arc<ConsumerProgressSet>`; it cannot coexist
with single-owner progress or admission on that producer.

`NativeConsumerProgress` exposes immutable member IDs and read-only observations,
not storage writers. Constructors retain it without reading or subscribing
before start. Bounds include 256 members, 4,096 checkpoint/transport entries,
a 1-MiB encoded progress frame and 16 retained subscriptions. Failed encoding
must not consume an observation; a cancelled wait must not consume a publication.
Stop revokes old readers, and successful cleanup precedes new subscriptions.

The source returns one stable immutable position-policy handle. Comparisons
can run outside the component's mutable-operation lock, including while an I/O
operation is pending. Both arguments use the source's checkpointable position
format, bounded by 65,536 bytes. Preserve comparator errors; a panic quarantines
the native instance and wakes waiters. Queued input can retain the comparator
after component removal, so the producer-owned handle and library pin must
outlive every such use.

The optional `input_progress` slot has a different direction: it binds an
eligible component's own upstream-facing handled-input owner. The Host attaches
the volatile contract, but the handle never enters the plugin and direct proxy
calls never publish it. A transformer can have both its own input owner and a
set of downstream owners. Reject conversion of an actually tracked instance
into a transactional participant.

Managed `ConsumerProgressConfig` resolves
`{kind: "consumerProgress", producer: <ID>, consumers: {<component>: <resource>}}`.
Declare exactly one distinct Checkpoint dependency per consumer, using its
actual `sourceProgress` resource. The producer binds the set to
`consumer_progress`; a query binds its member to `source_progress`, while an
eligible standalone transformer/Handled sink binds its member to `input_progress`.
The [Host SDK recipe guide](../../components/host-sdk/README.md#native-producer-consumer-progress)
also distinguishes canonical secret references from literal Server graph config.

For scheduling and lifetime changes, start with the
[native SDK design](../../components/computation-plugin-sdk/README.md), then
[`host-sdk/computation/proxy.rs`](../../components/host-sdk/src/computation/proxy.rs)
and [`factory.rs`](../../components/host-sdk/src/computation/factory.rs).
For capability changes, follow
[`transaction.rs`](../../components/host-sdk/src/computation/transaction.rs),
[`admission.rs`](../../components/host-sdk/src/computation/admission.rs),
[`bootstrap.rs`](../../components/host-sdk/src/computation/bootstrap.rs) or
[`consumer.rs`](../../components/host-sdk/src/computation/consumer.rs).
Shared producer capabilities live in
[`consumer_progress.rs`](../../components/host-sdk/src/computation/consumer_progress.rs)
and [`input_position.rs`](../../components/host-sdk/src/computation/input_position.rs);
data-only contracts remain in
[`drasi-computation-contracts`](../../components/computation-contracts/src/consumer_progress.rs).
Keep the [native ABI](../../components/computation-plugin-abi/src/lib.rs) and
ordinary SDK wire contracts separate.

A trait implemented by an in-process Rust component does not automatically
exist across FFI. Test separately built libraries and actual capability
negotiation, not just Rust mocks in one binary.

### Current source adoption and consumer boundaries

| Surface | Implemented scope and limit |
|---|---|
| [PostgreSQL shared rows](../../components/computation-plugins/sources/postgres/README.md#shared-ordinary-row-queries) | Independent memory/persistent owners, row-position filtering and per-query configured-slot snapshots; memory mode makes no process-restart promise |
| [PostgreSQL shared transactions](../../components/computation-plugins/sources/postgres/README.md#shared-complete-transactions) | Whole committed inputs for actual atomic persistent transaction-enabled queries; distinct source/bootstrap identities and whole-commit positions |
| [MySQL](../../components/sources/mysql/README.md#shared-consumer-recovery) | One native Rust live worker, persistent consumer sets and per-query snapshots; finite unregistered history readers avoid live replication-ID collisions |
| [SQLite](../../components/sources/sqlite/README.md#shared-native-recovery) | Existing transaction journal, oldest-member replay/pruning and serialized snapshots; bounded enrollment metadata includes retired identities |
| [WAL replay](../src/computation/v1/wal_source.rs) | Actual shared-owner resume and typed factory binding; no new WAL pruning/deletion policy |

PostgreSQL's shared bootstrap providers use temporary export/import resources,
not ownership of the configured live slot. Never reset/drop/recreate that slot
to enroll a member. Old single-owner, row and whole-transaction positions remain
distinct; a new factory identity is not permission to reinterpret stored state.
Strict interrupted persistent bootstrap remains fenced unless an explicit
recovery policy permits the supported reset.

MySQL snapshot timestamps use a server clock sampled before acquisition;
live events use precise immediate commit timestamps where required. Regressing
to coarse statement seconds can silently reject valid post-snapshot updates.
SQLite cannot prune a lagging member's history or remove its enrollment merely
because a reader disappeared.

Server's canonical definitions now cover seven PostgreSQL configurations and
Trading's actual 11-query group with two shared PG sources, one HTTP source and
composed initialization. Generation uses actual factory metadata and the app's
query declarations; secret `ConfigurationValue` references remain references.
Provisioning installs the complete fixed-member bundle in one existing batch,
then its SSE reaction; partial/conflicting bundles fail explicitly. See the
[Server recipes and qualification tools](https://github.com/drasi-project/drasi-server/tree/15dafa225ac76ba9868452a7082f693027621ad7/examples/postgres).

This does not close native schema discovery, MySQL/SQLite dynamic **source**
packaging and Server registration, cold undrained-input reconstruction, or the
rest of cleanup stage 6 and stages 7 through 9. HTTP snapshots/live prices have
no added durable or atomic replay guarantee.

## Inspection and diagnosing a running system

The controller publishes desired topology and observed state coherently.
[`graph/registry.rs`](../src/computation/v1/graph/registry.rs) adds weak
in-process bindings used by ordinary facades and native query read APIs.
Weak bindings prevent inspection from extending storage/component lifetime
or keeping a replaced generation alive.

[`inspection.rs`](../src/computation/v1/inspection.rs) provides read-only
snapshots, coalesced observations and bounded history. The compatibility
translator in [`runtime/events.rs`](../src/computation/runtime/events.rs)
derives old-style events synchronously from publication changes; it does not
maintain a second lifecycle state machine.

Query discovery is based on semantic role and captured read capabilities,
not a hard-coded factory name. Inspect
[`runtime/native_query.rs`](../src/computation/runtime/native_query.rs) and
[`query_api.rs`](../src/computation/v1/query_api.rs) when a native query is
missing from ordinary query APIs. Result reads do not create another evaluator;
live ordinary subscriptions require the actual outlet/catalog connection.

Public errors preserve internal causes through
[`error.rs`](../src/error.rs), rather than reducing them to formatted strings.
Use the public classification for broad error categories and the typed cause
for specific failures. Cleanup can fail in several owners: inspect the retained
aggregate or lifecycle report, not only its primary source chain. Otherwise a
second resource still requiring cleanup can disappear from the diagnosis.

An instance inventory comes from one coherent root publication. Separate DrasiLib
instances or child processes still do not form one global transactional snapshot.
A resource link shows a binding/ownership relationship,
not proof that every event used that resource. Private plugin resources are
visible only when reported. Topology-as-data excludes configuration values,
secrets and failure text; privileged configuration exports have different
exposure rules.

Use failure phase, generation and the relevant progress boundary together:

| Symptom | First useful investigation |
|---|---|
| Node exists but never starts | Creation failure, missing binding/resource, or unmet readiness in the controller; not just the instance's running flag |
| Query reads work but live subscription is empty | Outlet/catalog wiring and output generation, then subscriber progress |
| A replayed event is skipped or applied twice | Immediate producer/query identity, logical versus transport sequence, receipt retention and handoff confirmation |
| Temporal result disappears after restart | Committed future queue, bootstrap state and due-work transaction; not only the timer notification |
| A slow consumer causes apparent upstream inactivity | Pipe capacity, subscriber cursors and pending output; distinguish backpressure from a dead worker |
| Shared CDC will not start or acknowledge further history | Every actual member's readiness, failure, reset generation and source cursor; then owner/path validation and source cleanup |
| A mixed bootstrap will not restart | Each child's preparation outcome, same-query owner binding, namespaced state and retained cleanup; do not silently reset persistent state |
| Replacement cannot acquire storage or a port | Previous generation's cleanup/retirement owner and retained worker joins |

Logs and metrics complement these publications. The relevant paths are
[`observability/tracing_layer.rs`](../src/observability/tracing_layer.rs),
[`metrics/`](../src/metrics) and
[`pipe_metrics.rs`](../src/computation/v1/pipe_metrics.rs).
Profiling intervals can include queueing, waits and storage, so do not interpret
every elapsed interval as evaluator CPU time. Bounded history/watch channels
are diagnostic tools, not a lossless audit ledger.
Logging and event-history implementations live in `observability`. The old
`managers` forwarding module has been removed; use the supported observability
imports directly.

## Making and validating a change

### Keep the change in its owning layer

A queue-ordering fix belongs in the queue/scheduling path, a matching fix in
the shared evaluator, a snapshot-to-live handover fix in bootstrap/recovery,
and a replacement race in the controller and actual cleanup owner. Follow
the data once across adapters before deciding the engine lost it.

When extending a native component, first decide which state it owns, what
successful processing proves, and whether a cancelled call can leave an
obligation. Then decide how that state is reconstructed. A new method or
capability flag is not the design by itself.

The essential review questions are:

- Can a component, binding or callback from an old generation act on the new one?
- If cancellation happens at each await, who still owns cleanup and pending work?
- Does every acknowledgement follow the boundary it claims?
- Do retries retain logical identity and reject conflicting or expired content?
- Are configuration intent, processing state and external effects kept distinct?
- Does the ordinary memory-only path acquire work or allocations for an unused feature?
- Does a shared producer cover every actual required owner without merging their cursors or assuming unrelated counters are comparable?
- Can membership, reset or bootstrap retirement race source-owned I/O after graph polling is paused?

### Measure the path actually changed

The fast path should not allocate durable receipt ledgers, shared journal
reservations or management machinery unless selected. Large opt-in async
branches can enlarge every future even when disabled; boxing just those
branches avoids charging all ordinary processing for them.

Use [`fast_path.rs`](../examples/fast_path.rs) for ordinary in-process work,
[`core_workload.rs`](../examples/core_workload.rs) for projection/aggregation/join
workloads, and the
[native Host benchmark](../../components/host-sdk/examples/native_fast_path.rs)
for the dynamic-plugin boundary. The
[interleaved measurement driver](../tests/measure-fast-path.py) helps separate
real regressions from run-to-run noise. Compare the same dependency features,
storage, queue bounds, payloads and runtime flavor; a debug test duration is
not a production throughput claim.

### Test the boundary, then the adjacent paths

Run focused tests for the subsystem first. Scheduling/lifecycle changes need
current-thread and multithreaded coverage. Recovery changes need reopen with
real storage and failures before mutation, after mutation, before commit,
after commit and during cleanup. Cancellation after commit but before its
acknowledgement is a different case from rollback before commit.

For example, from the repository root:

```bash
# Routing, independent execution, lifecycle and control ownership.
cargo test --locked -p drasi-lib \
  --test computation_graph_dataflow --test computation_runtime_lifecycle \
  --test computation_controller --test computation_task_execution

# Actual shared owners, normal/batched handling, and composed initialization.
cargo test --locked -p drasi-lib \
  --test computation_input_progress --test computation_queries \
  --test computation_wal_source
cargo test --locked -p drasi-lib --lib computation::v1::composite_bootstrap::tests

# Real RocksDB query reset/deprovision and merger child-crash reconstruction.
cargo test --locked -p drasi-lib --no-default-features \
  --features computation-rocksdb-tests \
  --test computation_query_recovery --test computation_time_merge \
  -- --test-threads=1
```

These commands are examples of targeted selection, not the complete CI gate.
Tests involving source/reaction crates that depend on drasi-lib belong in
[`lib-integration-tests`](../../lib-integration-tests), avoiding dependency
cycles. Backend injection can be tested from `lib` with its existing backend
test features.

For native progress/position interfaces, include the
[Host consumer-progress suite](../../components/host-sdk/src/computation/consumer_progress_tests.rs)
with freshly built independent libraries. For composition, include the
[Host recipe tests](../../components/host-sdk/src/management/composite_bootstrap_tests.rs)
and [real PG/HTTP test](../../components/host-sdk/tests/native_postgres_http_bootstrap.rs)
on both Tokio flavors. The latter exercises static and rebuilt dynamic providers,
a held HTTP snapshot with concurrent PG writes and a feedback fence, live input,
rollback exclusion, restart, and actual retirement hold/resume. Follow its
database/plugin prerequisites; a Rust-only mock is not equivalent evidence.

Use the connector-specific suites for shared source behavior: PostgreSQL
[row, snapshot and transaction cases](../../components/sources/postgres/tests/native_transactions),
MySQL [shared recovery](../../components/sources/mysql/src/native/tests/shared.rs),
and SQLite [consumer enrollment](../../components/sources/sqlite/tests/native/consumers.rs).
Check unequal snapshot positions and unrelated counters, the slow member's
retention floor, exact transaction effects, failure/cancellation, and explicit
replacement. Passing row fan-out does not qualify whole-transaction fan-out.

Before a cross-cutting change is considered qualified, consult
[`run-runtime-parity.sh`](../tests/run-runtime-parity.sh) and its
[contract inventory](../tests/runtime_parity/computation-contracts.tsv).
They cover default/no-default builds, additional capabilities, separately built
plugins and backend profiles. An ignored process-crash worker does not count
as coverage without its passing parent crash driver. Keep assertions about
ordering, exact output and retained obligations; "the graph did not panic" is
not a sufficient oracle.

The query and merger parent drivers run the same normal DrasiLib in a separate
process, check that its live RocksDB cannot be opened by another owner, externally
SIGKILL it at selected persistence boundaries, and verify actual reopen. Query
cases cover reset/deprovision intent and partial wipes, strict refusal of
incomplete state, and explicit retry with exact rows/high-water/generation.
Merger cases cover admission, emission, journal handoff and confirmation with
exact buffered/output/receipt recovery. These selected cases passed on macOS
and an init-enabled Linux container; they are not every provider's wipe or
production fault matrix.

For the actual mutual-TLS macOS/Linux-VM profile, use the
[prepared-peer instructions](../../components/instance-host/README.md#qualified-unix-platforms-and-prepared-network-peer).
Do not count an unprepared ignored network case as acceptance or substitute an
injected loopback error for the required real receipt fault.

Use [the qualification ledger](../tests/runtime_parity/requirements.tsv) and
[current limitations](componentgraph-vs-computationgraph.md#performance-and-qualification-boundaries)
when making release claims. Framework support, one backend test and
production-duration operational evidence are different levels of confidence.
