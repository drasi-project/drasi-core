# DrasiLib

[![Crates.io](https://img.shields.io/crates/v/drasi-lib.svg)](https://crates.io/crates/drasi-lib)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

DrasiLib is a Rust library that brings [Drasi](https://drasi.io/) change processing into your application as an embedded library. It monitors data sources using **continuous queries** and delivers precise change notifications to reactions — all in-process, with no external infrastructure required.

DrasiLib is part of the [Drasi project](https://github.com/drasi-project), a [CNCF Sandbox](https://www.cncf.io/projects/drasi/) Data Change Processing platform.

## How It Works

```
Sources  -->  Continuous Queries  -->  Reactions
  |                 |                     |
Data In       Change Detection       Actions Out
```

1. **Sources** connect to databases, APIs, or streams and model incoming data as a property graph of nodes and relationships.
2. **Continuous Queries** run [Cypher](https://opencypher.org/) or [GQL (ISO 9074:2024)](https://www.iso.org/standard/76120.html) queries perpetually against that graph. When source data changes, queries detect which results were **added**, **updated** (with before/after), or **deleted**.
3. **Reactions** receive those result changes and take action — send webhooks, write to databases, log alerts, or anything else.

You declare *what changes matter* with a query. DrasiLib handles the rest.

---

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
drasi-lib = "0.4"
tokio = { version = "1", features = ["full"] }
```

**Note:** If you don't use middleware, or only use non-jq middleware, you don't need these build tools.

## Experimental computation graphs

The **default-off** `computation` feature exposes additive, versioned contracts and a runtime at
`drasi_lib::computation::v1`:

```toml
drasi-lib = { version = "0.9", features = ["computation"] }
```

### Hosting both graphs in one DrasiLib instance

`DrasiLib` can now own **both** its existing ComponentGraph pipeline and explicitly
registered ComputationGraphs. Nothing is automatically converted. Native queries
run their own evaluator, indexes, bootstrap and output recovery; they do not call
the legacy QueryManager.

Run the complete [side-by-side example](examples/computation_instance.rs):

```bash
cargo run -p drasi-lib --features computation --example computation_instance
```

It uses a real ApplicationSource and two ApplicationReactions. Both engines run the
same query over a shared source, then the native graph is stopped while the legacy
pipeline continues.

For an initialized `drasi` instance, an existing `query_config`, and a **fresh**
reaction plugin instance, the compatibility builder looks like this:

```rust,ignore
use drasi_lib::computation::v1::*;

let pipeline = drasi.computation_pipeline("analytics")?;
let reaction = ReactionPluginHost::owned(
    Box::new(reaction),
    pipeline.services(),
    pipeline.catalog(),
    ReactionPluginOptions::default(),
)?;
let graph = pipeline
    .source(
        drasi.borrow_computation_source("orders").await?,
        SourceSubscriptionOptions::default(),
    )?
    .query(query_config)
    .reaction(reaction, true)
    .build()?;
let handle = drasi.add_computation_graph(graph, ComputationOptions::default()).await?;
drasi.start().await?;
// ...
drasi.shutdown().await?;
```

The query must name sources supplied to this builder. Existing `QueryConfig`
synthetic joins, Cypher/GQL, registered middleware, per-source middleware pipelines,
label filters, bootstrap settings, queue capacities and dispatch choices are
translated into native specifications and bindings. The builder includes a
`QueryResultsOutletFactory`, so a query can expose results without any reaction.
`pipeline.catalog()` provides native snapshots, retained replay and a bounded live
broadcast subscription; a lag error is not a lossless subscription.

`add_computation_graph` registers a graph and its instance-owned driver. Its
`auto_start` option joins instance startup, or starts it immediately if the instance
is already running. `with_computation_graph` is also available on `DrasiLibBuilder`
for graphs constructed independently of instance services. Creation and activation
remain separate, with per-item reports on the returned `ComputationHandle`.
Builder validation failures await disposal of transferred graph resources; keep
awaiting a consuming build when those resources require asynchronous cleanup.
If rollback itself fails, the returned `DrasiError::Internal` contains a
`ComputationCleanupError`; downcast to it and await `cleanup()` to retry the
still-owned resources. The same ownership rule applies to rejected registrations.

Use `start_computation_graph`, `stop_computation_graph`, and
`remove_computation_graph` to manage only the selected native graph. Soft stop
parks processing at safe boundaries without destroying the controller; restart
reuses native state and reconstructs plugins when their host has a constructor.
Instance `stop()` stops both systems, while `shutdown()` permanently cancels,
joins and disposes native drivers before completing legacy shutdown. A cancelled
shutdown retains cleanup ownership: await `shutdown()` again. Dropping the entire
instance is **not** a substitute for awaited shutdown.

### Reusing existing plugins

| Plugin family | ComputationGraph integration |
| --- | --- |
| Source | `SourcePluginHost::owned`, `recreatable`, or `borrowed`, with one `LegacySourceSubscription` per native query. Full adapters preserve filtering, bootstrap results, cursor/sequence, schema, timestamps and profiling. |
| Reaction | `ReactionPluginHost` injects a native snapshot fetcher, bridges bootstrap/checkpoint/outbox recovery, and accepts normal `QueryResult` values. Completion is always **Accepted**, never Handled. |
| Bootstrap | Install the existing provider with `SourcePluginHost::set_bootstrap_provider` before initialization. `LegacySourceBootstrap` coordinates snapshots and live subscriptions with native query progress. |
| State, identity, WAL, secrets | `pipeline.services()` exposes graph-scoped instance services. Captured services and bootstrap providers are explicit declared dependencies, separate from desired configuration. State and WAL partitions include instance and graph identity. |
| Index backend | `LegacyIndexProviderAdapter` runs existing `IndexBackendPlugin` implementations in separate namespaces. The pipeline also uses the instance's named/default index provider. Legacy writers imply **non-atomic** publication; persistent recovery requires actual checkpoint, outbox and live-result stores. |
| Native index provider | `query_provider(query_id, provider, publication)` supplies an explicit native provider, including supported atomic output bundles. Callers sharing a provider across instances must supply distinct graph/storage scopes. |

An underlying plugin has exactly one lifecycle owner. A borrowed source is not
initialized, started, stopped, deprovisioned or given a new bootstrap provider by
the native graph. Owned hosts must receive fresh plugin instances. Borrowed
bootstrap/replay requires explicit `borrowed_recovery` permission; source-side
Broadcast requires `allow_broadcast_loss`. Neither permission upgrades a plugin's
capabilities. The instance places initial native subscriptions before the common
legacy source subscription-complete fence.

Use a `SourcePluginConstructor` or `ReactionPluginConstructor` when a plugin cannot
restart the same object. ApplicationSource, for example, consumes its receiver on
first start: reconstruct it and obtain its new application handle. A transient
source cannot recover changes emitted while its native subscription was stopped.
Replay-capable Channel sources resume from **confirmed raw source progress**, not
adapter receipt; the native producer sequence is separate. Snapshot reset, strict
recovery and deliberately lossy gap handling remain distinct policies. A legacy
reaction without handled checkpoints can receive duplicates after restart.
Recreated volatile sources retain the committed raw sequence floor even without
positional replay. Reconstructed volatile queries have a separate incarnation
identity, so an old durable reaction checkpoint cannot match unrelated fresh
query state merely because its sequence and reset generation are both zero.

The optional `drasi-plugin-sdk/computation` module supplies descriptor-backed
`SourcePluginFactory`, `ReactionPluginFactory`, `BootstrapPluginFactory`, and
provider creation helpers. They validate the descriptor's exact kind/configuration
version and available schema, retain an unresolved `PluginConfiguration` recipe,
and reuse existing plugin constructors. No library-to-SDK dependency or plugin ABI
change is introduced. Store the original recipes alongside exported topology and
re-supply their external bindings on import; export never calls `properties()` to
recover potentially secret-bearing resolved configuration.

In-process descriptor creation uses task-scoped instance secret resolution through
`PluginResolution`, not a process-global resolver replacement. This scope also
wraps automatic source/reaction reconstruction. `create_with_services` and
`create_scoped_index_provider` apply it to bootstrap/index creation; other helper
calls can be wrapped with `PluginResolution::run`. Existing schemas sometimes erase
the inner type of `ConfigValue<T>`: those references get structural validation,
then the descriptor performs the concrete type conversion during creation.

**Compatibility limits:** descriptors must already be safely loaded and
version-checked by the host. An FFI plugin retains its host-injected resolver and
executor; task-local scopes do not cross that boundary. Current FFI SourceProxy
reports replay unsupported because its ABI cannot remove position handles.
The adapter does not bypass that restriction. Full source adapters consume and
ignore subscription-control notifications, as the legacy query path does, rather
than encoding them as data; native queries own their scheduled-future control.
Cleanup can only await the plugin's own `stop()` contract; it cannot join
opaque workers that a plugin fails to join itself. No adapter makes arbitrary
plugins durable, lossless, externally exactly-once, or restartable.

### Inspecting and changing a managed graph

`get_computation_graph`, `list_computation_graphs` and
`inspect_computation_graph` expose native state without adding native components
to ComponentGraph. `ComputationInspector` publishes coherent desired/observed
snapshots and the latest 256 controller publications. Requests for evicted history
fail explicitly. The handle exposes the same revision/generation-checked control,
preview/reconcile and desired export APIs as a standalone graph.

`ComputationTopologySource`/`ComputationTopologyFactory` expose that inspection as
queryable graph data: components, resources, active flows and explicitly unbound
relationships. They converge to the latest publication, not every intermediate
transition, and omit configuration values, secret values and failure messages.
`subscribe_computation_logs` reads the existing log registry under
`<instance>::computation::<graph>`; use a wrapped plugin's own ID for its worker
logs. Native transformers use the Query log category and native sinks use Reaction.

This milestone does not add Server YAML/REST routing, a dynamic-plugin loader,
automatic legacy persistence migration, or a second legacy manager hierarchy.

### Native graph contracts

These contracts support custom schema-validated immutable record bytes, ordered
Adds/Updates/Deletes (explicit PATCH versus REPLACE), and input lineage.
Every new graph data-plane boundary carries a `ChangeEnvelope`: a shared immutable
`ChangeEvent` describing a set diff at a time, plus a branch-owned appendable list
of immutable context entries. `append_annotation` extends only that envelope's
history; fanout shares the event and existing entries without mixing branch histories.
`derive` creates a new event for a transformed diff without changing the input.
`Envelope` is a compatibility name for the same type. These contracts also define host-owned
`EnvelopeSource`, `Transformer`, and `EnvelopeSink` traits, named schema ports, and
pipe capability negotiation. A transformer can emit zero, one, or many outputs,
using its own producer stream and sequence while retaining input lineage.

`ComputationGraph::builder` accepts owned native components, explicit named-port
edges with `PipeProvider` instances (normally `BoundedPipeConfig { capacity }`),
and one unique stream binding per output port. Build validates the entire DAG
before provider creation or component starts: roles, endpoints, every connected
port, full schemas, stream identities, finite capacities, capabilities and cycles.
Direct Source -> Sink and arbitrary acyclic transformer chains require **no
Continuous Query**. Fanout shares payloads; fanin preserves per-stream FIFO.
One output stream cannot feed multiple input ports on the same component:
independent queues for that stream would not preserve component-wide FIFO.

Declarative components use `component(ComponentSpecification, Arc<dyn ComponentFactory>)`.
Factories declare implementation/plugin identity, configuration schema/version,
resource interfaces and cardinalities. The complete graph is validated before
factory creation. `declare_resource` records ownership and an unresolved binding;
`provide_resource` supplies the actual instance separately. Missing instances and
failed creation remain visible in per-item deployment reports without erasing
desired specifications. Secret fields require unresolved references; resolved
values and resource handles are excluded from desired snapshots.

Run the native custom-schema example (direct graph and two-transformer chain):

```bash
cargo run -p drasi-lib --features computation --example computation_graph
```

The example includes inputs and expected outputs. Its source, transforms and sink
implement the public traits directly; no legacy adapter or query is hidden inside.

**Pipe profiles:** `BoundedPipeConfig` provides FIFO/backpressure.
`BroadcastPipeConfig` provides exact bounded retention with an explicit report-or-skip
lag policy and no backpressure claim. `RetainedPipeConfig` names a declared
`RetainedStoreResource`; `MemoryEnvelopeStore` retains history within the process,
while `IndexedEnvelopeStore` uses a complete persistent computation index bundle
and `EnvelopeCodec` for durable acceptance/replay. The versioned codec requires
registered schema validators on decode and preserves event metadata, lineage and
immutable annotations; it does not change any legacy serializer.

Retained deliveries use separate one-shot acknowledgements. Dropping or failing
a delivery does not advance its consumer position. The graph acknowledges only
after local handling and output forwarding succeed; it rejects acknowledging
pipes into acceptance-only sinks. These boundaries do not make external effects
exactly-once. Store dependencies participate in graph preflight/ownership, and
retained journals require exclusive binding ownership.

`send_batch` preserves accepted receipts and the exact failed/unattempted suffix.
For a durable commit error, inspect `SendFailure::acceptance()` before deciding
what to retry: `Unknown` is not definite rejection. Explicit event-time input
merging compares available stream heads without reordering any producer's stream.

**Graph-owned queries and legacy boundaries:** `ContinuousQueryFactory` constructs
a Cypher or GQL transformer using an explicit `QueryIndexProviderResource`.
`ContinuousQueryTransformer` also supports programmatic construction and exposes
typed snapshot/retained-history readers. Query evaluation, scheduled future work,
source checkpoints and output publication belong to the new graph; no legacy
query manager is used. Complete persistent bundles stage index/checkpoint/sequence/
outbox/live-row changes together, and startup reconciles those records before
accepting more input. Failed processing is fenced until cleanup/recovery.

`LegacySourceFactory` adapts compatible isolated Channel subscriptions while
preserving raw source sequence/cursor/time/profiling/schema separately from its
own producer sequence. Borrowed sources are never initialized/stopped/deprovisioned;
owned sources require a fresh transferred instance and registered cleanup owner.
The basic adapter does not implicitly inject identity/state/WAL/bootstrap services.
`LegacyReactionFactory` borrows an already-managed Reaction and only enqueues
normal `QueryResult` values, declaring `Accepted`, never `Handled`.
For a dedicated borrowed-source adapter, an edge can explicitly enable
`fence_producer_on_failure`: consumer failure stops only that graph-owned adapter,
releasing its isolated subscription so it cannot block the shared legacy Source.
This is opt-in lifecycle coupling, not a change to default independent activation
or to the legacy Source's lifecycle.

**Recovery:** graph-owned bootstrap streams and watermarks establish the initial
query state separately from creation. Persistent in-progress markers reject
partial bootstrap; explicitly configured `AutoReset` requires a bootstrap provider
and preserves output high-water and reset generation. Non-atomic publication is
an explicit mode with a durable pending-output fence, not an atomicity claim.
`WalReplaySourceFactory` resumes/tails an explicitly registered WAL partition and
reports unavailable positions instead of silently skipping them.

An optional declared `QuerySourceProgressResource` connects that source to its
owning query's confirmed input checkpoints. The source waits for query recovery,
then resumes after the committed **raw source** sequence; adapter producer sequence
remains separate. Confirmation follows successful query/checkpoint/output commit,
not receipt or failed handling. The read-only view is rehydrated from the actual
persistent checkpoint store after reconstruction, carries reset generation and
opaque cursor bytes, and wakes waiting sources with an error if its query fails.
It does not install a position handle on a borrowed legacy Source or prune its WAL.

`QueryReplayTransformer` joins a typed snapshot or retained suffix to the bound
live stream. Native `CheckpointedSink` advances its query-sequence/reset-generation
checkpoint only after actual handling or supported snapshot replacement; failed
initialization never seeds progress. Strict, snapshot-reset and explicitly lossy
gap policies are distinct. A borrowed legacy enqueue sink cannot pretend to
replace external state from a snapshot.

Desired topology snapshots support exact/dependency/dependent/all selections and
versioned JSON export/import. They contain component/resource specifications and
unresolved bindings, not observed health/lifecycle, secrets, data or live handles.
External components/providers must be supplied again; incomplete selections keep
their boundary relationships explicit rather than silently omitting dependencies.

**Lifecycle:** `let run = graph.start()?; run.await?;` drives a caller-owned future.
Every eligible created component is attempted; data relationships are
activation-independent unless explicitly configured otherwise. A failed Source
does not prevent its consumers from starting, and its bound pipes do not turn
into false EOF. There are no detached graph workers. `run.control()` supplies a generation-specific
cancel/status handle; call `control.cancel()` and **keep awaiting the run** to
cancel pending operations and await asynchronous stop hooks. Mutable component
calls use exclusive instance leases, but cancellation never waits for their locks.
`graph.run()` drives deployment without automatic activation and keeps a live
controller open. The control handle exposes deployment/start reports, immutable
desired/observed snapshots, revision-checked lifecycle-policy changes and scoped
start/stop commands, plus generation/operation-bound health reporting.
Deployment constructs and binds components without requiring their sources to
run, external systems to be reachable, or bootstrap to finish. Call
`graph.dispose().await` to release graph-owned provider resources after component
cleanup; borrowed providers are never shut down by the graph. Failed provider
cleanup remains registered and explicitly retryable.

**Live changes:** use `control.preview(revision, mutations)` followed by
`control.reconcile(preview, bindings)`. The immutable preview identifies creation,
replacement, in-place update, restart, pause, binding and removal impact. Execution
checks revision and generation/operation epochs again, then validates all actual
factories/resources before changing desired topology. Factory identities cannot
silently switch implementations. Explicitly supported in-place configuration
updates keep the construction generation; other specification changes reconstruct
only affected instances.

A sink-only replacement parks that sink at a handling boundary, transfers its
unconsumed input queues, and leaves upstream lifecycle hooks alone. Changed pipes
drain in dependency order while their consumers are still active. Providers must
implement `PipeControl::is_idle` to prove a binding can be drained, including
recovered retained backlog; diagnostic metrics are not used as that proof.
Old senders are revoked before replacement bindings are installed. Unchanged
producer streams retain sequence high-water; newly named streams start independent
sequences. Exhausted components and their closed bindings can be explicitly restarted.

`Reject` refuses dependency-breaking removal. `Cascade` removes selected components
and their dependents. `Orphan` requires an explicitly optional, orphan-permitted
relationship and retains that unsatisfied relationship in desired export. `Drain`
waits for pending work through handled boundaries and refuses acceptance-only sinks
or failed processing boundaries. All binding removal waits for admitted work; no
operation claims to undo or drain an external effect merely accepted by a legacy
queue. A removal that would strand a mandatory port is rejected.

Cleanup failure leaves the old desired topology intact and reports partial effects:
successfully stopped instances remain stopped, and bindings to released or
cleanup-failed resources remain visibly failed and quiesced until explicit repair.
Creation/start failures after a desired update leave the new specifications present.
`Retry` applies only to visible retryable failures; terminal creation failures require
a changed specification or removal. Reconciliation remains caller-polled, bounded
by the cleanup deadline, and cancellable while unrelated graph workers keep running.

Only a fully drained, successfully stopped `Completed` graph can restart.
Components and sequence high-watermarks are retained; each new generation gets
fresh pipes. No automatic failure retry or rollback is implied. While the
controller is open, a failed activation may be explicitly stopped and retried
without restarting its independent consumers. Dropping a run closes
its pipes and drops all scoped operations, but leaves `CleanupRequired`: explicitly
call `graph.shutdown().await` to await remaining stop hooks. Cleanup has a bounded
shared deadline; failed/timed-out hooks remain visibly incomplete. Dropping a graph
cannot await cleanup for resources a component itself spawned.

This is a parallel in-process runtime, not a replacement for the platform.
Server configuration, plugin loading/ABI changes, distributed graph control and
generic cross-component exactly-once transactions are not supplied by these
components. Enabling computation alone does not migrate existing Sources, Queries,
Reactions, or change an unextended `DrasiLib` pipeline. Immutable topology snapshots contain no live provider handles
or resolved secrets. The explicit boundary codecs preserve typed values; internal
canonical identity bytes are not used as a reversible wire codec.

Enqueue receipts mean **acceptance only**, not handling or acknowledgement.
Sinks declare a fixed `Accepted` or `Handled` completion boundary; legacy reaction
queue acceptance must never be advertised as completed handling. Local
acknowledgement handles remain outside envelopes and contexts. The volatile bounded
pipe advertises only per-stream FIFO and backpressure; stronger requirements need
an explicitly capable provider. Cross-component transactions and exactly-once
effects are not inferred from a pipe. Sequence is authoritative; equal/backward timestamps do not reorder
a stream. Opaque logical IDs remain producer-owned; `emission_id` is an optional
stream/sequence identity helper, not a global arbitrary-ID deduplication service.

Explicit pipe close rejects sends and drains accepted events. Runtime cancellation
may discard queued/in-flight events and cannot roll back effects. Sequential fanout
is **not atomic**: failure reports prior branch acceptances, cancellation can also
leave partial delivery, and a slow branch backpressures its producer. No failed
branch is automatically retried. Queue capacity bounds envelopes per edge, not
bytes, component state or transformer result vectors. See the v1 rustdocs for the
complete lifecycle, provider and delivery contracts.

## Identity Providers

DrasiLib includes a trait-based identity provider abstraction for authenticating with databases and external services. The core trait (`IdentityProvider`) and `PasswordIdentityProvider` are built into `drasi-lib`. Cloud-specific providers are available as separate crates.

### Built-in: Password Authentication

```rust
use drasi_lib::identity::PasswordIdentityProvider;

let identity = PasswordIdentityProvider::new("myuser", "mypassword");
```

### Azure AD Authentication

Add `drasi-identity-azure` to your dependencies:

```toml
[dependencies]
drasi-identity-azure = "0.1"
```

```rust
use drasi_identity_azure::AzureIdentityProvider;

// System-assigned managed identity
let identity = AzureIdentityProvider::new("user@tenant.onmicrosoft.com")?;

// User-assigned managed identity
let identity = AzureIdentityProvider::with_managed_identity(
    "user@tenant.onmicrosoft.com",
    "03bbedd2-cce5-45ab-9414-1c1cb82361f0",
)?;

// Workload identity (AKS)
let identity = AzureIdentityProvider::with_workload_identity("user@tenant.onmicrosoft.com")?;

// Developer tools (local development)
let identity = AzureIdentityProvider::with_default_credentials("user@tenant.onmicrosoft.com")?;
```

### AWS IAM Authentication

Add `drasi-identity-aws` to your dependencies:

```toml
[dependencies]
drasi-identity-aws = "0.1"
```

```rust
use drasi_identity_aws::AwsIdentityProvider;

// Region from environment
let identity = AwsIdentityProvider::new("mydbuser").await?;

// Explicit region
let identity = AwsIdentityProvider::with_region("mydbuser", "us-west-2").await?;

// Assumed role
let identity = AwsIdentityProvider::with_assumed_role(
    "mydbuser",
    "arn:aws:iam::123456789012:role/my-role", None
).await?;
```

### Using Identity Providers with Reactions

All identity providers implement the `IdentityProvider` trait and can be passed to any reaction or source that supports it:

```rust
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("mydb.postgres.database.azure.com")
    .with_database("mydb")
    .with_identity_provider(identity)
    .build()
    .await?;
```

---

## Initialization Methods

DrasiLib can be initialized in two ways:
1. **Builder Pattern** (Recommended) - Fluent API for programmatic configuration
2. **Config Struct** - Direct configuration for YAML/JSON loading scenarios

---

## Method 1: Builder Pattern (Recommended)

The builder provides a fluent interface for configuring sources, queries, and reactions.

### Basic Example

```rust
use drasi_lib::{DrasiLib, Query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Sources and reactions are plugins — create instances from plugin crates
    let source = my_source::MySource::new("sensors", config)?;
    let reaction = my_reaction::MyReaction::new("alerts", vec!["high-temp".into()]);

    let core = DrasiLib::builder()
        .with_id("my-app")
        .with_source(source)
        .with_reaction(reaction)
        .with_query(
            Query::cypher("high-temp")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s.id, s.temperature")
                .from_source("sensors")
                .build()
        )
        .build()
        .await?;

    core.start().await?;

    // DrasiLib runs until you stop it
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
```

**What happens when you call `start()`:**

1. All sources begin ingesting data and populating the graph.
2. Each query bootstraps (loads initial data), then continuously evaluates against live changes.
3. Reactions subscribe to query results and process every add/update/delete.

---

## Table of Contents

- [Builder API](#builder-api)
- [Query Builder](#query-builder)
- [Multi-Source Queries and Joins](#multi-source-queries-and-joins)
- [Query Examples (Cypher)](#query-examples-cypher)
- [Runtime Management](#runtime-management)
- [Component Lifecycle Events](#component-lifecycle-events)
- [Component Dependency Graph](#component-dependency-graph)
- [Dispatch Modes](#dispatch-modes)
- [Storage Backends](#storage-backends)
- [State Store Providers](#state-store-providers)
- [Logging](#logging)
- [Middleware](#middleware)
- [Plugin Architecture](#plugin-architecture)
- [YAML Configuration](#yaml-configuration)
- [Error Handling](#error-handling)
- [Feature Flags](#feature-flags)

---

## Builder API

Create a DrasiLib instance with `DrasiLib::builder()`:

```rust
let core = DrasiLib::builder()
    .with_id("my-app")                          // Instance name (default: UUID)
    .with_source(source1)                        // Add a source plugin
    .with_source(source2)                        // Add another source
    .with_reaction(reaction)                     // Add a reaction plugin
    .with_query(query_config)                    // Add a query (see Query Builder)
    .with_priority_queue_capacity(50_000)         // Event queue depth (default: 10,000)
    .with_dispatch_buffer_capacity(5_000)         // Channel buffer size (default: 1,000)
    .add_storage_backend(backend_config)          // Optional named backend declaration
    .with_index_provider("rocks", Arc::new(index_provider)) // Configured persistent provider
    .with_state_store_provider(state_store)       // Plugin state persistence
    .build()
    .await?;
```

Sources and reactions are **owned by DrasiLib** after calling `with_source()` / `with_reaction()`. You cannot use the instance after passing it to the builder.

### Builder Method Reference

| Method | Type | Default |
|--------|------|---------|
| `with_id(impl Into<String>)` | Instance name for logging | Auto-generated UUID |
| `with_source(impl Source + 'static)` | Source plugin (chainable) | — |
| `with_reaction(impl Reaction + 'static)` | Reaction plugin (chainable) | — |
| `with_query(QueryConfig)` | Query config from `Query` builder | — |
| `with_priority_queue_capacity(usize)` | Default event queue capacity | `10,000` |
| `with_dispatch_buffer_capacity(usize)` | Default channel buffer size | `1,000` |
| `add_storage_backend(StorageBackendConfig)` | Optional named memory or plugin declaration | — |
| `with_index_provider(name, Arc<dyn IndexBackendPlugin>)` | Register a named persistent provider | — |
| `with_default_index_provider(name, Arc<dyn IndexBackendPlugin>)` | Register a provider as the default | In-memory |
| `with_state_store_provider(Arc<dyn StateStoreProvider>)` | Plugin state persistence | In-memory |
| `build() -> Result<DrasiLib>` | Validate and construct | — |

---

## Query Builder

Use the `Query` builder to create query configurations:

```rust
use drasi_lib::Query;

let config = Query::cypher("active-orders")
    .query(r#"
        MATCH (o:Order)
        WHERE o.status = 'active' AND o.total > 100
        RETURN o.id, o.customer, o.total
    "#)
    .from_source("orders-db")
    .build();
```

For GQL (ISO 9074:2024 graph query language — not GraphQL):

```rust
let config = Query::gql("active-orders")
    .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o.id, o.total")
    .from_source("orders-db")
    .build();
```

### Query Builder Methods

| Method | Description | Default |
|--------|-------------|---------|
| `query(impl Into<String>)` | Cypher or GQL query string | **Required** |
| `from_source(impl Into<String>)` | Subscribe to a source by ID | **Required** (at least one) |
| `from_source_with_pipeline(id, Vec<String>)` | Subscribe with named middleware pipeline | — |
| `auto_start(bool)` | Start with `core.start()` | `true` |
| `enable_bootstrap(bool)` | Load initial data from sources | `true` |
| `with_bootstrap_buffer_size(usize)` | Buffer size during bootstrap | `10,000` |
| `with_joins(Vec<QueryJoinConfig>)` | Synthetic joins for multi-source queries | `None` |
| `with_priority_queue_capacity(usize)` | Override instance-level queue capacity | Inherited |
| `with_dispatch_buffer_capacity(usize)` | Override instance-level buffer size | Inherited |
| `with_dispatch_mode(DispatchMode)` | `Channel` (backpressure) or `Broadcast` (fanout) | `Channel` |
| `with_outbox_capacity(usize)` | Number of recent results retained for reaction replay | `1,000` |
| `with_storage_backend(StorageBackendRef)` | Persistent storage for this query | In-memory |
| `with_recovery_policy(RecoveryPolicy)` | Gap-recovery behavior for persistent queries (`Strict` fails on gap, `AutoReset` wipes + re-bootstraps) | `Strict` (via global default) |
| `with_middleware(SourceMiddlewareConfig)` | Add middleware transformation | `[]` |
| `build() -> QueryConfig` | Build the configuration | — |

---

## Multi-Source Queries and Joins

A single query can span data from multiple sources. Define **synthetic joins** to tell DrasiLib how to create relationships between elements from different sources:

```rust
use drasi_lib::config::{QueryJoinConfig, QueryJoinKeyConfig};

let config = Query::cypher("orders-with-customers")
    .query(r#"
        MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
        WHERE o.status = 'pending'
        RETURN o.id, c.name, c.email, o.total
    "#)
    .from_source("orders-db")
    .from_source("customers-db")
    .with_joins(vec![QueryJoinConfig {
        id: "PLACED_BY".to_string(),
        keys: vec![
            QueryJoinKeyConfig { label: "Order".into(), property: "customer_id".into() },
            QueryJoinKeyConfig { label: "Customer".into(), property: "id".into() },
        ],
    }])
    .build();
```

DrasiLib creates `PLACED_BY` relationships whenever `Order.customer_id == Customer.id`, even though the orders and customers come from different databases.

---

## Query Examples (Cypher)

DrasiLib supports a subset of [openCypher](https://opencypher.org/) optimized for continuous evaluation:

**Simple filter:**
```cypher
MATCH (s:Sensor)
WHERE s.temperature > 80
RETURN s.id, s.temperature, s.location
```

**Relationship traversal:**
```cypher
MATCH (e:Employee)-[:WORKS_IN]->(d:Department)
WHERE d.name = 'Engineering'
RETURN e.name, e.title, d.name
```

**Aggregation (results update as underlying data changes):**
```cypher
MATCH (o:Order)
WHERE o.status = 'completed'
RETURN o.region, count(o) AS order_count, sum(o.total) AS revenue
```

**Multi-hop traversal:**
```cypher
MATCH (c:Customer)-[:PLACED]->(o:Order)-[:CONTAINS]->(p:Product)
WHERE p.category = 'electronics' AND o.total > 500
RETURN c.name, o.id, collect(p.name) AS products
```

**Temporal (NULL-based state detection):**
```cypher
MATCH (t:Task)
WHERE t.completed_at IS NULL AND t.created_at < datetime() - duration('P7D')
RETURN t.id, t.title, t.assignee
```

> **Limitation:** `ORDER BY`, `LIMIT`, and `TOP` are not supported in continuous queries.

---

## Runtime Management

### Lifecycle

```rust
core.start().await?;                    // Start sources -> queries -> reactions
core.stop().await?;                     // Stop reactions -> queries -> sources
let running = core.is_running().await;  // Check if running
```

### Adding, Removing, and Updating Components at Runtime

```rust
// Add (auto-starts if server is running and component has auto_start=true)
core.add_source(new_source).await?;
core.add_query(query_config).await?;
core.add_reaction(new_reaction).await?;

// Remove (cleanup=true calls deprovision() for resource cleanup)
core.remove_source("my-source", /* cleanup */ true).await?;
core.remove_query("my-query").await?;
core.remove_reaction("my-reaction", /* cleanup */ false).await?;

// Hot-swap (preserves graph edges, event history, and relationships)
core.update_source("my-source", replacement_source).await?;
core.update_query("my-query", new_query_config).await?;
core.update_reaction("my-reaction", replacement_reaction).await?;

// Start / stop individual components
core.start_source("my-source").await?;
core.stop_source("my-source").await?;
core.start_query("my-query").await?;
core.stop_query("my-query").await?;
core.start_reaction("my-reaction").await?;
core.stop_reaction("my-reaction").await?;
```

### Inspecting Components

```rust
// List all components with their current status
let sources: Vec<(String, ComponentStatus)> = core.list_sources().await?;
let queries = core.list_queries().await?;
let reactions = core.list_reactions().await?;

// Get status of a specific component
let status: ComponentStatus = core.get_source_status("my-source").await?;

// Get detailed info (type, status, configuration metadata)
let info = core.get_source_info("my-source").await?;       // -> SourceRuntime
let info = core.get_query_info("my-query").await?;         // -> QueryRuntime
let info = core.get_reaction_info("my-reaction").await?;   // -> ReactionRuntime

// Get current query result set as a JSON snapshot
let results: Vec<serde_json::Value> = core.get_query_results("my-query").await?;

// Get query configuration
let config: QueryConfig = core.get_query_config("my-query").await?;

// Export full DrasiLib configuration
let config: DrasiLibConfig = core.get_current_config().await?;
```

### `ComponentStatus` Values

| Status | Meaning |
|--------|---------|
| `Stopped` | Not running (initial state) |
| `Starting` | Initialization in progress |
| `Running` | Actively processing |
| `Stopping` | Graceful shutdown in progress |
| `Error` | Failed (check events for details) |
| `Reconfiguring` | Being updated via `update_*()` |

---

## Component Lifecycle Events

Every status change is recorded and can be subscribed to in real-time:

```rust
// Subscribe to events for a specific component (returns history + live stream)
let (history, mut rx) = core.subscribe_source_events("my-source").await?;
let (history, mut rx) = core.subscribe_query_events("my-query").await?;
let (history, mut rx) = core.subscribe_reaction_events("my-reaction").await?;

// Process historical events
for event in &history {
    println!("[{}] {} -> {:?}", event.timestamp, event.component_id, event.status);
}

// Stream live events
while let Ok(event) = rx.recv().await {
    println!("Live: {} -> {:?} ({})",
        event.component_id,
        event.status,
        event.message.as_deref().unwrap_or("")
    );
}

// Subscribe to ALL component events (global broadcast)
let mut rx = core.subscribe_all_component_events();
while let Ok(event) = rx.recv().await {
    // Receives events from every source, query, and reaction
}
```

### `ComponentEvent` Fields

```rust
pub struct ComponentEvent {
    pub component_id: String,
    pub component_type: ComponentType,  // Source, Query, Reaction, ...
    pub status: ComponentStatus,
    pub timestamp: DateTime<Utc>,
    pub message: Option<String>,
}
```

---

## Component Dependency Graph

DrasiLib maintains a directed graph of all components and their relationships, backed by [petgraph](https://docs.rs/petgraph/). The graph is the single source of truth for component metadata, runtime instances, and lifecycle events.

```
Instance ("my-app")
|-- Owns --> Source: "orders-db"
|              '-- Feeds --> Query: "active-orders"
|-- Owns --> Query: "active-orders"
|              '-- Feeds --> Reaction: "webhook"
'-- Owns --> Reaction: "webhook"
```

### Querying the Graph

```rust
// Full graph snapshot (serializable to JSON via serde)
let snapshot: GraphSnapshot = core.get_graph().await;
let json = serde_json::to_string_pretty(&snapshot)?;

// Find what depends on a component
let dependents: Vec<ComponentNode> = core.get_dependents("orders-db").await;

// Find what a component depends on
let deps: Vec<ComponentNode> = core.get_dependencies("my-query").await;

// Check if safe to remove (errors if other components depend on it)
core.can_remove_component("orders-db").await?;
```

### Relationship Types

| From | Relationship | To |
|------|-------------|-----|
| Source | Feeds | Query |
| Query | Feeds | Reaction |
| BootstrapProvider | Bootstraps | Source |
| IdentityProvider | Authenticates | Component |

All relationships are bidirectional (e.g., `Feeds` / `SubscribesTo`). Ownership edges (`Owns` / `OwnedBy`) are created automatically between the instance root and each component.

---

## Dispatch Modes

Configure how query results are routed to reaction subscribers:

| Mode | Backpressure | Message Loss | Best For |
|------|-------------|--------------|----------|
| **`Channel`** (default) | Yes — slow consumers block producers | None | Reliable delivery, different consumer speeds |
| **`Broadcast`** | No — fast fire-and-forget | Possible if receivers lag | High fanout (many subscribers), uniform speeds |

```rust
Query::cypher("my-query")
    .with_dispatch_mode(DispatchMode::Channel)    // Default: dedicated channel per subscriber
    .build()

Query::cypher("my-query")
    .with_dispatch_mode(DispatchMode::Broadcast)  // Shared broadcast channel
    .build()
```

---

## Storage Backends

By default, query indexes are held in memory. For persistent state that survives restarts, configure a storage backend:

Persistent backends are **named bindings to injected providers**: you construct the
provider (e.g. `RocksDbIndexProvider` from the `drasi-index-rocksdb` crate), register
it under a name with `with_index_provider`, and reference that name from queries.
A separate backend declaration is optional. When present, its id, the provider
registration name, and the `StorageBackendRef::Named` value must match (such as
`rocks` below); none needs to match the provider kind (`rocksdb`). Only in-memory
backends can be configured inline.

```rust
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{DrasiLib, Query, StorageBackendRef};
use std::sync::Arc;

let provider = RocksDbIndexProvider::new("/data/drasi-indexes", false, false)
    .with_memory_budget_bytes(512 << 20)?;

let core = DrasiLib::builder()
    .with_id("my-app")
    // 1. Register the provider under a name
    .with_index_provider("rocks", Arc::new(provider))
    .with_source(source)
    .with_query(
        Query::cypher("my-query")
            .query("MATCH (n:Sensor) RETURN n")
            .from_source("sensors")
            // 2. Assign the backend to a specific query by name
            .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
            .build()
    )
    .build()
    .await?;
```

### `StorageBackendSpec` Variants

| Variant | Fields | Notes |
|---------|--------|-------|
| `Memory` | `enable_archive: bool` | Default. Volatile — data lost on restart. Usable inline or named. |
| `Plugin` | `kind: String` | Declares a named persistent backend (`rocksdb`, `redis`) by kind. The declaration is optional when the provider is registered directly. When present, its id must match the provider registration name and query reference, not `kind`. Plugin backends cannot be configured inline. |

The provider crates define their own construction options (for RocksDB:
data path, archive on/off, direct I/O, and a shared memory budget). Apply those
settings when constructing the provider; `StorageBackendSpec::Plugin` does not
configure an injected provider.

---

## State Store Providers

State stores let plugins (sources, reactions) persist key-value data across restarts. This is independent of query index storage.

```rust
// Default: in-memory (lost on restart)
let core = DrasiLib::builder().with_id("app").build().await?;

// Persistent: redb (ACID-compliant embedded database)
use drasi_state_store_redb::RedbStateStoreProvider;
let core = DrasiLib::builder()
    .with_id("app")
    .with_state_store_provider(Arc::new(RedbStateStoreProvider::new("/data/state.redb")?))
    .build()
    .await?;
```

Plugins access the state store through their runtime context:

```rust
// Inside a Source or Reaction implementation:
async fn initialize(&self, context: SourceRuntimeContext) {
    self.base.initialize(context).await;
}

async fn start(&self) -> Result<()> {
    if let Some(store) = self.base.state_store().await {
        // Read persisted state
        let cursor = store.get("my-store", "last-cursor").await?;
        // Write state
        store.set("my-store", "last-cursor", new_cursor.as_bytes().to_vec()).await?;
    }
    Ok(())
}
```

---

## Logging

DrasiLib provides component-aware logging built on [tracing](https://docs.rs/tracing/). Logging is **initialized automatically** when you call `build()` — no manual setup required.

Control verbosity with `RUST_LOG`:

```bash
RUST_LOG=info cargo run              # Default level
RUST_LOG=debug cargo run             # Verbose
RUST_LOG=drasi_lib=debug cargo run   # Debug only drasi-lib
```

### Subscribing to Component Logs

```rust
// Returns (recent_history, live_broadcast_receiver)
let (history, mut rx) = core.subscribe_source_logs("my-source").await?;
let (history, mut rx) = core.subscribe_query_logs("my-query").await?;
let (history, mut rx) = core.subscribe_reaction_logs("my-reaction").await?;

for msg in &history {
    println!("[{}] {} {}: {}", msg.timestamp, msg.level, msg.component_id, msg.message);
}
while let Ok(msg) = rx.recv().await {
    println!("[LIVE] {}: {}", msg.component_id, msg.message);
}
```

### `LogMessage` Fields

```rust
pub struct LogMessage {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,              // Trace, Debug, Info, Warn, Error
    pub message: String,
    pub instance_id: String,          // DrasiLib instance that owns the component
    pub component_id: String,         // e.g., "my-source"
    pub component_type: ComponentType, // Source, Query, or Reaction
}
```

Standard `log::info!()` and `tracing::info!()` macros both work inside plugin code — logs are automatically routed to the component that spawned the task.

---

## Middleware

Middleware transforms data between sources and queries. Each middleware is a Cargo feature that must be enabled explicitly.

```toml
[dependencies]
drasi-lib = { version = "0.4", features = ["middleware-promote", "middleware-decoder"] }
```

### Available Middleware

| Feature | Kind | Description |
|---------|------|-------------|
| `middleware-jq` | Transform | Apply jq expressions to incoming data |
| `middleware-bundled-jq` | Transform | Same as above, but bundles jq (no system dep) |
| `middleware-map` | Transform | Map properties using JSONPath selectors |
| `middleware-promote` | Transform | Copy nested values to top-level properties |
| `middleware-relabel` | Transform | Rename element labels |
| `middleware-decoder` | Transform | Decode base64, hex, URL-encoded, or JSON-escaped strings |
| `middleware-parse-json` | Transform | Parse JSON strings into structured objects |
| `middleware-unwind` | Transform | Expand arrays into separate graph elements |
| `middleware-all` | Convenience | Enable all middleware |

> **Note:** `middleware-jq` compiles jq from source and requires build tools:
> macOS: `brew install autoconf automake libtool` /
> Ubuntu: `sudo apt-get install autoconf automake libtool flex bison`

### Configuring Middleware on a Query

```rust
use drasi_core::models::SourceMiddlewareConfig;
use serde_json::json;

let config = Query::cypher("my-query")
    .query("MATCH (n:Device) RETURN n")
    .from_source("iot-source")
    .with_middleware(SourceMiddlewareConfig {
        kind: "promote".into(),
        name: "extract-location".into(),
        config: serde_json::from_value(json!({
            "mappings": [
                {"path": "$.metadata.location", "target_name": "location"}
            ]
        })).unwrap(),
    })
    .build();
```

---

## Plugin Architecture

DrasiLib uses a trait-based plugin system. Sources, reactions, bootstrap providers, and index backends are all implemented as plugins.

### Dynamic Plugin Loading

When using cdylib plugins (shared libraries), the plugin loader discovers and loads them from a configured directory:

- Plugins are matched by glob patterns; the defaults (`libdrasi_*` / `drasi_*`) discover every plugin type, so new types are picked up automatically
- Only cdylib shared libraries are loaded: `.dylib` (macOS), `.so` (Linux), `.dll` (Windows)
- Non-cdylib Cargo artifacts (`.rlib`, `.rmeta`, `.d`) that may exist alongside the cdylib are silently ignored
- Each plugin must have exactly one cdylib file; if multiple cdylib extensions exist for the same base name, the loader reports an ambiguity error

### Source Plugins

A source implements the `Source` trait:

```rust
use drasi_lib::{Source, SourceBase, SourceBaseParams, ComponentStatus};
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::channels::SubscriptionResponse;
use async_trait::async_trait;

pub struct MySource {
    base: SourceBase,
    // your config fields
}

#[async_trait]
impl Source for MySource {
    fn id(&self) -> &str { &self.base.get_id() }
    fn type_name(&self) -> &str { "my-source" }
    fn properties(&self) -> HashMap<String, serde_json::Value> { HashMap::new() }
    fn auto_start(&self) -> bool { self.base.get_auto_start() }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        // spawn your data ingestion task
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(&self, settings: SourceSubscriptionSettings) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "MySource").await
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
}
```

**Available source plugins:** `drasi-source-postgres`, `drasi-source-http`, `drasi-source-grpc`, `drasi-source-mock`, `drasi-source-mssql`, `drasi-source-platform`, `drasi-source-application`.

### Reaction Plugins

A reaction implements the `Reaction` trait:

```rust
use drasi_lib::{Reaction, ReactionBase, ReactionBaseParams, ComponentStatus};
use drasi_lib::context::ReactionRuntimeContext;
use async_trait::async_trait;

pub struct MyReaction {
    base: ReactionBase,
}

#[async_trait]
impl Reaction for MyReaction {
    fn id(&self) -> &str { self.base.get_id() }
    fn type_name(&self) -> &str { "my-reaction" }
    fn properties(&self) -> HashMap<String, serde_json::Value> { HashMap::new() }
    fn query_ids(&self) -> Vec<String> { self.base.get_queries().clone() }
    fn auto_start(&self) -> bool { self.base.get_auto_start() }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        // spawn your result processing task — use base.enqueue_query_result()
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
}
```

**Available reaction plugins:** `drasi-reaction-http`, `drasi-reaction-grpc`, `drasi-reaction-sse`, `drasi-reaction-log`, `drasi-reaction-platform`, `drasi-reaction-profiler`, `drasi-reaction-storedproc-postgres`, `drasi-reaction-storedproc-mysql`, `drasi-reaction-storedproc-mssql`, `drasi-reaction-application`.

### Reaction Recovery

Reactions can be stopped and restarted without losing data. The runtime uses a **checkpoint + outbox** mechanism to guarantee at-least-once delivery:

```
Query emits results ──► Outbox (ring buffer) ──► Forwarder ──► Reaction
                           │                        │
                           │  retained N entries     │  tracks last-delivered seq
                           │                        ▼
                           │                   Checkpoint Store
                           │                   (persisted per query)
                           ▼
                     On restart: replay from checkpoint
```

1. Each query retains the last N results in an **outbox** (configurable via `with_outbox_capacity`).
2. Reactions persist a **checkpoint** (sequence number + config hash) after each delivered result.
3. On restart, the runtime replays missed results from the outbox starting after the checkpoint.
4. If the checkpoint falls behind the outbox (gap), the **recovery policy** decides what happens.

#### Recovery Policies

| Policy | Behavior on gap | Use case |
|--------|----------------|----------|
| `Strict` (default) | Fail with error — reaction stops | Correctness-critical (financial, audit) |
| `AutoReset` | Wipe checkpoint, re-bootstrap from full snapshot | Materialized views, caches |
| `AutoSkipGap` | Skip missing entries, resume from latest | Best-effort delivery (alerts, logs) |

#### Configuring Recovery

Recovery is configured via the `Reaction` trait and `ReactionBaseParams`:

```rust
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};

// Per-instance configuration (highest priority):
let params = ReactionBaseParams::new("my-reaction", vec!["q1".into()])
    .with_recovery_policy(ReactionRecoveryPolicy::AutoReset);

let base = ReactionBase::new(params);
```

Or override the default in your `Reaction` trait implementation:

```rust
impl Reaction for MyReaction {
    // ...

    fn is_durable(&self) -> bool {
        true  // requires a durable StateStoreProvider
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true  // triggers bootstrap() on first start with no checkpoint
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::AutoReset
    }

    async fn bootstrap(&self, ctx: BootstrapContext) -> Result<()> {
        // Called on fresh start (if needs_snapshot_on_fresh_start=true)
        // and on AutoReset recovery after a gap.
        let snapshot = ctx.fetch_snapshot().await?;
        while let Some(row) = snapshot.next().await {
            // Process each row...
        }
        Ok(())
    }
}
```

#### Reaction Recovery Trait Methods

| Method | Description | Default |
|--------|-------------|---------|
| `is_durable()` | Whether a persistent state store is required | `false` |
| `needs_snapshot_on_fresh_start()` | Whether to bootstrap on first start (no prior checkpoint) | `false` |
| `default_recovery_policy()` | Fallback policy when not set via `ReactionBaseParams` | `Strict` |
| `bootstrap(ctx)` | Hook called for initial load or `AutoReset` recovery | no-op |

#### Compatibility Rules

The runtime validates these constraints at startup:

| Condition | Result |
|-----------|--------|
| `is_durable=true` + no durable `StateStoreProvider` | Error: cannot persist checkpoints |
| `needs_snapshot_on_fresh_start=true` + `AutoSkipGap` | Error: contradictory (skip means no snapshot) |
| `needs_snapshot_on_fresh_start=false` + `AutoReset` | Error: AutoReset requires bootstrap capability |

#### Query Outbox Configuration

The outbox is a bounded ring buffer on the query side:

```rust
let query = Query::cypher("q1")
    .query("MATCH (n:Sensor) RETURN n.id, n.value")
    .from_source("sensors")
    .with_outbox_capacity(5000)   // retain last 5000 results (default: 1000)
    .build();
```

If a reaction's checkpoint is older than the oldest outbox entry, that's a **gap** — and the recovery policy activates.

### Result Format

Reactions receive `QueryResult` values containing `ResultDiff` items:

```rust
pub enum ResultDiff {
    Add { data: serde_json::Value },
    Delete { data: serde_json::Value },
    Update {
        data: serde_json::Value,      // current row
        before: serde_json::Value,    // previous values
        after: serde_json::Value,     // new values
        grouping_keys: Option<Vec<String>>,
    },
}
```

---

## YAML Configuration

Queries can be defined in YAML and loaded at startup. Sources and reactions are always created programmatically (they are runtime plugin instances, not config).

```yaml
id: my-app
priority_queue_capacity: 50000
dispatch_buffer_capacity: 5000

queries:
  - id: high-temp-alerts
    query: |
      MATCH (s:Sensor)
      WHERE s.temperature > 75
      RETURN s.id, s.temperature, s.location
    queryLanguage: Cypher
    sources:
      - source_id: sensors
    auto_start: true
    enableBootstrap: true
    bootstrapBufferSize: 10000

  - id: cross-source
    query: |
      MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
      WHERE o.status = 'pending'
      RETURN o.id, c.email, o.total
    sources:
      - source_id: orders
      - source_id: customers
    joins:
      - id: PLACED_BY
        keys:
          - label: Order
            property: customer_id
          - label: Customer
            property: id
```

### Loading YAML

```rust
use drasi_lib::DrasiLibConfig;

let yaml = std::fs::read_to_string("config.yaml")?;
let config: DrasiLibConfig = serde_yaml::from_str(&yaml)?;
config.validate()?;

let mut builder = DrasiLib::builder().with_id(&config.id);
for q in &config.queries {
    builder = builder.with_query(q.clone());
}
let core = builder
    .with_source(my_source)
    .with_reaction(my_reaction)
    .build()
    .await?;
```

### `DrasiLibConfig` Fields

| Field | Type | Default |
|-------|------|---------|
| `id` | `String` | UUID |
| `priority_queue_capacity` | `Option<usize>` | `10,000` |
| `dispatch_buffer_capacity` | `Option<usize>` | `1,000` |
| `storage_backends` | `Vec<StorageBackendConfig>` | `[]` |
| `queries` | `Vec<QueryConfig>` | `[]` |

### `QueryConfig` Fields

| Field | YAML Key | Type | Default |
|-------|----------|------|---------|
| `id` | `id` | `String` | **Required** |
| `query` | `query` | `String` | **Required** |
| `query_language` | `queryLanguage` | `Cypher` or `GQL` | `Cypher` |
| `sources` | `sources` | `Vec<SourceSubscriptionConfig>` | `[]` |
| `middleware` | `middleware` | `Vec<SourceMiddlewareConfig>` | `[]` |
| `auto_start` | `auto_start` | `bool` | `true` |
| `enable_bootstrap` | `enableBootstrap` | `bool` | `true` |
| `bootstrap_buffer_size` | `bootstrapBufferSize` | `usize` | `10,000` |
| `joins` | `joins` | `Option<Vec<QueryJoinConfig>>` | `None` |
| `dispatch_mode` | `dispatch_mode` | `Option<DispatchMode>` | `Channel` |
| `storage_backend` | `storage_backend` | `Option<StorageBackendRef>` | In-memory |
| `outbox_capacity` | `outbox_capacity` | `Option<usize>` | `1,000` |
| `recovery_policy` | `recoveryPolicy` | `Option<RecoveryPolicy>` | `Strict` (via global default) |

---

## Error Handling

All public methods return `drasi_lib::Result<T>`, which wraps `DrasiError`:

```rust
use drasi_lib::{DrasiError, Result};

match core.get_source_status("unknown").await {
    Ok(status) => println!("Status: {:?}", status),
    Err(DrasiError::ComponentNotFound { component_type, component_id }) => {
        println!("{component_type} '{component_id}' does not exist");
    }
    Err(e) => println!("Unexpected error: {e}"),
}
```

### `DrasiError` Variants

| Variant | When |
|---------|------|
| `ComponentNotFound { component_type, component_id }` | Component does not exist |
| `AlreadyExists { component_type, component_id }` | Duplicate component ID |
| `InvalidConfig { message }` | Configuration validation failed |
| `InvalidState { message }` | Operation not valid in current state |
| `Validation { message }` | Input validation failed |
| `OperationFailed { component_type, component_id, operation, reason }` | Runtime operation failed |
| `Internal(anyhow::Error)` | Unexpected internal error |

---

## Feature Flags

| Feature | Description |
|---------|-------------|
| `middleware-jq` | JQ transformations (requires system jq build tools) |
| `middleware-bundled-jq` | JQ transformations (bundles jq, no system dependency) |
| `middleware-decoder` | Base64, hex, URL, JSON-escape decoding |
| `middleware-map` | JSONPath property mapping |
| `middleware-parse-json` | Parse JSON strings into objects |
| `middleware-promote` | Promote nested properties to top level |
| `middleware-relabel` | Rename element labels |
| `middleware-unwind` | Expand arrays into elements |
| `middleware-all` | Enable all middleware |
| `azure-identity` | Azure Managed Identity / Workload Identity credential provider |
| `aws-identity` | AWS IAM / RDS credential provider |
| `all-identity` | Enable all identity providers |

---

## Related Projects

- [Drasi documentation](https://drasi.io/)
- [Drasi Platform](https://github.com/drasi-project/drasi-platform) — Kubernetes deployment
- [Drasi Server](https://github.com/drasi-project/drasi-server) — Single-process / Docker deployment
- [Drasi Core](https://github.com/drasi-project/drasi-core) — Continuous query engine (this repo)

## License

Apache License 2.0
