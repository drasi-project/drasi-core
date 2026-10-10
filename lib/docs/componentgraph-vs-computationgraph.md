# ComputationGraph: current functionality

**Snapshot: 9 October 2026**

This document describes the implementation on
`agentofreality-parallel-computation-graph` in the local drasi-core workspace,
including its matching Server and test-framework integration. It is a development
branch snapshot, not a statement about published packages or universal production
readiness.

## Summary

**ComputationGraph is the sole runtime for DrasiLib, Drasi Server and embedded
test-framework instances.** It owns component creation, execution, connections,
resources, inspection and cleanup. It supports ordinary source/query/reaction
pipelines and native graph components, including pipelines with no query.

The implemented feature set includes transactional continuous queries, durable
middleware, linear transaction containers, retained and multicast delivery,
opt-in recovery-path validation, native plugin recovery services, and persisted
desired-state management. Resource controls include opt-in full-envelope byte
quotas, paged persistent history, bounded configuration receipts, snapshot cleanup
and explicit encryption-key rotation. These are selectable capabilities, not
guarantees automatically granted to every graph.

**Acceptance, committed processing, durable publication and completed external
effects are separate boundaries.** A deployment must select compatible sources,
components, pipes and storage for the boundary it needs.

## Runtime and component model

Each DrasiLib instance owns one root ComputationGraph. Component batches join that
graph and share its component namespace. The public `ComponentGraph` inspection
facade is read-only; it does not own a separate registry or execution engine.
ComputationGraph is available with default features disabled. The empty
`computation` Cargo feature is a build-name alias, not a runtime selector.

| Surface | Current behaviour |
|---|---|
| Ordinary APIs | Source, Query and Reaction APIs use graph-owned adapters and the shared query evaluator. |
| Native components | Sources, transformers, sinks and portless services implement graph contracts directly. Built-in queries are transformer-shaped components. |
| Construction | Callers can supply objects or factory specifications with implementation identity, configuration, ports and resource dependencies. |
| Validation | Strict graph construction checks identities, cycles, schemas, ports, required connections and declared capabilities before activation. Incremental additions can retain incomplete declarations. |
| Readiness | Acceptance, successful construction and readiness are distinct. Handles can wait for creation or startup and expose failures. |
| Observation | Creation state, execution state and health are independent; a running instance does not imply that every component is ready. |
| Control | Bounded readiness and neighbour-control mailboxes are separate from data queues. Generation checks invalidate stale handles and bindings. |
| Ownership | Components and graph-owned resources have explicit asynchronous cleanup; borrowed resources retain their external owner. |

Accepted additions remain inspectable when initialization or activation fails.
An addition can still be rejected for invalid ownership, duplicate IDs or a closed
instance. Standard `add_source` and `add_reaction` receive preconstructed objects,
so a caller-side constructor failure cannot appear as a graph node. Factory-backed
declarations support construction within the managed lifecycle.

Ordinary DrasiLib queries own nested QueryGraphs with independent, owned Tokio
tasks. They can execute on different workers in a multithreaded runtime; a
current-thread runtime is also supported. Native nodes within one graph are
cooperatively polled by its controller, not automatically assigned a task or
thread per node. Per-node work budgets bound continuously ready work and allow
other nodes and control operations to progress.

## Data contracts, ordering and connections

Native data boundaries carry `ChangeEnvelope`: immutable, schema-validated change
records plus provenance, lineage and branch-local annotations. Adds, updates and
deletes preserve operation order and distinguish full, partial and patch images.
Fan-out shares immutable payloads. Exact schema descriptors and executable
validators govern connections and decoding; a matching schema name alone is
insufficient.

An ordinary query has one bounded ranked inbox shared by its configured sources.
Each producer's earlier admitted logical sequence remains ahead of its later
work, even when timestamps move backwards. Available producer heads are compared
by source event timestamp, query-local source order, then source sequence.
Scheduled notifications share that queue but have a distinct kind and rank.
This orders available work, not unseen events, and does not wait for every source
to advance.

The source's dispatch mode controls admission to the query inbox: Channel
backpressures; Broadcast can drop arrivals when full. The query's dispatch mode
controls its outgoing results. Source order is part of the query's configuration
identity and cannot change invisibly underneath saved recovery state.

| Connection | Behaviour and boundary |
|---|---|
| Bounded | Volatile FIFO with backpressure. |
| Byte-bounded | Opt-in volatile FIFO limited by count and full serialized envelope bytes; one oversized envelope can occupy an otherwise empty pipe exclusively. |
| Broadcast | Volatile bounded history with explicit lag handling. |
| Ranked | Shared bounded query input with source-aware ordering and blocking or drop-newest admission. |
| Retained | Single-consumer history with handling acknowledgement; memory or persistent storage. |
| QoS multicast | One append to a shared channel, independent subscriber cursors, blocking or explicitly lossy retention, and optional persistent replay. |

Acceptance into a queue is not completed handling. Acknowledgement-required
connections reject acceptance-only sinks. Retained/QoS lossless capacity remains
occupied until required consumers complete; a temporary disconnect does not
retire a subscriber's obligation. Membership changes or explicit retirement do.
Each acknowledged subscriber has one outstanding delivery. Neither queue
capacity nor backpressure provides a configured events-per-second or burst-rate
limit; there is no built-in rate-limiting pipe.

Lossy QoS requires an explicit gap policy. `SkipWithNotification` logs the exact
half-open skipped range only after cursor advancement succeeds; cancelled or
failed advancement cannot report a successful skip. The log is diagnostic, not
a durable audit-delivery guarantee.

Fan-out to independent destinations is not atomic. Some branches can accept
before another fails, and a slow required branch can backpressure the producer.
Count-only bounds remain the default. `ByteBoundedPipeConfig { capacity, max_bytes }`
also charges payloads, identifiers, metadata, annotations and lineage using their
complete binary representation. Capacity is released on receipt, not handling.
Separate fan-out pipes each charge the complete envelope. The oversized singleton
exception and uncounted pending/downstream work mean this is not a process-memory
limit. Retained stores and non-shared QoS journals also support opt-in binary-byte
quotas, charged once per journal rather than once per subscriber. Reclaiming
lossless history requires actual handling progress from every required consumer;
a rejected append cannot partially prune history. An oversized record can be
accepted only after the prior window can be removed safely. A reduced quota on
reopen preserves existing obligations rather than deleting them.

Persistent retained and QoS owners can opt into record/serialized-byte
bounded read pages and a bounded payload cache. Startup still validates every
record, including admission/output-replay progress and receipt consistency.
This bounds cached payloads, not startup scan time or all metadata: optional
journal-byte accounting keeps one size per retained record. Shared transactional
QoS preserves a group-owned view across startup pages and refills, but still uses
count-only admission; ranked and broadcast
pipes remain count-bounded. Byte/page policies are resource-owner construction
options, not automatically saved Host/Server recipes.

## Continuous queries and transactional processing

Cypher and GQL queries use the shared core evaluator for matching, joins,
projection, aggregation, middleware and change detection. Queries maintain current
result rows and produce result changes; graph composition does not introduce a
second evaluator.

`ContinuousQueryFactory` constructs a `TransactionTransformer` query body. With
a complete atomic storage provider, indexes, source checkpoints, logical result
sequence, materialized rows and retained output commit together. Forwarding
confirmation is separate: a crash after commit can replay saved output without
reevaluating the input. Logical result identity stays stable while replay
transport sequences advance.

`QueryScheduledSource` observes committed due work and emits notifications without
deleting it. The query transaction removes actual due work, evaluates it and
records resulting output together. A lost or duplicate notification does not by
itself lose the persisted scheduled work. Future-deadline sleeps are capped at
five seconds before rechecking wall-clock time; due notifications preserve the
scheduled timestamp and use a separate increasing transport sequence. This is
not a real-time execution deadline or a general clock-injection API.

Default memory queries use direct in-memory indexes and a no-op session
controller. Persistence requires an explicitly suitable provider; persistent
indexes alone do not establish atomic output or replay.

Two further processing arrangements are supported:

- **Standalone middleware:** `MiddlewareTransformer` reuses the middleware
  registry outside a query. Durable mode commits previous-element state, input
  progress and pending output together. It replays unconfirmed output without
  executing stateful middleware twice and requires compatible durable outputs.
- **Linear transactions:** `TransactionTransformer` runs opted-in
  `TransactionalTransformer` steps in order, with isolated step state and no
  internal queues. State, input progress and final output share one commit.
  Ordinary transformers are not automatically participants; sources, external
  effects and nested transaction containers are excluded. A query uses its
  dedicated body, not the linear-step interface.

### Complete source transactions

`SourceTransactionBuilder` and the complete-transaction schema can preserve a
committed upstream transaction as one bounded input. A query explicitly enables
`QueryExecutionSettings::source_transactions` and requires atomic publication.
It evaluates the group under one transaction and exposes the final result changes,
not intermediate row-by-row results.

Change-count, byte and duration limits are explicit. An incomplete or rejected
assembly cannot publish a successful prefix. Native Rust PostgreSQL, MySQL and
SQLite implementations provide transaction-aware paths and coordinated snapshot
options; their supported capture, retention and database features differ.
SQLite replay captures writes through its owned native handle, not arbitrary
external writers.

This boundary is one query's processing of one source transaction. It does not
make all downstream queries, consumers or remote effects visible atomically.
General production packaging of these database implementations as native dynamic
plugins is not supplied merely by the framework interfaces.

## Recovery and durable delivery

### Requested guarantees are checked against actual bindings

`RecoveryRequirement`, `ComputationGraphBuilder::require_recovery` and
`DesiredTopology::recovery_requirements` express opt-in requirements for a selected
consumer boundary. Assessment includes its contributing upstream paths, ordinary
subscriptions, component contracts, pipe policies and actual storage survival.
It distinguishes in-memory lifetime from declared failure scopes.

Checks cover acceptance, replay, committed processing and publication. Unknown
capabilities, lossy links or mismatched progress owners cannot satisfy a stronger
claim. Actual transaction/progress ownership matters, not matching resource names
or paths. Requirements are checked across construction, activation and relevant
live changes; they do not enable missing services or confer exactly-once external
effects.

### Durable admission, replay and shared storage

`SourceAdmission` lets a source publish through the graph's bounded mailbox into
its actual outgoing QoS channel. Producer sessions and consecutive client
sequences identify requests. Matching retries return retained receipts;
conflicting or expired retries fail. Acknowledgement follows the configured
storage commit, not entry into a plugin's volatile queue.

Output-replay receipts instead preserve a persistent component's logical output
identity. They prevent duplicate journal appends within a bounded receipt window.
Admission and output replay are different, mutually exclusive channel modes.
Uncertain commits are reported as uncertain and fence the affected owner until
cleanup and reconstruction; clients must resolve or retry the same identity.

Built-in persistent queries, durable middleware and linear transaction containers
can persist their required output destinations. Pending output cannot silently
lose a port, subscriber or journal binding. Under the same destination identity,
replacement configuration applies to pending work. Confirmation follows acceptance
by all required branches, not completion of their external effects.

`SharedStorageGroup` can place one built-in processing owner and participating
QoS journals in one proven transaction domain. Producer state, input progress,
retained output and journal appends then commit together. Capacity is reserved
before processing; subscriber handling remains separate. The group supports one
processor and at most 256 active journals, not transactions spanning arbitrary
queries, storage providers or external systems.

### Consumer completion and recovery policies

`DeliveryRunner` records exact batch identity/content and advances the completed
operation prefix. Failed handling leaves subsequent operations pending; retained
input is acknowledged only after the whole batch completes. Stable operation keys
survive replay and replacement under the same consumer identity.

External handlers must report actual completion and may need destination-side
deduplication: an effect can succeed before its local progress commit. Transactional
handlers use a borrowed `TransactionContext` to commit business state and one
operation's completion together. That is per-operation atomicity, not a transaction
around the whole batch or a remote side effect.

A PostgreSQL delivery handler supports effect-and-cursor commits in the destination
database. A separate HTTP completion service validates and confirms the exact
batch identity. Neither automatically upgrades ordinary HTTP/gRPC sinks or other
acceptance-only plugins.

Query recovery provides Strict refusal or AutoReset with supported bootstrap.
Reaction recovery additionally permits explicit AutoSkipGap, including a changed
query identity; this can leave external state incomplete. Source/query identity,
generation and sequence checks prevent reuse of unrelated checkpoints. A missing
checkpoint for one source does not authorize clearing other sources' committed
state. Corrupt state follows supported recovery policy; storage read outages do
not authorize treating it as empty.

Coordinated bootstrap streams initial rows and commits completion with source
watermarks. Native providers can retain bounded initialization intent in the
query's actual storage domain. Partial loading remains incomplete; dropping a
stream requests cancellation, and query cleanup awaits the provider's stop hook.
Initial snapshot rows are not represented as one fictitious source transaction.

## Plugin integration

The host supports two independently versioned plugin families in the same runtime:

| Family | Contract |
|---|---|
| Source/Reaction/Bootstrap | SDK ABI `0.17.0`; graph-owned adapters preserve these interfaces and their declared capabilities. |
| Native ComputationGraph | ABI `1.0.0`, wire version `2`; direct source, transformer, sink and service factories using binary change envelopes. |

Native metadata, schemas, role, capabilities and ABI layout are validated before
use. Host-controlled poll/wake/cancel operations own scheduling; plugin buffers
are released by their producer. Libraries remain loaded for the process lifetime.
Native plugins are trusted in-process code, not isolated workers.

The native SDK and Host implement optional transaction participation,
graph-owned source admission, read-only committed source progress, query-owned
bootstrap and host-owned consumer completion. Consumer completion supports
external and transactional modes through the same `DeliveryRunner`. Revocable
capabilities preserve actual ownership across cancellation and replacement;
Rust storage handles do not cross the ABI.

Concrete native packages include standard counter/middleware/arithmetic/capture
components and HTTP/gRPC sources and sinks plus an SSE sink. Native HTTP/gRPC
durable admission uses separate session/receipt protocols and an explicitly bound
QoS service. Fast-mode ingestion acknowledges volatile admission.

Native SSE consumes query envelopes directly. It is volatile and acceptance-only:
disconnected browsers receive no retained history, and lag closes the stream so
clients can reconnect and fetch a snapshot. Browser delivery is not a durable
consumer-completion boundary.

Framework bootstrap/progress/completion contracts have separately built native
test-library coverage. This does not mean every production plugin implements
them. Dynamic query-engine hosting, arbitrary resource injection, native in-place
reconfiguration and hot unloading are not supported.

## Configuration, live changes and cleanup

`GraphControl::preview` and `reconcile` support revision-checked additions,
replacement, connection changes, resource changes, restart and retry. In-place
updates require a supporting factory. Component and binding generations prevent
old handles from controlling replacements. Removal policies can reject dependent
breakage, cascade, retain permitted unresolved relationships or drain a supported
completed-handling boundary.

Resource dependencies order creation before use and cleanup after dependents.
Failed or cancelled cleanup retains the owner and its prerequisites for retry.
Await shutdown before releasing storage or reconstructing owners. DrasiLib
`stop()` permits a subsequent `start()`; `shutdown()` is terminal. Dropping a Rust
handle cannot complete asynchronous cleanup.

Optional `ManagementOptions` adds registered factories, a resource resolver and
an optional `ConfigurationStore`. Configuration persistence is off by default in
DrasiLib. With a durable store, `apply_desired_state` commits the desired definition
and idempotency receipt before construction or lifecycle effects. Expected
revisions prevent lost updates; the same request ID and content resolve to the
same receipt. Accepted but failed components remain declared and inspectable.
Resource resolution has a 30-second deadline; custom resolvers must release
partial acquisitions safely if cancelled. A deadline, missing secret or occupied
port does not roll back an already accepted definition. Fix the dependency and
reconcile the retained target rather than resubmitting unrelated state.

Persisted managed instances restore accepted definitions on reconstruction and
require configuration changes through the desired-state API. Missing factories
or providers do not turn the definition into an empty graph. Supplied opaque
objects are not automatically restorable. Configuration snapshots preserve
factory definitions and explicit provider recipes; named snapshots and restoration
are available through the Rust management API.
Missing or incompatible implementations remain visible as restoration failures;
damaged records and unsupported persisted versions fail explicitly, not as empty
configuration. Cancelling restoration or a storage-call waiter does not release
the configuration lease while reconciliation, storage I/O or cleanup still owns it.

The external redb configuration store encrypts definitions, receipts and snapshots
using a host-supplied key and supports isolated instance namespaces. It is separate
from indexes, event journals and consumer progress. Restoring a configuration
snapshot does not rewind processing state or external effects. Opt-in bounded
receipt batches use generated IDs and explicitly reject expired retries; default
stores retain arbitrary IDs indefinitely. Bounded snapshot listing and
revision-conditional deletion support manual cleanup. Atomic live-record key
rotation requires closed configuration sessions and externally retained old/new
keys; it is not secure erasure of old pages or backups.

Persistent recovery-domain changes are guarded before acceptance. Supported
lossless retirement requires successful component stop/cleanup and verified drain
under actual ownership gates. Unknown acceptance retains those gates until
authoritative resolution. Explicit, revision-bound loss permission allows removal
of complete named domains, not partial removal or in-place reset/reuse. It does
not delete data, mark work handled or undo external effects.

**A durable configuration commit is not an all-or-nothing deployment.** Creation,
startup or cleanup can leave a partially realized target with visible failures.
There is no automatic processing-state migration.

## Server, inspection and operational surfaces

Server supports native topology declarations, resource recipes and ordinary
components in one instance. Recipes include query indexes/catalogs, middleware,
QoS, shared storage, source progress, native bootstrap and native consumer
resources. Built-in native queries participate in ordinary query inspection,
result and supported lifecycle APIs. A connected query-results outlet/catalog
provides ordinary reaction and attach-stream integration; direct native edges
do not need that adapter path.

Under `/api/v1/instances/{instanceId}/computation`, Server exposes:

| Surface | Available operations |
|---|---|
| Inspection and export | `GET /`, `GET /configuration` |
| Imperative components | `POST /components`, `DELETE /components`, `POST /start`, `POST /stop` |
| Managed configuration | `GET /desired`, `PUT /desired`, `GET /receipts/{requestId}`, `GET /management`, `POST /reconcile` |

Server's opt-in `configurationStore` uses encrypted redb and a provisioned key
file. YAML seeds the definition only for an uninitialized instance; the accepted
stored definition is authoritative thereafter. Managed mutations return a durable
acceptance receipt before readiness. Imperative configuration changes are rejected
for persistently managed instances. Server does not expose every low-level Rust
graph operation or the complete named-snapshot management API.

Ordinary configuration-file persistence and cloning preserve reconstructible
native declarations and recipes. Clone disables auto-start and does not copy
external processing storage. Opaque bindings without reconstruction recipes are
rejected. Solution templates select ordinary source/query/reaction components.

Inspection includes the root graph, nested query graphs, component states,
connections, reported resources and exact plugin references. Resource dependency
links describe ownership/bindings, not proof of per-event usage. Private plugin
resources require explicit reporting. Each graph publication is coherent, but
combined observations of running graphs are not one global transaction.

Topology-as-data excludes configuration values, resolved secrets and failure
messages. Full configuration exports are privileged and can contain secrets;
Server's `no-store` responses are not access control. Logs, metrics, bounded
inspection history and profiling support diagnosis. Profiling intervals include
waiting and are not pure CPU measurements.

## Performance and qualification boundaries

Memory-only execution does not require durable admission, delivery ledgers,
shared-storage journals or configuration persistence. Those services are opt-in.
Independent ordinary queries can use multiple Tokio workers; native nodes in one
graph do not imply independent CPU parallelism. Serialization, adapters, storage,
network delivery and slow consumers remain workload-dependent costs.

The repository includes an [ordinary fast-path benchmark](../examples/fast_path.rs),
a [native Host benchmark](../../components/host-sdk/examples/native_fast_path.rs)
and a [measurement driver](../tests/measure-fast-path.py). These distinguish
in-process query work from native-plugin work; neither establishes a general
Server throughput or latency guarantee.

An additional [core workload](../examples/core_workload.rs) and
[matrix driver](../tests/measure-core-workloads.py) compare in-process ordinary
and native projection, aggregation and synthetic joins with multiple queries,
payload sizes, windows and runtime flavors. Native aggregation also has a real
RocksDB workload. The local evidence contains 192 projection/aggregation runs,
96 join runs and 48 persistent runs, with every measured result verified.
These compare complete pipeline paths, not isolated schedulers; persistent
throughput does not by itself establish crash recovery.

Focused release qualification also covers 72 projection/aggregation/join runs
and 12 persistent runs with 4 KiB payloads, window 32, one/four queries and both
runtime flavors. Independent oracles verify 1,830,000 measured results.
The ordinary fast-path comparison uses seven interleaved repetitions per window:
median throughput is within 1.3% of the preserved control binary and allocation
counts are effectively unchanged. Full-envelope FIFO budgeting adds about
32 allocated bytes per input in that workload, not another allocation. These
are local measurement results, not a zero-overhead or universal capacity claim.

An [isolated resource soak](../examples/core_resource_soak.rs) checks 1,024
create/start/stop-or-cancel/dispose cycles per runtime after 64 warmup cycles.
Its replacement mode holds 16 services with real temporary files and loopback
listening sockets, replacing 64 services per graph. Both runtimes completed
87,040 file/socket owner pairs, starts and stops, including warmup. Every
post-disposal sample returned to 14 process descriptors and zero Tokio tasks;
live Rust bytes remained exactly flat at 54,412 (current-thread) and 60,756
(two-worker), with zero measured growth slope. This is a scoped lifecycle
fixture, not a process-RSS bound or a many-day production leak guarantee.

Executable coverage includes schema/connection matrices, current-thread and
multithreaded execution, ordering, transaction rollback, scheduled work, partial
fan-out, corrupt storage, cancelled cleanup, live replacement and process crashes.
Native Host and Server tests exercise actual separately built libraries,
bootstrap reconstruction, consumer completion, uncertain commits and persisted
retirement. Bounded core churn additionally checks multi-hop fan-out replacement
and dependent-resource release over 128 cycles on each runtime flavor, including
failed cleanup and retry. Persistent timer-burst cases on both runtimes schedule
256 entries, cancel 64, reconstruct, and verify the remaining 192 results exactly
once without advancing the 320-entry live-input frontier.

Additional core resilience evidence is bound into the executable
[contract inventory](../tests/runtime_parity/computation-contracts.tsv):

| Area | Checked boundary |
|---|---|
| Managed resources | Real post-bind constructor timeout, external port contention, process exit before/after listener replacement, persistent cleanup failure, cancelled restoration and stale lease owners. Accepted intent remains available for retry. Full-queue handover runs 64 cycles per scenario on both runtimes. |
| Configuration storage | Real redb I/O barriers serialize snapshots and commits despite cancellation. Missing secrets/providers/versions and damaged encrypted configuration, receipt and snapshot records fail explicitly without replacing intent or releasing another owner's lease. |
| QoS | 128 seeded schedules execute 131,072 ledger-checked actions. Sixteen-member persistent fan-out verifies 2,048 exact deliveries and 2,048 cancelled admissions per runtime through eight disconnection/rebinding cycles. Membership process crashes and rejected capacity shrink preserve the exact pending obligations. Successful skip diagnostics follow confirmed cursor advancement, not cancelled attempts. |
| Query transactions | Failures after actual scheduling push/remove/pop mutations roll back across native/legacy RocksDB and direct/transaction-body query entrypoints. Interrupted transaction replacement retries rolled-back input or replays committed output without repeating completed step evaluation. |
| Mixed sources and timers | Grouped synthetic joins match an independent oracle with reversed timestamps, alternating source arrival and eight reconstructions per runtime. Scheduling-source tests control forward/backward wall-clock jumps without changing due timestamps or popping work. |
| Responsiveness | With eight permanently ready producers and a 4,096-entry timer burst, both query branches progress in eight trials per runtime. Every live producer appears in actual results. Local debug stop measurements are about 2.1-22.0 ms; the executable regression guard is two seconds, not a production latency promise. |

These are scoped contracts, not proof for every plugin, backend, failure
combination or sustained production workload.

The local 28-profile compatibility gate passes with 7,393 successful test
executions, including separately built plugins and real container-backed
database checks through Podman's Docker-compatible API. Its 29 ignored entries
are explicit worker/diagnostic/quarantine exceptions, not silently omitted
tests; worker exceptions require their crash drivers to pass. Test executions
include repeated cases across feature profiles, not 7,393 distinct scenarios.
The broader [qualification ledger](../tests/runtime_parity/requirements.tsv)
remains at 20 covered, 23 partial and five unqualified contracts; the full
replacement-qualification claim is not met. Remaining core evidence includes
broader bootstrap/retention/writer failure combinations, whole-query
clock/persistence combinations and production-duration persistent workloads.

The main boundaries when adopting the functionality are:

- **Recovery must be configured and supported end to end.** A durable index or
  journal cannot repair an unreplayable source or an acceptance-only destination.
- **Exactly-once external effects are destination-specific.** No general graph
  setting makes arbitrary remote effects transactional.
- **Whole-source-transaction visibility stops at its declared processing boundary.**
  Downstream branches and per-operation consumers can become visible separately.
- **Processing retention and replay receipts are bounded.** Expired receipts or unavailable
  history fail explicitly; unlimited disconnected retention is not promised.
- **Reconstruction needs real recipes and stable identities.** Opaque objects,
  storage migration and arbitrary provider replacement are not inferred.
- **Plugin adoption and operational qualification remain selective.** Framework
  capability does not establish connector feature parity or deployment capacity.

## Implementation and further reading

| Area | References |
|---|---|
| Runtime and contracts | [Design](computation-graph-design.md), [schemas/ports/pipes](computation-graph-schemas-ports-pipes.md), [graph implementation](../src/computation/v1/graph.rs) |
| Query and transaction boundaries | [Transaction guide](computation-graph-transactions.md), [query factory](../src/computation/v1/query.rs), [source-transaction tests](../tests/computation_source_transactions.rs) |
| Recovery and delivery | [QoS guide](computation-graph-qos.md), [path assessment](../src/computation/v1/graph/recovery.rs), [recovery-contract tests](../tests/computation_recovery_contracts.rs) |
| Native plugins | [SDK](../../components/computation-plugin-sdk/README.md), [network components](../../components/computation-plugins/network/README.md), [Host implementation and tests](../../components/host-sdk/src/computation) |
| Managed configuration | [Management guide](managed-configuration.md), [resource-lifecycle tests](../tests/computation_resource_dependencies.rs), [Server configuration and API](https://github.com/drasi-project/drasi-server/blob/agentofreality-parallel-computation-graph/docs/plugin-architecture.md) |
| Using the library | [Rust developer guide](developer-guide/README.md), [usage](computation-graph-usage.md), [configuration](computation-graph-configuration.md), [API reference](computation-graph-reference.md) |
| Qualification | [Current evidence and boundaries](computation-graph-backlog.md#current-compatibility-gate), [foundation runner](../tests/run-computation-foundations.sh), [full compatibility runner](../tests/run-runtime-parity.sh) |
