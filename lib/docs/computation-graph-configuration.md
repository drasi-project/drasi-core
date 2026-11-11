# ComputationGraph configuration

[Design](computation-graph-design.md) |
[Usage](computation-graph-usage.md) |
[Implementation reference](computation-graph-reference.md)

**ComputationGraph is the only runtime in this branch.** This reference describes
the branch implementation, not a promise about released crate versions.

## Runtime selection has been removed

| Where you run Drasi | Current setup | Details |
|---|---|---|
| Rust application using DrasiLib | `DrasiLib::builder()`; no engine selector | [Library settings](#library-settings) |
| Drasi Server | Normal configuration; no `executionMode` field or engine CLI flag | [Server reference](https://github.com/drasi-project/drasi-server/blob/agentofreality-parallel-computation-graph/README.md#execution-engine) |
| Embedded instance in the test framework | Normal embedded-instance configuration; no `execution_mode` override | [Harness reference](https://github.com/drasi-project/test-infra/blob/agentofreality-parallel-computation-graph/e2e-test-framework/README.md) |

Remove obsolete engine-selector settings. They must not suggest that
ComponentGraph remains a supported option. DrasiLib no longer has a
`computation` Cargo feature; disabling default features does not disable
ComputationGraph.

This change does not import the retired engine's saved indexes/checkpoints.
ComputationGraph keeps its existing storage namespaces.

## Component UUIDs and names

Desired topology version **2** requires a UUID in every component descriptor and
a separate validated `name` on every `DesiredComponent`. The same UUID is the
component's graph-node key. Names are unique across kinds, including stopped,
waiting and failed declarations. Snapshots and selected subgraphs preserve both
fields; factory specifications remain independent of mutable name metadata.

For DrasiLib, rename through `rename_component` or `ComponentHandle::rename`;
both write through the configuration owner. Standalone graphs also support
`GraphControl::rename_component` and `DesiredMutation::RenameComponent`.
A name-only desired-topology change preserves the existing instance. UUID-addressed connections,
streams, plugin configuration, storage identity and construction generation do
not change. Reusing the old name cannot retarget existing handles or connections.

This is a breaking format change: rebuild authoring definitions with names and
let Core assign their UUIDs, then use fresh processing state. Assigned native
topology and stored definitions contain both fields. Old string IDs and version-1 topology records are not
converted, migrated or deleted. Instance external-ID admission policy and trusted
persisted restoration are separate from these graph naming APIs.

## Configuration ownership and definition batches

Every initialized DrasiLib has a configuration store. Without a configured
backend it uses a private `MemoryConfigStore`; acceptance receipts then report
`durable: false`, and a fresh instance starts with fresh records. Store providers
declare `ConfigurationStoreGuarantees` explicitly. `DURABLE` requires
reconstructible component and resource recipes; selecting a store never changes
the instance's immutable `allowExternalComponentIds` policy.

Definition-based single and batch operations share preparation, ID assignment,
validation, acceptance and realization:

- `apply_configuration(revision, request_id, ConfigurationRequest)` accepts
  optional replacement `desired` records plus `additions`. Each addition is a
  `NewComponentDefinition::Native` or `::Ordinary` with an optional ID.
- `create_managed_component` and `create_managed_ordinary_component` submit
  one-element batches through that path.
- `create_managed_components(revision, request_id, |ids| ...)` builds a complete
  native `DesiredTopology` of factory/resource recipes. Its callback declares
  configuration only; actual construction follows acceptance.
- `create_computation_component(ComponentDefinition)` and `create_components(|ids| ...)`
  use the same owner without a caller-supplied request ID. The latter supplies
  `ComponentBatch` metadata and deferred factories, not eager component objects.
- `with_component_batch_factory(|ids| ...)` contributes scoped native declarations
  to the same initial batch as ordinary builder declarations. Every member is
  validated before any component constructor runs.
- `ids.allocate()?` generates a UUID. `ids.allocate_requested(Some(id))?` accepts
  that exact UUID only when the instance permits external IDs. Derived helpers
  must belong to an owner allocated in the same scope.

New additions never become updates by matching a name. Updates in `desired`
address existing UUIDs. Business configuration remains opaque: UUID-looking
strings inside plugin settings are not rewritten or treated as references.

Native additions can declare UUID-addressed relationships to previously accepted
components. Accepted native topology contains only native declarations; an edge
to an ordinary query can therefore be stored in `boundary_relationships` until
ordinary records are expanded. Both relationship collections are owned desired
configuration, not disposable inspection context. Reconciliation combines them
with generated query input edges and binds required links once both endpoints
exist, without changing their UUIDs. An unresolved name is not a native endpoint;
allocate the dependency through its normal creation API before wiring it.

`AcceptanceReceipt.creation.components` contains every newly declared
component's UUID and original name. Its `allocations` transcript and
`request_digest` are stored atomically with the definition. Reusing the request ID
with the same operation returns those original assignments, even after deletion;
it does not recreate deleted members. Changed operations conflict. Historical
restoration still loads actual records through the configured store owner.

Durable builders intentionally reject initial component additions, including
query declarations and native batches. Configure factories/resolvers and the
store, build the instance from its saved records, then submit revisioned
definition requests. Process-local constructor/object paths do not make opaque
objects reconstructible and are not a durable-import mechanism.

Ordinary memory-only constructors also use this path. Supply lifecycle and input
metadata before constructing:

```rust,ignore
let source = core.create_source("orders", false, |id| make_source(id)).await?;
let reaction = core.create_reaction(
    "consumer",
    vec![ComponentReference::by_name("view")?],
    true,
    |id, inputs| make_reaction(id, inputs),
).await?;
```

Async variants additionally accept `requested_id: Option<ComponentId>` and return
`(object, metadata)` from their futures. Constructors receive assigned UUIDs;
reaction inputs remain names until the owner commits their UUID bindings. Names
are never resolved again after binding. Adding already constructed objects requires
external-ID opt-in and memory-only storage. Their IDs are never rewritten.
Updates address an existing UUID rather than adopting a new identity.

The memory store records process-local declarations explicitly as opaque
bindings. A pending opaque constructor has unavailable configuration, not a fake
empty plugin configuration. Removing its object invalidates that binding;
neither an old snapshot nor reuse of its UUID can restore that object. Durable
ordinary records instead use the configured `OrdinaryComponentResolver` and
explicit query factory. Current and historical restoration load the actual
store owner's records; ordinary import DTOs have no restoration authority.

## Object lifecycle and assessed updates

Object lifetime is distinct from activation. Native components implement optional
async, fallible `initialize`, `start`, `stop` and `dispose` hooks. Ordinary
Source/Reaction implementations first bind their required runtime context through
`bind_runtime_context(context)`; their optional `initialize()` takes no arguments.
Initialization runs once per actual object, after binding and before first start.
Normal stop/start retains that initialized object. Final retirement calls
`dispose`, including after partial initialization. Failed cleanup retains its
actual owner and blocks conflicting activation.

`pause_processing`/`resume_processing` are temporary processing boundaries, not
aliases for stop/start. Borrowed adapters do not take lifecycle ownership.
Explicit destructive removal orders stop, `delete_owned_resources`, dispose and
final release. Disposal, update and ordinary shutdown do not delete stored state.

Managed updates require a registered factory's pure `plan_update` assessment:
`InPlace`, `Restart` or `Recreate`, with explicit `Preserve` or `Rebootstrap` data
semantics. The default is unsupported. Retained objects execute `apply_update`;
reconstruction uses the factory with an owner-issued `ComponentUpdateDirective`.
There is no opaque-object or replacement-callback update API. Source/reaction
update façades accept `OrdinaryComponentRecord`; native definitions use
`apply_configuration`. Memory-only constructor callbacks remain creation inputs.

`AcceptanceReceipt.updates` retains accepted plans and operation IDs.
`CommittedConfiguration.pending_updates` records unfinished execution, including
current and proposed recipes, atomically with the target configuration.
`ConfigurationSession::complete_updates` confirms exact operations without
advancing the desired revision. Core gates processing until that confirmation;
an uncertain acknowledgement is resolved by authoritative readback. Historical
configuration snapshots do not grant permission to replay their old update
operations. Plugins with durable update effects must handle repeated operation
IDs across process recovery.

The generic configuration path currently rejects `Rebootstrap`: initial
bootstrap support alone does not establish an authoritative snapshot/live
handover, safe handling of superseded work, or downstream reset acknowledgement.
It never synthesizes `reset_configuration` to bypass this boundary.
Native live changes to input bindings or resource recipes also require an
assessment that represents those bindings. The specification-only planner cannot
establish that contract, so such changes are rejected rather than silently
reconstructing a live component from creation support alone.

## Declare before dependencies are available

A valid component can be declared before a required source, query, resource or
factory is available. Its observation lists the missing prerequisites in
`waiting_for`; `waiting_reason()` provides a readable explanation. The graph
resumes construction and eligible startup when those prerequisites arrive.
Explicitly stopped components and components with auto-start disabled stay
stopped. Real validation, provider and processing failures are not automatically
retried as dependency waits.

Unconnected ports wait for an explicit connection; the graph does not guess
wiring. The old `allow_incomplete`/`allowIncomplete` switch is removed and
rejected. Construction, live additions and saved definitions use the same
declaration-first policy.

## Library settings

Set these on `DrasiLib::builder()`. The application supplies source/reaction
objects or native factory declarations; the library does not discover Server
YAML plugin kinds itself.

| Builder method | Default | Meaning |
|---|---|---|
| `with_id(id)` | `"drasi-lib"` | Instance identity; keep it stable when resuming instance-scoped storage |
| `with_query_factory(factory)` | None | Explicitly select query construction before adding ordinary queries; no linked fallback |
| `with_configuration_store(store)` | Private in-memory configuration store | Supply a configuration owner with explicit durability/reconstruction guarantees |
| `with_allow_external_component_ids(allow)` | `false` | Immutable opt-in for caller-chosen UUIDs and preconstructed objects; does not waive reconstruction requirements |
| `with_component_batch_factory(declare)` | No initial native batch | Scope UUID allocation for deferred native factories; accepted atomically with ordinary builder declarations in memory-only instances |
| `with_priority_queue_capacity(n)` | 10,000 when no override is set | Default query input-queue capacity |
| `with_dispatch_buffer_capacity(n)` | 1,000 when no override is set | Default runtime dispatch-buffer capacity |
| `with_default_recovery_policy(policy)` | `RecoveryPolicy::Strict` | Query recovery default when the query has no override |
| `with_activation_timeout_secs(seconds)` | 300 | Positive per-component deadline covering input binding, startup and output binding |
| `with_startup_timeout_secs(seconds)` | 600 | Overall `start()` budget, including lifecycle-lock admission and managed reconciliation |
| `with_shutdown_timeout_secs(seconds)` | 60 | Overall `stop()` / `shutdown()` budget, including lock admission and cleanup phases |
| `with_construction_timeout_secs(seconds)` | 5 | One budget for configuration resolution, construction and initialization, or an accepted update |
| `with_resource_construction_timeout_secs(seconds)` | 30 | Per-resource construction budget |
| `with_cleanup_timeout_secs(seconds)` | 5 | Individual owner cleanup budget, independent of the overall shutdown budget |
| `with_index_provider(name, provider)` | None registered by this call | Register an existing index provider under a name |
| `with_default_index_provider(name, provider)` | In-memory indexes otherwise | Register the provider and make it the default for queries |
| `with_state_store_provider(provider)` | In-memory state store | Storage for component state, including supported reaction checkpoints |
| `with_identity_provider(provider)` | None | Supply the default identity provider |
| `with_secret_store_provider(provider)` | None | Supply the secret store |
| `with_wal_provider(provider)` | None | Supply the write-ahead log used by sources that support it |

A query's explicit capacity overrides the corresponding instance default.
Plugins can also allocate their own queues; these settings are not a total
process-memory limit. In particular, check the reaction plugin's own queue
settings rather than assuming an already-created plugin inherits every builder
default.

For native plugin-backed queries, select `plugin.query_factory()?`; Server
resolves the unique eligible loaded implementation. Loading/registering
component recipes is separate from selecting an ordinary-query default.
An empty/source-only instance needs no query factory. See the
[query plugin guide](../../components/computation-plugins/transformers/query/README.md).

`create_computation_component` accepts a native `ComponentDefinition`, assigning
its UUID before factory construction. `add_source_from_factory` and
`add_reaction_from_factory` accept `ComponentAddition` declarations whose
caller-chosen UUIDs require external-ID opt-in. Handles can represent declarations
waiting for dependencies. `ComponentAddition::declaration` can precede factory registration.
Explicit recipes preserve versions, resource dependencies and secret references
for `GraphConfigurationSnapshot::reconstructible_topology()`. Existing legacy
object-based APIs remain supported; the new recipe constructors are native-only.

`DrasiLibConfig` has a different ID default: constructing/deserializing it without
an ID generates a UUID. The Rust builder defaults to `"drasi-lib"`. For persistent
storage, set an explicit stable ID rather than relying on either default.

RocksDB preserves existing short storage-directory names. Longer combined
instance/graph/query identifiers use a stable bounded directory name and retain
their full logical identity; applications do not need to shorten IDs to fit a
filesystem's filename limit.

Example settings, before adding your source/query/reaction objects:

```rust,ignore
use drasi_lib::{DrasiLib, RecoveryPolicy};

let builder = DrasiLib::builder()
    .with_id("orders-app")
    .with_priority_queue_capacity(10_000)
    .with_dispatch_buffer_capacity(1_000)
    .with_default_recovery_policy(RecoveryPolicy::Strict);
```

## Query settings

Use `Query::cypher(name)` or `Query::gql(name)`, then pass the fallible `build()?`
result (`QueryConfig`) to
DrasiLib. GQL means the graph query language, not GraphQL.

The serialization column below describes **DrasiLib's `QueryConfig`**. It is not
a replacement for Server's configuration DTOs or the evaluator's configuration.
Authoring IDs are optional; `AssignedQueryConfig` returned by inspection has an
assigned UUID. A bare input string always means a name, including UUID-looking
strings. Use `QueryInputConfig::by_id(id)` or the JSON `{"id":"UUID"}` reference
form to address an ID explicitly.

| Rust builder method | Serialized `QueryConfig` field | Default / behaviour |
|---|---|---|
| `Query::cypher(name)` / `Query::gql(name)` | `name`, `queryLanguage` | Name required; default language `Cypher`, alternative `GQL` |
| `with_id(id)` | `id` | Omit for a new Core-assigned UUID; a supplied new UUID requires external-ID opt-in |
| `query(text)` | `query` | Required query text |
| `from_input(name)` | `inputs[].component` (or the bare `inputs[]` name) | Ordered list of upstream names; position determines input rank |
| `from_input_port(name, port)` | `inputs[].port` | Consume a named output port; otherwise the component's primary (or only) output is used |
| `from_input_with_pipeline(name, names)` | `inputs[].pipeline` | No per-input middleware pipeline by default |
| `with_input(input)` | `inputs[]` | Fully specified input with port, label selectors and pipeline |
| `with_result_label(label)` | `result.label` | Optional non-empty node label for published results; the output schema stays graph-change |
| `with_middleware(item)` | `middleware` | Empty; each call adds one middleware definition |
| `with_joins(items)` | `joins` | None |
| `auto_start(value)` | `auto_start` | `true`; startup still needs creation and dependencies to succeed |
| `enable_bootstrap(value)` | `enableBootstrap` | `true`; request initial data before live processing |
| `with_priority_queue_capacity(n)` | `priority_queue_capacity` | Query override, then instance value, then 10,000 |
| `with_dispatch_buffer_capacity(n)` | `dispatch_buffer_capacity` | Query override, then instance value, then 1,000 |
| `with_dispatch_mode(mode)` | `dispatch_mode` | `DispatchMode::Channel` |
| `with_storage_backend(backend)` | `storage_backend` | Instance default, otherwise in-memory |
| `with_recovery_policy(policy)` | `recoveryPolicy` | Query override, then instance policy, then `strict` |
| `with_outbox_capacity(n)` | `outboxCapacity` | 1,000 recent result emissions; minimum retained count is 1 |
| `with_bootstrap_timeout_secs(n)` | `bootstrapTimeoutSecs` | 300 seconds for snapshot/history fetches waiting for readiness |

`QueryInputConfig` also exposes `nodes` and `relations` label mappings. A query input
serializes as a bare component name when it sets only the component, otherwise as an
object with `component`, optional `port`, `nodes`, `relations` and `pipeline`; any
other key (including the removed `sources`/`source_id`) is rejected.
An omitted mapping does not mean that all data will match the query; the query
and source still perform their normal label filtering.

Native query updates can wait for an undeclared input producer. An existing
explicit boundary endpoint is retained; a new input with an omitted port stays
unresolved until the producer's declared ports and factory primary are available.
The graph's declaration refresh then resolves the actual primary without guessing
`out`, while preserving the query's native catalogue, middleware, index provider,
and explicit bootstrap bindings. Internal pending boundaries are construction
dependencies, not producer ports or additional executing components.

Use `priority_queue_capacity` for live events queued while the query
bootstraps. There is no separate bootstrap-buffer setting. Unknown query
settings are rejected rather than silently ignored.

For example:

```rust,ignore
use drasi_lib::{channels::DispatchMode, Query, RecoveryPolicy};

let query = Query::cypher("order-names")
    .query("MATCH (o:Order) RETURN o.name AS name")
    .from_input("primary")
    .from_input("secondary")
    .enable_bootstrap(false)
    .auto_start(true)
    .with_priority_queue_capacity(10_000)
    .with_dispatch_buffer_capacity(1_000)
    .with_dispatch_mode(DispatchMode::Channel)
    .with_outbox_capacity(2_000)
    .with_bootstrap_timeout_secs(60)
    .with_recovery_policy(RecoveryPolicy::Strict)
    .build();
```

The corresponding library query configuration is:

```yaml
id: order-names
query: "MATCH (o:Order) RETURN o.name AS name"
queryLanguage: Cypher
inputs:
  - primary
  - secondary
enableBootstrap: false
auto_start: true
priority_queue_capacity: 10000
dispatch_buffer_capacity: 1000
dispatch_mode: channel
outboxCapacity: 2000
bootstrapTimeoutSecs: 60
recoveryPolicy: strict
```

Deserializing configuration does not register source/reaction plugins.
When loading a `DrasiLibConfig`, explicitly apply its instance
settings and queries to your builder and supply the query factory, plugin
objects/providers or native factory declarations.

## Ordering and buffers

The source list is significant. `primary` ranks before `secondary` in the example.
Each producer's admitted order is preserved. Among the available producer heads,
comparison uses source-reported event timestamp, then this query-local rank,
then the raw source sequence number.

- Use the source event wrapper's timestamp, not an element's `effective_from`
  value or a profiling timestamp.
- Standard `SourceBase` dispatch assigns a source sequence before sending the
  event to subscribers. Custom sources must supply a valid sequence.
- Reordering sources changes the configuration used to validate saved query state.
- Earlier events that arrive after processing has begun are not moved back in
  front of an already selected event. There is no wait for all sources to advance.

One `priority_queue_capacity` limit covers a query's shared input queue, including
scheduled signals. It does not include every upstream source buffer or in-flight
send. Use positive queue capacities.

The **source's** dispatch mode determines admission into that input queue:
Channel waits for capacity; Broadcast drops new arrivals when full.
The **query's** dispatch mode controls result delivery to its consumers, not its
input ordering. Increasing a buffer can absorb a burst, but does not fix a slow
consumer and increases memory use.

## Storage and recovery choices

Memory is the simplest starting point and does not survive process restart.
For persistence, supply a compatible index provider and select it with
`StorageBackendRef::Named(name)` or a supported inline backend specification.
See [storage backend configuration](../README.md#storage-backends).

Before relying on restart recovery, check all four:

1. Every source feeding a persistent query supports replay from saved progress.
2. Stateful middleware uses durable mode and the required durable output connections.
3. The query's backend persists the required checkpoints, rows and output history.
4. A durable reaction has persistent state and receives persistent query outputs.

Persistent query indexes alone do not make query output durable.
A provider's `supports_atomic_query_output()` opt-in determines whether index,
checkpoint and output writes can share one transaction. A configuration flag
cannot make an unsupported provider atomic.

See the [middleware guide](computation-graph-middleware.md#durable-state-and-delivery)
for persistent transformer storage and connection requirements. Durable middleware
uses replay-only source subscriptions; it is not a replacement for query bootstrap.

For a [transactional sequence](computation-graph-transactions.md), configure
`TransactionTransformerDefinition.steps` and supply one atomic persistent index
provider for the whole container. Step IDs select separate storage areas within
that provider's transaction. Register implementations through
`TransactionalTransformerRegistry`; ordinary transformer factories are not
implicitly eligible.

| Policy | Query source recovery | Reaction output recovery |
|---|---|---|
| `Strict` (`strict`) | Fail when saved progress cannot be honoured | Fail when missing output cannot be recovered |
| `AutoReset` (`auto_reset`) | Clear incompatible state and rebuild using supported bootstrap | Rebuild from a supported snapshot |
| `AutoSkipGap` (`auto_skip_gap`) | Not a `RecoveryPolicy` option for queries | Deliberately skip an unrecoverable gap; data can be lost |

Query policy comes from `with_recovery_policy` or the instance default.
Reaction policy comes from its plugin/`ReactionBaseParams`; it is not set by
`Query::with_recovery_policy`.

For reactions, AutoSkipGap also applies when a query is reset or an in-memory
query is rebuilt. It starts from the new query's current result position rather
than rebuilding external state. That state can therefore remain incomplete or
stale; choose AutoReset with a snapshot-capable reaction when it must be replaced.

Do not treat restart as a way to reuse unrelated state under the same ID.
Query reset/recreation identity and configuration checks protect against that.
AutoReset does not erase output with a valid foreign producer identity; that
ownership mismatch is an error under every policy. A replacement snapshot after
completed durable bootstrap requires explicit reset authorization: Strict rejects
it, while AutoReset starts a new output generation before replacing state.
The snapshot cannot silently change rows at an unchanged output identity/cursor.

## Startup, shutdown and worker count

`auto_start` requests startup; it does not mean readiness has already been
reached when an add call returns. Use a component handle and a caller deadline
when readiness matters.

`DrasiLib::start()` returns a typed `PartialStartError` cause for failed requested
startup, including ordinary queries. Already started components remain owned and
`is_running()` remains true so that inspection and cleanup are available. Call
`stop()` or `shutdown()` before retrying startup. Missing dependencies remain
visible waits, not errors. Ordinary additions serialize with instance startup
and stop, so their auto-start decision uses the completed lifecycle boundary.
Quiescing and quiesced components appear as `Reconfiguring`, not `Running`.
An initialized query can still serve generation-fenced snapshot/history reads
while quiesced; that read capability does not mean it is processing new inputs.

`bootstrapTimeoutSecs` is a snapshot/history readiness timeout, not a universal
startup or shutdown timeout. Each component's input binding, start hook and
output binding are bounded by graph activation, defaulting to 300 seconds.
Direct graph builders can set `activation_timeout(Duration)`. Configuration
resolution and construction share `construction_timeout(Duration)`, defaulting
to 5 seconds. Resource creation uses `resource_construction_timeout(Duration)`,
defaulting to 30 seconds. `cleanup_timeout(Duration)` defaults to 5 seconds per
owner, separately from the overall shutdown budget. Interrupted activation
retains cleanup ownership rather than allowing immediate reuse of damaged state.
Component batches carry definitions and bindings, not a separate execution
policy; they use the owning instance's deadlines.
For DrasiLib, use `with_activation_timeout_secs`; its configuration snapshot
retains `activation_timeout_secs`. Server exposes the same setting as
`instances[].activationTimeoutSecs`, including environment references, and on
instance creation. Set it high enough for the intended full bootstrap workload;
changing a YAML instance setting takes effect when that instance is reconstructed.

Instance `start()` has a 600-second overall budget; `stop()` and `shutdown()` have
a 60-second overall budget. Each budget starts before lifecycle-lock admission
and is shared across subsequent phases, not restarted at every await. Native
start/stop/quiesce/force-cancellation requests and deployment/startup report waits
also have finite budgets. Direct graph `shutdown()` and `dispose()` retain
unfinished resources when their overall budget expires.

Timeouts retain a typed `GraphError::LifecycleTimeout` cause with the instance
or graph identity, operation, expired phase, configured budget and whether work
was accepted. Acceptance is not completion: inspect the current observations,
then retry cleanup. An accepted start may finish after its caller times out.
Timed-out or cancelled stops block restart until cleanup is retried. Permanent
shutdown remains permanent even when its first cleanup attempt times out, and
configuration storage is not closed underneath unfinished processing. Dropped
management cleanup retains its owner and logs failed attempts with exponential
retry delays from 1 to 30 seconds.

Stop and shutdown supersede the instance-owned startup coordinator and join it
before cleanup. They do not depend on the original `start()` caller continuing
to poll its future. A superseded start reports a typed `StartupSupersededError`
through `PartialStartError`, and cannot later set a stopped instance running.
Managed reconciliation releases the instance gate before construction/startup
waits while retaining the pending graph result. Definition acceptance and
resource-retirement decisions remain serialized; they are not silently cancelled.

Server accepts all six limits as positive `instances[]` settings and on instance
creation: `startupTimeoutSecs`, `shutdownTimeoutSecs`, `constructionTimeoutSecs`,
`resourceConstructionTimeoutSecs`, `cleanupTimeoutSecs` and
`activationTimeoutSecs`. YAML settings support environment references. Omitted
values retain the defaults; configuration snapshots preserve explicit overrides.
Changing an existing instance's YAML requires reconstruction to apply new limits.
Budgets bound cooperative asynchronous work; they cannot preempt nonyielding
synchronous code, a native fault, or constructors an application runs before
handing a component to Drasi.

Reconciliation is bound to the preview's affected components, resources, bindings
and generations. A changed impact returns `GraphError::StalePreview`; obtain a
new preview rather than applying the old one. Always inspect the returned
`ReconciliationReport`'s `committed`, `summary` and `failures`. A pre-commit cleanup
failure can leave the old topology intact with some components stopped.
`report.ensure_committed()?` checks acceptance and retains the complete rejected
report as a typed error. It deliberately does not claim that accepted components
have finished construction or become ready.

There is no ComputationGraph-specific worker-count setting. It uses the host's
Tokio runtime. Independent DrasiLib queries can run on separate workers when that
runtime is multi-threaded; a current-thread runtime is also supported.

## Profiling

Profiling timestamps describe elapsed wall-clock intervals, including waits:

| Interval | What it includes |
|---|---|
| Source send to query receive | Queues, adapters and input preparation before the query's receive stamp |
| Query core call to return | The core call, including its preparation of output and transaction commit |
| Query send to reaction receive | Output delivery, conversion, scheduling and reaction-queue wait |
| Application reaction receive to complete | Handling after dequeue and waiting for application-channel capacity |

Query send is stamped before live publication, not after delivery completes.
Completion stamps are added to live results without rewriting committed disk
records. Replayed disk records can therefore have missing completion timestamps.
Third-party reactions may not fill every field.

These are not CPU measurements. Do not sum overlapping event latencies or
replace missing timestamps with zero. For test-framework capture, use its
`include_profiling` handler option and JSONL output in a separate diagnostic run;
keep that extra file I/O out of throughput comparisons.
