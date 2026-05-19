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
    .add_storage_backend(backend_config)          // Named storage backend (RocksDB, Redis)
    .with_index_provider(index_plugin)            // Plugin for persistent indexes
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
| `add_storage_backend(StorageBackendConfig)` | Named storage backend definition | — |
| `with_index_provider(Arc<dyn IndexBackendPlugin>)` | Persistent index plugin | In-memory |
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

```rust
use drasi_lib::{StorageBackendConfig, StorageBackendSpec, StorageBackendRef};

let core = DrasiLib::builder()
    .with_id("my-app")
    // 1. Define a named backend
    .add_storage_backend(StorageBackendConfig {
        id: "rocks".to_string(),
        spec: StorageBackendSpec::RocksDb {
            path: "/data/drasi-indexes".to_string(),
            enable_archive: false,     // Enable drasi.past() time-travel queries
            direct_io: false,          // Bypass OS page cache
        },
    })
    // 2. Provide the plugin that implements the backend
    .with_index_provider(Arc::new(my_rocksdb_plugin))
    .with_source(source)
    .with_query(
        Query::cypher("my-query")
            .query("MATCH (n:Sensor) RETURN n")
            .from_source("sensors")
            // 3. Assign the backend to a specific query
            .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
            .build()
    )
    .build()
    .await?;
```

### `StorageBackendSpec` Variants

| Variant | Fields | Notes |
|---------|--------|-------|
| `Memory` | `enable_archive: bool` | Default. Volatile — data lost on restart. |
| `RocksDb` | `path: String`, `enable_archive: bool`, `direct_io: bool` | Path must be absolute. |
| `Redis` | `connection_string: String`, `cache_size: Option<usize>` | URL must start with `redis://` or `rediss://`. |

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

- Plugins are matched by glob patterns (e.g., `libdrasi_source_*`, `libdrasi_reaction_*`)
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

**Available reaction plugins:** `drasi-reaction-http`, `drasi-reaction-grpc`, `drasi-reaction-grpc-adaptive`, `drasi-reaction-sse`, `drasi-reaction-log`, `drasi-reaction-platform`, `drasi-reaction-profiler`, `drasi-reaction-storedproc-postgres`, `drasi-reaction-storedproc-mysql`, `drasi-reaction-storedproc-mssql`, `drasi-reaction-application`.

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
