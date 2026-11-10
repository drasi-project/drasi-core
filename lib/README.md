# DrasiLib

[![Crates.io](https://img.shields.io/crates/v/drasi-lib.svg)](https://crates.io/crates/drasi-lib)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

DrasiLib is a Rust library that brings [Drasi](https://drasi.io/) change processing into your application as an embedded library. It monitors data sources using **continuous queries** and delivers precise change notifications to reactions — all in-process, with no external infrastructure required.

DrasiLib is part of the [Drasi project](https://github.com/drasi-project), a [CNCF Sandbox](https://www.cncf.io/projects/drasi/) Data Change Processing platform.

**New to the Rust library? Start with the [drasi-lib developer book](docs/developer-guide/README.md).**
Build a native Rust inventory application, then develop its queries, processing
pipeline, recovery, deployment, and operations in order. The book includes
a complete file-driven starter project and a retained-output recovery companion.

## How It Works

```
Sources  -->  Continuous Queries  -->  Reactions
  |                 |                     |
Data In       Change Detection       Actions Out
```

1. **Sources** connect to databases, APIs, or streams and model incoming data as a property graph of nodes and relationships.
2. **Continuous Queries** run [Cypher](https://opencypher.org/) or [GQL (ISO/IEC 39075:2024)](https://www.iso.org/standard/76120.html) queries perpetually against that graph. When source data changes, queries detect which results were **added**, **updated** (with before/after), or **deleted**.
3. **Reactions** receive those result changes and take action — send webhooks, write to databases, log alerts, or anything else.

You declare *what changes matter* with a query. DrasiLib handles the rest.

---

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
drasi-lib = { path = "../drasi-core/lib" }
tokio = { version = "1", features = ["full"] }
```

This path assumes your application is beside a matching `drasi-core` checkout.
The APIs in this branch are not necessarily available in published crate versions.
The [development setup](docs/developer-guide/README.md#1-run-your-first-application)
includes a complete native application, its dependencies, and the required
toolchain. Non-jq examples do not need jq build tools.

## ComputationGraph runtime

**ComputationGraph is the only runtime in this branch**, including builds with
`--no-default-features`. It owns running components, keeps failed additions
visible, supports dependency-aware changes and lets independent queries run in
parallel. Existing source/reaction plugins remain usable through adapters;
there is no second execution engine.

`QueryBase`, `SequenceDedup` and the old JSON query-output cache have been removed;
there is no compatibility module or second query executor. Snapshot/outbox
response and stream types remain supported. Use the graph-owned construction,
result and recovery APIs described below.

`start()` reports requested component startup failures through a typed
`PartialStartError` cause. Some independent components may already be running:
inspect their status, then call `stop()` or `shutdown()` before retrying.
Valid declarations waiting for missing dependencies are still accepted.

Start with the guide for your task:

| Guide | What it covers |
|---|---|
| [drasi-lib developer book](docs/developer-guide/README.md) | A linear path from a native application through queries, composition, recovery, deployment and operations |
| [Maintainer's guide](docs/computation-graph-maintainers-guide.md) | Runtime ownership, execution and recovery design, ComponentGraph contrasts, and code navigation for engineers changing Drasi |
| [Design](docs/computation-graph-design.md) | How components, execution, ordering, storage and cleanup fit together |
| [Schemas, ports, and pipes](docs/computation-graph-schemas-ports-pipes.md) | A layered explanation of data contracts, connections, pipe choices, guarantees, and limitations |
| [Usage](docs/computation-graph-usage.md) | A runnable example, readiness, results and shutdown |
| [Configuration](docs/computation-graph-configuration.md) | Exact settings, defaults, queue limits, source order and recovery choices |
| [Middleware transformer](docs/computation-graph-middleware.md) | Reuse query middleware in graph-change pipelines without a query |
| [Transaction transformer](docs/computation-graph-transactions.md) | Run a linear transformer sequence with one commit and isolated per-step state |
| [Implementation reference](docs/computation-graph-reference.md) | API types, state transitions, custom components, connections and code locations |
| [Runtime architecture review](docs/runtime-architecture.md) | Ownership diagrams, complete type inventory, cardinality matrix, namespace LOC heatmap and repeatable drift review |
| [ComputationGraph remaining work](docs/computation-graph-remaining-work.md) | Single active list of validated issues, investigations, qualification gaps and improvements, with priorities, scope and completion criteria |
| [Testing and qualification](#testing-and-qualification) | Executable requirements, mandatory feature profiles and interpretation of qualification evidence |
| [Managed-query test controls](docs/query-test-control.md) | Opt-in independent physical clocks and actual due-work/output frontiers; no production clock or downstream completion substitution |

From the drasi-core repository root, run a complete in-process example:

```bash
cargo run --locked -p drasi-lib --example computation_runtime
```

Use `DrasiLib::builder()` directly: no engine-selection call is needed or
supported. The [usage guide](docs/computation-graph-usage.md#use-the-library-in-an-application)
shows the dependency setup for this branch. There is no `computation` feature
or runtime-selection switch.

The main behaviour to understand before using it:

- Adding a component accepts its node; it does not prove successful
  initialization or startup. Use a component handle to wait for readiness.
- Valid declarations can wait for missing dependencies. The node explains
  what is missing and starts automatically when prerequisites arrive, subject
  to auto-start settings and explicit stops.
- Native components default to arrival-order input merging. Explicit
  `EventTimeAcrossStreams` compares available stream heads while preserving
  each stream's order. Ordinary source subscriptions use a shared ranked
  queue with event time and source-declaration rank; neither waits for unseen
  events from a quiet source.
- Persistent recovery depends on the source, query storage and reaction
  capabilities. Disk indexes alone do not make all output durable.
- Preserve stable storage identities when restarting. Always await shutdown.

[Server configuration](https://github.com/drasi-project/drasi-server/blob/agentofreality-parallel-computation-graph/README.md#execution-engine)
and [test-framework configuration](https://github.com/drasi-project/test-infra/blob/agentofreality-parallel-computation-graph/e2e-test-framework/README.md)
use different settings; their guides show where each setting belongs.

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
                .from_input("sensors")
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

- [ComputationGraph](#computationgraph-runtime)
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
| `with_id(impl Into<String>)` | Stable instance namespace; use distinct IDs for coexisting instances | `drasi-lib` |
| `with_telemetry(TelemetryOptions)` | Runtime-only trace/metric collection policy | `Off` |
| `with_component_logging(ComponentLogRuntime)` | Supply host-composed capture instead of automatic setup | Automatic default logging |
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
    .from_input("orders-db")
    .build();
```

For GQL (ISO 9074:2024 graph query language — not GraphQL):

```rust
let config = Query::gql("active-orders")
    .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o.id, o.total")
    .from_input("orders-db")
    .build();
```

### Query Builder Methods

| Method | Description | Default |
|--------|-------------|---------|
| `query(impl Into<String>)` | Cypher or GQL query string | **Required** |
| `from_input(impl Into<String>)` | Consume an upstream component (a source, another query or a transformer) by ID | **Required** (at least one) |
| `from_input_port(id, port)` | Consume a named output port of an upstream component | — |
| `from_input_with_pipeline(id, Vec<String>)` | Consume with a named middleware pipeline | — |
| `with_input(QueryInputConfig)` | Add a fully specified input (port, label selectors, pipeline) | — |
| `with_result_label(impl Into<String>)` | Label published result nodes (the output schema is unchanged) | `Result` |
| `auto_start(bool)` | Start with `core.start()` | `true` |
| `enable_bootstrap(bool)` | Load initial data from sources | `true` |
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
    .from_input("orders-db")
    .from_input("customers-db")
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

`stop_query()` also cleans up queries in `Error`, including failures during
startup or bootstrap. It releases active subscriptions and processing tasks before
reporting `Stopped`, while preserving the query configuration and persisted state.
Stopping acknowledges the failure; its details remain in the component events,
and the next start retries the query, including a failed construction. A terminal
construction failure stays in `Error` until the query is updated or removed.
The managed `stop_query()` command rejects an already stopped query with an
"already stopped" error. In contrast, calling a query runtime's `Query::stop()`
directly is idempotent and returns `Ok(())` when already stopped.
Updating or removing a failed query, and stopping the instance, also release its
active lifecycle resources. Updating a failed query leaves its replacement stopped
until explicitly started.

`stop_source()` likewise invokes cleanup for sources in `Error`. Source plugins
must release their resources and report `Stopped` before returning success; a
legacy plugin that no-ops in `Error` is rejected instead of leaving the graph at
`Stopping`. Updating/removing failed sources and instance shutdown also invoke
cleanup. Updating a failed source leaves its replacement stopped. Managed
`stop_source()` rejects an already stopped source, even if its direct `stop()`
is idempotent.

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

Aggregate snapshots use the engine's grouping identity even when an ordinary
`RETURN` follows an aggregating `WITH`. Changes to numeric grouping identity,
lazy extrema keys, or older contributor-keyed output require explicit complete
reconstruction of affected queries; upgrading does not automatically rekey
persisted state. See
[aggregate result identity and migration boundaries](../docs/aggregate-result-identity.md)
for regression coverage, empty-group semantics, and compatibility limits.

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

DrasiLib exposes a read-only component view derived from its ComputationGraph
registry. The native graph owns component metadata, instances and lifecycle;
the compatibility view is not a second mutable registry.

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
let snapshot: GraphSnapshot = core.get_graph().await?;
let json = serde_json::to_string_pretty(&snapshot)?;

// Find what depends on a component
let dependents: Vec<ComponentNode> = core.get_dependents("orders-db").await?;

// Find what a component depends on
let deps: Vec<ComponentNode> = core.get_dependencies("my-query").await?;

// Check if safe to remove (errors if other components depend on it)
core.can_remove_component("orders-db").await?;
```

Graph inspection and manager inventory, configuration, labels and subscriptions
return `Result`; unavailable state is an error, not an empty result or a panic.

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
            .from_input("sensors")
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

Logging is independent of new tracing/metrics collection. Both new signals
default to `Off`; enabling them never changes the log filter. Automatic setup
installs no metric recorder or exporter. Invalid log directives and conflicts
with an unrelated host subscriber/logger are returned as initialization errors,
not silently ignored.

Control verbosity with `RUST_LOG`:

```bash
RUST_LOG=info cargo run              # Default level
RUST_LOG=debug cargo run             # Verbose
RUST_LOG=drasi_lib=debug cargo run   # Debug only drasi-lib
```

### Ownership and custom hosts

`init_default_subscriber()` explicitly initializes or borrows the same default
service used by library construction. Use it before constructing components if
their constructor-time logs matter. All default library instances share its
owned worker; stopping one instance does not stop another's logging.

For custom setup, `init_component_logging()` or
`ComponentLogRuntime::new(LogPolicy::new("info")?)` creates capture without
installing globals. Compose `logging.layer()` into the host subscriber, configure
the host's `log` bridge, then supply `logging.clone()` to
`DrasiLib::builder().with_component_logging(...)`. Apply formatting/export filters
per layer, not as a global filter that hides component-routing spans.
The [compiled Stockroom example](docs/developer-guide/stockroom/src/main.rs)
demonstrates composition, setup-failure cleanup and awaited application shutdown.

Keep the logging runtime until all its instances have finished. Await
`logging.shutdown(timeout)` to close admission, drain accepted records and join
its worker. Timeout or cancellation retains cleanup ownership so shutdown can
be retried. Default-service shutdown is terminal: its installed global subscriber
cannot be replaced, and subsequent automatic setup returns `LoggingError::Closed`.
`logging.flush()` waits for earlier accepted messages without closing capture.
Full/closed transport losses are exposed by `logging.losses()` and non-recursive,
rate-limited stderr diagnostics. These logs are not a durable audit ledger.

Migration notes:

- `get_or_init_global_registry()` is passive; use a logging initializer when
  setup is intended. Plain library construction still initializes defaults.
- Standalone examples and component integration tests initialize the Drasi
  default service instead of installing a competing `env_logger`. The examples
  use the same INFO/stdout defaults as plain library construction; `RUST_LOG`
  remains the verbosity override. Custom stderr output is demonstrated by
  Stockroom's explicitly composed host.
- `ComponentLogLayer::new` takes a runtime, not a registry. Prefer
  `ComponentLogRuntime::layer()` for the independently filtered layer.
- Source/reaction runtime contexts include an optional host-local `log_binding`.
  Their constructors remain usable; struct literals must supply this field.
  The binding does not cross the legacy ABI. Host callbacks use its actual key
  and destination instead of a mandatory global registry or plugin-supplied key.
- Log-history removal now returns a result and is ordered after accepted capture
  messages, so queued stop logs do not recreate history that was just removed.
- Legacy plugin forwarding uses log-only policy, never the subscriber's maximum
  span level. Callbacks enforce target/level policy before history admission.
  Span/field directives cannot be reconstructed at that boundary and are
  reported as unsupported; use a static target/level capture policy for legacy
  plugins. Forwarded console messages use the `drasi_plugin` target.

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
    pub component_type: ComponentLogType, // Includes native execution roles
}
```

Standard `log::info!()` and `tracing::info!()` macros both work inside plugin code — logs are automatically routed to the component that spawned the task.

## Telemetry foundations

`observability` exposes shared catalogue descriptors, `TelemetryProfile`,
`TelemetrySettings`, `TelemetryOptions`, parent-based sampling decisions and
validated metric registration. The existing snapshot-oriented `metrics` module,
inspection APIs and profiler are unchanged.

| Profile | Eligible observations |
|---|---|
| `Off` | No new tracing or metrics; ordinary logging remains available |
| `Basic` | Basic catalogue entries |
| `Debug` | Basic plus debug operation detail |
| `Persistence` | Basic plus supported persistence entries |

Enabled profiles instrument the owning graph's actual operation boundaries:

| Detail | Supported spans |
|---|---|
| Basic | Source publication, transformer/query processing, single/batch sink handling, bootstrap, scheduled/replay attempts, ordinary reaction workers, start/stop/reconfigure, and control requests |
| Debug | Query input/output preparation and finalization, inner evaluation, query-owned commits, pipe acceptance, output confirmation, and supported acknowledgements |

Publication excludes idle `next()` waiting; batch handling excludes collection.
Processing excludes later forwarding, confirmation, and acknowledgement. Sink
completion stays `accepted` or `handled` according to its declared contract.
Query input processing and input-driven continuations use `query.process`;
transformers use `transformer.process`, and bounded service work retains
`component.process`.
`query.evaluate` measures the evaluator-only boundary, not the full query
callback. Query and transformer processing were previously named `component.process`:
update span-name filters and saved searches accordingly. Component identities, causal
parentage, timing boundaries and metric names/labels are unchanged.
Graph-owned operation spans for components constructed from a registered
factory specification also include `implementation`, the host-assigned
implementation name (for example `drasi.standard/arithmetic`). Directly
constructed components without a registered factory identity omit it; no type
is guessed from an instance ID or payload. This is a trace attribute, not a
metric label or part of the span name.
Ordinary reaction enqueue acceptance is not completed reaction processing.
Output-only replay does not imply another evaluation. `component.scheduled`
starts only for an actual bounded future-queue, input-replay or output-replay unit;
idle wakeups and final empty drain probes have no processing trace. Scheduled
attempts use new roots, never a stored historical event parent. Real replay
bookkeeping commits can still have debug spans.

For actual query input processing, Debug adds bounded `query.prepare_input`,
`query.prepare_output` and `query.finalize` measurement windows around progress/
checkpoint validation and decoding, result projection/serialization/staging,
and post-commit cache/watermark/profiling publication respectively. They include
elapsed async waits, not just CPU work. No per-row spans or additional metric
instruments are introduced; Basic/Off/Persistence do not collect these phases.
They preserve the caller's execution context, so live output and downstream
handling remain causally parented to the processing/scheduled attempt, not its
last detail span. Unsampled parents are not overridden, and phase detail does
not create unrelated root traces when no operation context is available.

These phases do not exhaustively account for `query.process`: Core transaction
gating/session begin, result/transaction orchestration, scheduling, and
instrumentation/export/boundary overhead can remain between named spans.
Preparation/finalization can contain their own bookkeeping commit in duplicate
or non-atomic paths; do not sum overlapping child durations or interpret a
residual as CPU time. Existing evaluation/commit meanings and metric ownership
are unchanged. Output-only replay and empty wakeup probes are not reclassified
as fresh input processing.

`query.bootstrap` is one aggregate root linked to its initiating start operation.
It includes awaited initialization and completion, with bounded
`bootstrap.snapshot` detail and aggregate counts, not a span per snapshot row.
Restart/rebootstrap suppresses per-row evaluator and commit detail too. Tracing
does not change startup readiness or cancellation-safe provider cleanup.

Ordinary reaction workers emit `reaction.process` or `reaction.process_batch`
around actual effects, separately from enqueue acceptance. Batches choose a
shared parent or bounded links before sampling; collection is outside handling.
The trace scope can include checkpoint/policy work. Separate worker metric
guards account for actual handling before checkpointing; neither observation
adds acknowledgements or stronger delivery guarantees.

The common component instruments count operation attempts, successfully processed
input envelopes, and validated output envelopes before fan-out. Source dispatch
counts successful publication attempts accepted by all bound destinations.
Successful idempotent admission retries are publication attempts but do not
produce new envelopes or physical queue entries. Call durations use local
monotonic clocks and record success, error or cancellation independently of
tracing. The `up` gauge follows lifecycle/readiness, not idle or backpressure.

Queue observations use one graph-scoped identity per physical owner, including
shared ranked queues and multicast. Volatile depth describes queued deliveries;
QoS/retained depth describes unique retained records still owed to at least one
non-retired consumer, including delivered-but-unacknowledged work. Completed
retained history is excluded. Disconnection does not retire an obligation.
Only confirmed durable mutations update these observations; page-cache movement,
staging and receipt lookups do not. Blocking counts once per waiting attempt;
drops exclude eviction of already completed history. Queue gauges reset when
their binding ends, and counters retain their stable host series.
Custom providers may expose `PipeControl::observations()`; custom retained stores
may expose `RetainedEnvelopeStore::observations()`. Unsupported capabilities
remain absent, not fabricated zeros. Existing pipe inspection snapshots retain
their original semantics.

Query and ordinary reaction metrics use the same host-owned recorder, even
without tracing, sampling, or the optional OpenTelemetry adapter:

| Instrument | Meaning |
|---|---|
| `drasi.query.events_processed{query_id,phase}` | Completed, nonduplicate data input envelopes, not source changes or timer notifications |
| `drasi.query.results_emitted{query_id,change_kind}` | Newly published add/update/delete result-row changes before fan-out, not replayed envelopes |
| `drasi.query.engine.duration{query_id,phase}` | Seconds per actual evaluation attempt, including failure/cancellation, excluding preparation, hooks, commit, and forwarding |
| `drasi.reaction.results_processed{reaction_id,query_id}` | Successfully handled non-Noop result-row changes at the ordinary worker, not queue acceptance or checkpoint advancement |

`phase` is `bootstrap` or `steady`. Bootstrap records successful snapshot inputs
and actual evaluation, but loading the live view does not emit result-stream
rows. It retains aggregate tracing without per-row spans. Scheduled evaluation
records engine time and new results, but no new data input. Prepared input replay
uses the normal nonduplicate path; output-only replay records no new query input,
evaluation, or computed results. These are process-local observations, not
durable exactly-once totals.

The Persistence profile additionally exposes:

- `drasi.query.commit.duration{query_id,phase,outcome}`: real query-owned commit
  attempts in seconds, including actual bootstrap/replay bookkeeping commits.
- `drasi.query.outbox.entries{query_id}` and `drasi.query.live_results{query_id}`:
  known framework retention/cardinality, including in-memory providers, not disk
  or storage-engine statistics. Removed bindings clear previously known gauges.
- `drasi.reaction.checkpoint_lag_events{reaction_id,query_id}`: current query
  result-sequence head minus a confirmed checkpoint in the same epoch. It updates
  on both head and checkpoint changes, including while handling is blocked.
  Staged bootstrap checkpoints and enqueue progress do not advance it.
- `drasi.reaction.dedup_skips`, `drasi.reaction.gaps_detected`, and
  `drasi.reaction.recovery_triggers{policy}`: actual per-reaction/query recovery
  decisions; policy is `strict`, `auto_reset`, or `auto_skip_gap`.

Unknown, unsupported, or epoch-mismatched checkpoint lag is absent, not a
fabricated zero. A recorder may retain the last series after observations stop.
Existing reaction snapshots retain their compatibility behavior, including the
runtime adapter's acceptance-based progress; they are not the source of the new
confirmed-checkpoint gauge. Existing query/lifecycle snapshots and profiling
metadata remain available with telemetry Off.

Handling success is independent of checkpoint success: completed rows remain
counted if a later checkpoint write fails and can count again when retried.
Partial batches count only positively completed rows; dropped/failed rows do not
become successes under AutoSkipGap. Whole-input handled counts wait for all of
that input's rows, including across split batches. Application-channel forwarding
is a local boundary, not application-consumer completion.

Bounded reaction/service work records common calls/durations/errors at the
worker. Once that component uses work observations, its service `run` lifetime
is not an additional owner of propagated processing errors. Checkpoint write
failures are observed separately as storage errors. Idle service loops do not
create work samples.

Native metric callbacks and completed-span/log collection use the separately
negotiated shared telemetry family. `require_native_metrics()` checks the actual
plugin capability at construction. Exporters remain host-owned; none is installed
automatically. Opaque native/legacy workers do not gain internal metrics merely
from host wrappers.

### Opt-in handled-branch latency

Add `.with_handled_branch_latency(HandledBranchLimits::default())` to
`TelemetrySettings` and supply the existing host-owned metric registration context.
This explicitly enables `drasi.pipeline.end_to_end.duration`; every profile still
leaves it off by default, and metric exclusions/disablement take precedence.
Tracing, OTel and sampling are independent.

One sample is the elapsed monotonic time from a fresh envelope's framework source
publication/admission entry to successful handling of one complete query-output
envelope at one sink. Source polling and upstream event age are excluded; graph
queueing, query processing/commit, batching and effect work are included. Each
fan-out sink and each distinct query output completes independently. `phase` is
`steady`; bootstrap, scheduled-only and reconstructed replay without a valid
original timing owner produce no sample. Metric labels never contain timing
tokens, generations, event/trace IDs or row ordinals.

Failed, dropped, partially handled and acceptance-only branches do not record a
success. A live retry keeps the original start; successful completion records
once before later checkpoint/acknowledgement work. Split worker batches retain
stable original non-Noop row ordinals. Query-results outlets and ordinary
application-channel forwarding are not terminal handling. Graph forwarding sinks
must override `observes_handled_branch_latency()` to return false; this does not
change their existing delivery contract, counters or trace topology.

Tracking has finite per-graph origin/lease/branch and byte budgets, with no
age-based expiry or eviction of admitted work. Default limits are 16,384
accounting records and 8 MiB of record/key/row data; allocator/map overhead is
additionally count-bounded, not included in that byte count. Capacity loss omits
new timing, not business data, and seals new branch admission for that origin
without discarding its admitted branches. Final live ownership releases timing
and retry receipts; completed QoS history retains no timing owner.

`GraphTelemetry::handled_branch_diagnostics()` exposes current charges,
completion/rejection counts and coverage gaps without traces or an exporter.
The histogram describes successful tracked branches, not upstream latency,
all-branches completion, durable exactly-once totals, or an unbiased distribution
when coverage is lost. Native propagation requires the V3 capability; older
plugins remain supported with an explicit timing propagation gap. A terminal
native handled callback can still be timed at the host boundary.

Use `TelemetrySettings::new(profile).with_metrics(false)` for tracing-only
policy, or `.with_tracing(false)` for metrics-only policy. Metric exclusions are
literal substrings, not globs; an empty substring excludes all metrics. Trace
sampling never samples metrics. A child inherits its parent's decision; a new
root consults the configured policy once. Sampling ratios must be finite and
within `[0, 1]`.

With `otel-context`, compose `TelemetrySampler(host_root_sampler)` into the host
tracer provider and supply `OpenTelemetryContextAdapter`. The wrapper inherits
parent decisions and applies configured Never/Ratio policies to new roots.
An incompatible host sampler is reported explicitly rather than silently
ignoring a requested root policy. No provider/subscriber is installed for you.

Batch handling uses a child when all callback inputs have the same valid context;
otherwise it uses an explicit root linked to distinct contextual inputs. Mixed
contextual/context-free inputs and different parents in one trace use links too.
Links, identity and `links_truncated` are supplied before sampling; unsampled
batch context still reaches static/native callbacks. `with_trace_link_limits`
accepts positive count/encoded-byte budgets (defaults: 128 links, 64 KiB).
Configure these no larger than the host adapter/processor/exporter limits.
Link truncation never changes the handler's inputs.

The host creates one `MetricRegistrationContext` per supplied
`Arc<dyn metrics::Recorder + Send + Sync>` and clones it into all graphs and
static SDK integrations targeting that recorder. Pass it through
`TelemetryOptions::with_metric_context(...)` before components register/cache
handles. No global installation is required when the recorder is supplied.
Do not construct independent contexts to bypass cross-instance conflicts.
Cached no-op handles are not rebound when a recorder is installed later.

Registration is atomic across callers: the same emitted name must retain its
type, unit and label schema. Label order is normalized; different permitted
identity values form distinct series. Disabled/excluded registrations consume
no state and do not call the recorder. Contexts for different recorders are
independent. Definitions are retained for the recorder's lifetime, not evicted
on component replacement.

`RegistrationLimits::default()` permits 256 name bytes, 1,024 description bytes,
16 labels (including identity), 64 bytes per label key, 1,024 bytes per label
value, 1,024 instrument definitions and 16,384 distinct series per context.
Limits must be positive; exhaustion and invalid enabled registrations return
typed errors. Dimensionless catalogue units map to facade `Unit::Count`;
durations use `Unit::Seconds`. Generation, trace IDs, sequences and raw errors
are not metric labels.

Library graphs use the library instance ID. Direct graphs with either signal
enabled require `TelemetryOptions::with_instance_id(...)`; construction with
telemetry off stays passive. Hosts own namespace uniqueness. Policies,
registration contexts and logging bindings are runtime-only and never enter
graph definitions, business payloads or replay hashes. Factories receive the
actual owning graph's policy/scope through `ConstructionContext::telemetry()`,
including additions and replacements; a temporary assembly graph is not the
owner.

## Portable execution context

### Shared native telemetry

Supply a host-owned `NativeSpanCollector` using
`TelemetryOptions::with_native_span_collector(...)` to collect native internal
spans. Collection is bounded and nonblocking at submission; metrics do not use
its queue. Ordinary logs still use `ComponentLogRuntime`, independently of
tracing and sampling. Native log records preserve original timestamps, target,
safe fields and optional trace context in `LogMessage::metadata`. Existing
constructors and old serialized log fields remain available; Rust callers using
`LogMessage` struct literals must initialize the new optional field.

With `otel-context`, wrap a host processor in `OpenTelemetrySpanProcessor`,
compose a clone and its `sampler()` into the host tracer provider, and supply
another clone to `NativeSpanCollector::new`. Both paths share the processor and
resource; native IDs, parentage and timestamps are not recreated as new spans.
Use bounded batch processors/exporters for production. After stopping producers,
await collector shutdown and logging drain before shutting down the provider.
Stopping an instance does not shut down a collector shared with another instance.

Component factories use `ConstructionContext::plugin_metrics(kind)` to cache a
`ScopedMetricEmitter`. Native SDK factories use that same emitter API through
their negotiated binding. Counters accumulate across replacement, including
generation-local `absolute()` reports; custom gauges reset and old handles are
fenced. `require_native_metrics()` is validated against the actual plugin
capability before construction. See the [native SDK guide](../components/computation-plugin-sdk/README.md#telemetry-registration-foundations)
for API and compatibility details.

For a runnable HTTP/PostgreSQL source -> query -> native inspector -> handled
callback trace, see the [observability demonstration](examples/observability/README.md).
It includes a separately built plugin, a live metrics/correlated-log dashboard,
local Jaeger, and an exact trace-shape verifier. `TRACE_SAMPLING=never` demonstrates
metrics/logging independently of sampled traces.

Execution context is transient metadata accompanying work, not the envelope's
persisted `ProcessingContext`/annotations. It holds an optional sending trace ID,
span ID, flags and bounded W3C trace state. Unsampled contexts remain valid.
No live span, arbitrary baggage, transaction, acknowledgement or timing token
travels in the carrier.

The runtime restores the receiving component's execution/logging scope on every
poll and transports context through inputs, outputs, pipes, batches and admission.
Bounded live sidecars preserve journal context across page-cache eviction.
Persistent reconstruction has no historical execution context. Event codecs,
query results, receipt digests and acknowledgement semantics are unchanged.
Default logging and existing metrics behavior are unchanged too.

Use `TelemetryOptions::with_context_adapter(...)` to supply a host-owned
`TraceContextAdapter`. `require_distributed_tracing()` validates that an adapter
is present and requires native context capability where native components are
bound. The optional `otel-context` feature supplies `OpenTelemetryContextAdapter`;
the host composes its `tracing-opentelemetry` layer with context activation enabled
and owns collection/export/shutdown. No helper installs a subscriber or exporter.
An adapter alone does not enable a collection profile or native span collection.
Graph-owned operation spans are emitted once on the host in static and native
builds; native component-authored children still require their own local tracing
integration, and production transport of completed native spans is separate.

`ExecutionScope::capture()` captures local execution bindings for explicitly
spawned work. The shared `spawn_owned_component_worker` helper keeps those
bindings but clears the startup operation's parent for a long-lived worker.
Use `spawn_owned_scoped_worker` for short child work such as one snapshot, or
`spawn_owned_scoped_blocking_worker` for its blocking equivalent. The original
ownership-only helpers remain available and do not implicitly capture context.
For a queue, retain each item's `ExecutionContext`, then use the receiving scope's
`with_context(...)`. An explicitly empty scope clears inherited context.
The component identity is the actual runtime construction/logging identity:
an ordinary instance uses its instance ID, while a standalone graph's execution
scope remains its graph ID. A separate configured telemetry namespace does not
rename existing component log keys.

Ordinary dispatchers and priority queues have transient `ChangeMessage<T>`
sidecars. Use `recv_message`, `dequeue_message`, and `try_dequeue_message` in
workers; payload-only convenience methods intentionally discard context.
Persisted replay has an empty historical context, while replay-then-live
receivers preserve subsequent live sidecars. No event/WAL/checkpoint format
or receipt digest contains this metadata.

`ReactionBase::worker_telemetry()` provides the receiving component's binding.
Use its `reaction`/`reaction_batch` observation at actual handling, not while
waiting for a queue. Services can use `WorkerTelemetry::capture().service_work`
for a bounded unit; do not instrument a status watcher or the whole service
lifetime as processing. Scope futures per poll and blocking closures with
`ExecutionScope::in_scope`; never hold an entered guard across an await.

For a reaction's real async processing loop:

```rust,ignore
let telemetry = base.worker_telemetry().await;
let message = base.priority_queue.dequeue_message().await;
telemetry.reaction(&message.execution_context, message.payload.results.len())?
    .run(process_result(message.payload))
    .await?;
```

For one bounded blocking snapshot operation, preserve its context while keeping
the join handle in the existing owner:

```rust,ignore
drasi_lib::context::workers::spawn_owned_scoped_blocking_worker(
    &snapshot_worker,
    move || read_snapshot(),
).await?;
```

Providers that start snapshot work during preparation can use
`BootstrapTrace::current()` and call `start()` immediately before that actual
initialization, then scope the work with its returned context. Calling this for
replay-only preparation would create a false bootstrap trace. The
`ComputationBootstrapProvider::bind_bootstrap_trace` hook passes the same lazy
run to subscription-owned initialization; it is host-local, not an ABI frame.

### Rust API migration

- `InputEnvelope` and `OutputEnvelope` now have `execution_context`. Prefer
  `new(port, envelope)` and `with_execution_context(...)` to two-field literals.
- Constructors are context-free. Forward an input's context explicitly, or use
  `OutputEnvelope::with_current_context()` inside a restored producing scope.
  Graph-owned processing fills a missing output context from that actual
  producing operation, not a later unrelated caller. Replay explicitly creates
  a fresh attempt context rather than borrowing a historical input parent.
  A graph-owned source publication can explicitly create and carry its own
  operation context; that does not reconstruct historical traces.
- `EnvelopeSender` implementors must implement `send_message(EnvelopeMessage)`.
  `send`/`send_batch` remain context-free conveniences; `send_messages` preserves
  context through accepted-prefix failures and unattempted suffixes.
- `Delivery::into_parts()` returns `(envelope, execution_context, acknowledgement)`.
  Failure ownership includes the carrier; dropping it is not an acknowledgement.
- Custom `RetainedEnvelopeStore` implementations must implement `append_message`
  and return context in `StoredEnvelope`, retaining it only in bounded live state.
  Never change persisted event bytes to carry it.
- `SourceAdmission::admit` captures the current execution context at publication;
  `admit_message` accepts an explicit carrier. Retry identity remains event-only.

Native context transport uses a negotiated extension, not appended fields in
frozen vtables or legacy Source/Reaction payloads. Missing required capabilities
fail explicitly. Completed native-span/log collection and metric callbacks
remain follow-up work. Legacy Source/Reaction cdylibs without context transport
retain their loading/logging behavior, but cannot provide transparent worker
trace continuity through their unchanged ABI.

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
    .from_input("iot-source")
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

**Available reaction plugins:** `drasi-reaction-http`, `drasi-reaction-a2a`, `drasi-reaction-grpc`, `drasi-reaction-sse`, `drasi-reaction-log`, `drasi-reaction-platform`, `drasi-reaction-profiler`, `drasi-reaction-storedproc-postgres`, `drasi-reaction-storedproc-mysql`, `drasi-reaction-storedproc-mssql`, `drasi-reaction-application`.

### Reaction Recovery

Recovery combines a saved delivery position with retained query results. With
persistent query output and a reaction that saves progress only after handling,
restart can redeliver unconfirmed results: this is **at-least-once**, not
exactly-once delivery. Retention gaps follow the configured recovery policy.

```
Query -> Outbox -> Forwarder -> Reaction handler -> External effect
           ^                                           |
           | replay after saved position               | success
           +---------------- Checkpoint store <--------+
```

1. Each query retains the last N results in an **outbox** (configurable via `with_outbox_capacity`).
2. Reactions persist a **checkpoint** after successful handling, not merely after enqueueing. The saved position includes the query configuration/output identity.
3. On restart, the runtime replays missed results from the outbox starting after the checkpoint.
4. If the checkpoint falls behind the outbox (gap), the **recovery policy** decides what happens.

A fresh trigger reaction starts at the head captured when it subscribes; it does
not replay earlier retained notifications. Snapshot reactions can instead request
an initial bootstrap. The ComputationGraph adapter never advances handled
checkpoints simply because catch-up results entered a plugin queue.

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
| `is_durable=true` + any query with volatile output | Error: the saved position could not be recovered after restart |
| `needs_snapshot_on_fresh_start=true` + `AutoSkipGap` | Error: contradictory (skip means no snapshot) |
| `needs_snapshot_on_fresh_start=false` + `AutoReset` | Error: AutoReset requires bootstrap capability |

#### Query Outbox Configuration

The outbox is a bounded ring buffer on the query side:

```rust
let query = Query::cypher("q1")
    .query("MATCH (n:Sensor) RETURN n.id, n.value")
    .from_input("sensors")
    .with_outbox_capacity(5000)   // retain last 5000 results (default: 1000)
    .build();
```

If a reaction's checkpoint is older than the oldest outbox entry, that's a **gap** — and the recovery policy activates.

#### Persisted Outbox Compatibility

Retained query output uses the ComputationGraph binary envelope codec, which
writes named-field MessagePack. Omitted optional fields, such as
`Update.grouping_keys`, therefore cannot shift later values.

ComputationGraph does not read outbox records written by the earlier runtime.
A retained record that cannot be decoded makes query recovery report an
inconsistent history; it is not dropped or reconstructed. Under `Strict` the
query fails visibly. `AutoReset`, with a declared bootstrap provider, rebuilds
the output **and** the indexes and checkpoints. Clearing only the outbox is not
a safe migration.

### Result Format

Reactions receive `QueryResult` values containing a `query_id`, a per-query
`sequence`, a `timestamp`, metadata, optional profiling and an ordered
`Vec<ResultDiff>`. Each change is one of:

| Variant | Data |
|---|---|
| `Add` / `Delete` | `data`, `row_signature` |
| `Update` | `data`, `before`, `after`, optional `grouping_keys`, `row_signature` |
| `Aggregation` | optional `before`, `after`, `row_signature` |
| `Noop` | No row change |

The [application reaction guide](../components/reactions/application/README.md#what-a-result-contains)
shows the complete message shape, serialized type tags and a handler covering
every variant. Results are changes to rows, not just plain JSON rows.

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
    inputs:
      - sensors
    auto_start: true
    enableBootstrap: true

  - id: cross-source
    query: |
      MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
      WHERE o.status = 'pending'
      RETURN o.id, c.email, o.total
    inputs:
      - orders
      - customers
    joins:
      - id: PLACED_BY
        keys:
          - label: Order
            property: customer_id
          - label: Customer
            property: id
```

### Loading YAML

Inside an async function, with the source/reaction objects already constructed:

```rust,ignore
use drasi_lib::{DrasiLib, DrasiLibConfig};

let yaml = std::fs::read_to_string("config.yaml")?;
let config: DrasiLibConfig = serde_yaml::from_str(&yaml)?;
config.validate()?;

let mut builder = DrasiLib::builder().with_id(&config.id);
if let Some(capacity) = config.priority_queue_capacity {
    builder = builder.with_priority_queue_capacity(capacity);
}
if let Some(capacity) = config.dispatch_buffer_capacity {
    builder = builder.with_dispatch_buffer_capacity(capacity);
}
for q in &config.queries {
    builder = builder.with_query(q.clone());
}
let core = builder
    .with_source(sensors_source)
    .with_source(orders_source)
    .with_source(customers_source)
    .with_reaction(my_reaction)
    .build()
    .await?;
```

This excerpt uses in-memory indexes. Register actual named storage providers
when using storage references; parsing `storage_backends` does not construct
external providers. The three source objects must use the IDs referenced in
the YAML above. Engine selection is also separate from `DrasiLibConfig`;
see the [ComputationGraph configuration reference](docs/computation-graph-configuration.md).

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
| `inputs` | `inputs` | `Vec<QueryInputConfig>` (a component name or `{component, port, nodes, relations, pipeline}`) | `[]` |
| `result` | `result` | `QueryResultConfig` (`{label}`) | `{}` |
| `middleware` | `middleware` | `Vec<SourceMiddlewareConfig>` | `[]` |
| `auto_start` | `auto_start` | `bool` | `true` |
| `enable_bootstrap` | `enableBootstrap` | `bool` | `true` |
| `joins` | `joins` | `Option<Vec<QueryJoinConfig>>` | `None` |
| `priority_queue_capacity` | `priority_queue_capacity` | `Option<usize>` | Instance default, otherwise `10,000` |
| `dispatch_buffer_capacity` | `dispatch_buffer_capacity` | `Option<usize>` | Instance default, otherwise `1,000` |
| `dispatch_mode` | `dispatch_mode` | `Option<DispatchMode>` | `Channel` |
| `storage_backend` | `storage_backend` | `Option<StorageBackendRef>` | Instance default, otherwise in-memory |
| `outbox_capacity` | `outboxCapacity` | `usize` | `1,000` |
| `bootstrap_timeout_secs` | `bootstrapTimeoutSecs` | `u64` | `300` |
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
| `Internal(anyhow::Error)` | Internal error or a public classification carrying its original cause |

**Cause-preserving errors:** operation, state and configuration conversions now
retain their underlying errors in the existing `Internal` variant. Match
`error.classification()` rather than the outer variant to handle their public
category. Existing enum variants and fields are unchanged; direct pattern matches
still compile, but must adopt this accessor to recognize these wrapped errors.
Simple structured errors, such as an unknown component, remain usable directly.

Use `error.downcast_ref::<T>()` to inspect a graph, provider or worker error instead
of parsing its message. `error::OperationFailures::failures()` exposes every error
from a multi-component operation; `computation::v1::LifecycleReportError` retains
the complete start, stop or reconciliation report. Their standard source chain
follows only the primary failure. `ComputationCleanupError::cleanup_error()` keeps
the failed cleanup separate from the original rejection, while its owner remains
available for an explicit cleanup retry. None of these errors implies rollback or
makes retrying an external effect safe.

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

## Testing and qualification

The [requirement ledger](tests/runtime_parity/requirements.tsv) tracks all 48
replacement-confidence scenarios. A passing test is evidence for its asserted
contract, not proof that the entire replacement is qualified. `partial`,
`blocked` and `unqualified` rows remain unsatisfied even when their existing tests
pass.

From the repository root, with Docker available and the pinned Rust toolchain:

```bash
make build-native-network-plugins build-test-plugins
python3 lib/tests/runtime_parity/test_requirements.py
bash lib/tests/runtime_parity/check-runner.sh
bash lib/tests/run-runtime-parity.sh target/runtime-conformance
```

The cross-platform native FFI and coverage jobs use the shared
[protoc setup action](../.github/actions/setup-protoc/action.yml). Its
[installer](../ci/install_protoc.py) pins protoc 36.2 and each official archive's
SHA-256, verifies the executable's version, and exports its compiler/include
paths. Build-cache keys include the installer pin, so a tool upgrade cannot
silently reuse artifacts generated by an older compiler. Updating the pin
requires matching release digests and the native build/wire matrix on all
supported runners; installer unit tests alone do not qualify that matrix.

The runner checks historical case mappings, named requirements, discovery and
actual results across default, no-default, additional recovery/middleware,
integration, plugin-factory, Host SDK and Redis-compatible backend profiles.
The matrix has 35 profiles: the original 32, including the source/database
profiles, plus explicit unit-test profiles for extracted PostgreSQL/MySQL code:

| Profile | Package | Selection |
| --- | --- | --- |
| `postgres-common` | `drasi-postgres-common` | `--no-default-features --features replication --lib` |
| `postgres-native` | `drasi-computation-source-postgres` | `--no-default-features --lib` |
| `mysql-common` | `drasi-mysql-common` | `--no-default-features --features native --lib` |

These profiles run serially within each test binary. `mysql-common` includes
ignored tests so its real MySQL authentication/TLS/connection-retirement case
must execute, not count as a quarantine. A source crate's `--lib` does not execute
its dependencies' unit tests. Keep each contract bound to its actual owning
package, binary, case and profile when moving code.

Backend cases are explicitly selected, not silently left ignored. Ignored
subprocess workers require a passing parent driver. The sole existing DLL stress
quarantine has an owner and expiry and cannot qualify resource-lifetime safety.
Quarantine validation uses the local calendar date at startup, during profile
checks and at final reporting. An expiry remains valid on that date, but a run
crossing into the following day fails validation; a separate successful manual
execution does not renew it or qualify other platforms.
Redis compatibility is not a claim of retained-volume Garnet product recovery.

The integration profile includes live replacement with full persistent multicast
queues, failed handlers/construction, cancelled waits and independent branch
traffic. Real HTTP/gRPC receivers record effects to a synced journal before
replying; partial replies and abrupt runtime death must leave the complete
unacknowledged query result replayable. The PostgreSQL case kills and restarts
an owned container at a stable port, using a pinned multi-architecture image and
a retained Docker volume, then compares recovered query rows and source
checkpoints with the database. These bounded scenarios do not replace longer
soak tests or qualify every connector/backend. Scheduled-query coverage also
combines cancelled/rescheduled timers with a full durable output pipe: recovery
must replay the original result without recalculation or advancing the live
source's checkpoint for timer notifications.

Per-profile discovery, execution logs and `qualification.json` are retained in
the output directory. To enforce the stronger replacement gate:

```bash
python3 lib/tests/runtime_parity/check_requirements.py \
  --evidence-dir target/runtime-conformance --require-qualified
```

This command intentionally fails while any requirement lacks complete passing
evidence. Native durable ingress/consumer services, the Server managed API,
real-product restart matrices and sustained-operation qualification are still
explicit gaps; volatile/lossy configurations remain supported as such.

Coverage CI enables the extra persistent test features and backend targets and
includes Host SDK unit tests. Source coverage does not by itself measure
assertion strength or independently loaded, uninstrumented DLL code.

## Related Projects

- [Drasi documentation](https://drasi.io/)
- [Drasi Platform](https://github.com/drasi-project/drasi-platform) — Kubernetes deployment
- [Drasi Server](https://github.com/drasi-project/drasi-server) — Single-process / Docker deployment
- [Drasi Core](https://github.com/drasi-project/drasi-core) — Continuous query engine (this repo)

## License

Apache License 2.0
