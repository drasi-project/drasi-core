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

Set these on `DrasiLib::builder()`. Sources and reactions are plugin objects
supplied by the application, not plugin kinds that the library discovers from
Server YAML.

| Builder method | Default | Meaning |
|---|---|---|
| `with_id(id)` | `"drasi-lib"` | Instance identity; keep it stable when resuming instance-scoped storage |
| `with_priority_queue_capacity(n)` | 10,000 when no override is set | Default query input-queue capacity |
| `with_dispatch_buffer_capacity(n)` | 1,000 when no override is set | Default runtime dispatch-buffer capacity |
| `with_default_recovery_policy(policy)` | `RecoveryPolicy::Strict` | Query recovery default when the query has no override |
| `with_activation_timeout_secs(seconds)` | 300 | Positive per-component deadline covering input binding, startup and output binding |
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

Use `Query::cypher(id)` or `Query::gql(id)`, then pass the built `QueryConfig` to
DrasiLib. GQL means the graph query language, not GraphQL.

The serialization column below describes **DrasiLib's `QueryConfig`**. It is not
a replacement for Server's configuration DTOs.

| Rust builder method | Serialized `QueryConfig` field | Default / behaviour |
|---|---|---|
| `Query::cypher(id)` / `Query::gql(id)` | `id`, `queryLanguage` | ID required; default language `Cypher`, alternative `GQL` |
| `query(text)` | `query` | Required query text |
| `from_input(id)` | `inputs[].component` (or the bare `inputs[]` name) | Ordered list of upstream components; position determines input rank |
| `from_input_port(id, port)` | `inputs[].port` | Consume a named output port; otherwise the component's primary (or only) output is used |
| `from_input_with_pipeline(id, names)` | `inputs[].pipeline` | No per-input middleware pipeline by default |
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
settings and queries to your builder and supply the plugin objects/providers.

## Ordering and buffers

The source list is significant. `primary` ranks before `secondary` in the example.
For queued events, comparison uses source-reported event timestamp, then this
query-local rank, then the raw source sequence number.

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
resolution uses the existing construction/cleanup boundary. A directly built
graph also has `cleanup_timeout(Duration)`, defaulting to 5 seconds; it is a
per-cleanup-pass bound, not an overall shutdown deadline. Interrupted activation
retains cleanup ownership rather than allowing immediate reuse of damaged state.
Component batches carry definitions and bindings, not a separate execution
policy; they use the owning instance's deadlines.
For DrasiLib, use `with_activation_timeout_secs`; its configuration snapshot
retains `activation_timeout_secs`. Server exposes the same setting as
`instances[].activationTimeoutSecs`, including environment references, and on
instance creation. Set it high enough for the intended full bootstrap workload;
changing a YAML instance setting takes effect when that instance is reconstructed.

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
