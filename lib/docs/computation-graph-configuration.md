# ComputationGraph configuration

[Design](computation-graph-design.md) |
[Usage](computation-graph-usage.md) |
[Implementation reference](computation-graph-reference.md)

**ComponentGraph remains the default.** This reference describes the
ComputationGraph implementation in this branch, not a promise about released
crate versions.

## Put the setting in the right place

| Where you run Drasi | How to select ComputationGraph | Details |
|---|---|---|
| Rust application using DrasiLib | Cargo feature `computation`, then `.with_execution_mode(ExecutionMode::ComputationGraph)` | [Library settings](#library-settings) |
| Drasi Server | `--execution-mode computation-graph`, or YAML `executionMode: computationGraph` | [Server reference](https://github.com/drasi-project/drasi-server/blob/agentofreality-parallel-computation-graph/README.md#execution-engine) |
| Embedded instance in the test framework | Instance override `test_run_overrides.execution_mode: computationGraph` inside a test run | [Harness reference](https://github.com/drasi-project/test-infra/blob/agentofreality-parallel-computation-graph/e2e-test-framework/README.md) |

These are different formats. Server uses `executionMode`; the test framework
uses `execution_mode` at its instance-override location. `DrasiLibConfig` itself
has no engine-selection field: choose the engine on the Rust builder.

Engine selection does not move a running instance or import ComponentGraph's
saved indexes/checkpoints. ComputationGraph uses its own storage namespaces.

## Library settings

Set these on `DrasiLib::builder()`. Sources and reactions are plugin objects
supplied by the application, not plugin kinds that the library discovers from
Server YAML.

| Builder method | Default | Meaning |
|---|---|---|
| `with_execution_mode(mode)` | `ExecutionMode::ComponentGraph` | Engine used by the usual source/query/reaction APIs |
| `with_id(id)` | `"drasi-lib"` | Instance identity; keep it stable when resuming instance-scoped storage |
| `with_priority_queue_capacity(n)` | 10,000 when no override is set | Default query input-queue capacity |
| `with_dispatch_buffer_capacity(n)` | 1,000 when no override is set | Default runtime dispatch-buffer capacity |
| `with_default_recovery_policy(policy)` | `RecoveryPolicy::Strict` | Query recovery default when the query has no override |
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

Example settings, before adding your source/query/reaction objects:

```rust,ignore
use drasi_lib::{DrasiLib, ExecutionMode, RecoveryPolicy};

let builder = DrasiLib::builder()
    .with_id("orders-app")
    .with_execution_mode(ExecutionMode::ComputationGraph)
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
| `from_source(id)` | `sources[].source_id` | Ordered list; position determines source rank |
| `from_source_with_pipeline(id, names)` | `sources[].pipeline` | No per-source middleware pipeline by default |
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

`SourceSubscriptionConfig` also exposes `nodes` and `relations` label mappings.
An omitted mapping does not mean that all data will match the query; the query
and source still perform their normal label filtering.

The retained `with_bootstrap_buffer_size` / `bootstrapBufferSize` setting
defaults to 10,000 in `QueryConfig`. It does **not** set a separate
ComputationGraph live-event inbox limit: use `priority_queue_capacity` for live
events queued while the query bootstraps.

For example:

```rust,ignore
use drasi_lib::{channels::DispatchMode, Query, RecoveryPolicy};

let query = Query::cypher("order-names")
    .query("MATCH (o:Order) RETURN o.name AS name")
    .from_source("primary")
    .from_source("secondary")
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
sources:
  - source_id: primary
  - source_id: secondary
enableBootstrap: false
auto_start: true
priority_queue_capacity: 10000
dispatch_buffer_capacity: 1000
dispatch_mode: channel
outboxCapacity: 2000
bootstrapTimeoutSecs: 60
recoveryPolicy: strict
```

Deserializing configuration does not register source/reaction plugins or select
the engine. When loading a `DrasiLibConfig`, explicitly apply its instance
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

## Startup, shutdown and worker count

`auto_start` requests startup; it does not mean readiness has already been
reached when an add call returns. Use a component handle and a caller deadline
when readiness matters.

`bootstrapTimeoutSecs` is a snapshot/history readiness timeout, not a universal
startup or shutdown timeout. A directly built graph has a separate
`ComputationGraphBuilder::cleanup_timeout(Duration)` setting, defaulting to
5 seconds.

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
