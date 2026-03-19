# DrasiLib

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Crates.io](https://img.shields.io/crates/v/drasi-lib.svg)](https://crates.io/crates/drasi-lib)
[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

DrasiLib is a Rust library for building change-driven solutions that detect and react to data changes with precision. It is the embedded library form of [Drasi](https://drasi.io/), a CNCF Sandbox project.

## What is DrasiLib?

DrasiLib enables real-time data change processing using **continuous queries**. Unlike traditional query systems that run once and return static results, DrasiLib's continuous queries run perpetually, maintaining live result sets and emitting precise change notifications whenever the underlying data changes.

You declare *what changes matter* using [Cypher](https://opencypher.org/) or [GQL (ISO 9074:2024)](https://www.iso.org/standard/76120.html) graph queries. DrasiLib handles the complexity of tracking state, detecting meaningful transitions, and delivering the results.

### How It Works

```
Sources -> Continuous Queries -> Reactions
   |              |                 |
Data In    Change Detection    Actions Out
```

1. **Sources** ingest data from external systems (databases, APIs, message queues) and model them as a property graph of nodes and relationships.
2. **Continuous Queries** evaluate Cypher/GQL queries against the graph whenever the data changes, producing a stream of result changes:
   - **Added** -- a new row matches the query criteria
   - **Updated** -- an existing row still matches but values changed (includes before/after)
   - **Deleted** -- a row no longer matches the query criteria
3. **Reactions** receive those result changes and trigger actions (webhooks, logging, alerts, database writes).

### Key Benefits

- **Declarative** -- define change semantics with Cypher/GQL queries, not custom code
- **Precise** -- get before/after states for every change, not just raw events
- **Pluggable** -- use existing source/reaction plugins or build your own
- **Multi-source** -- a single query can span data from multiple sources
- **Embeddable** -- runs fully in-process with no external infrastructure
- **Scalable** -- built-in backpressure, priority queues, and configurable dispatch modes

---

## Getting Started

### Installation

```toml
[dependencies]
drasi-lib = "0.4"
tokio = { version = "1", features = ["full"] }
```

### Basic Example

```rust
use drasi_lib::{DrasiLib, Query};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Create source and reaction plugin instances
    //    (these are external crates -- DrasiLib only knows the Source/Reaction traits)
    let source = MySource::new("sensors", my_config)?;
    let reaction = MyReaction::new("alerts", vec!["high-temp".into()], reaction_config);

    // 2. Build DrasiLib with the builder pattern
    let core = DrasiLib::builder()
        .with_id("my-app")
        .with_source(source)        // Ownership transferred
        .with_reaction(reaction)    // Ownership transferred
        .with_query(
            Query::cypher("high-temp")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s")
                .from_source("sensors")
                .build()
        )
        .build()
        .await?;

    // 3. Start processing (Sources -> Queries -> Reactions in dependency order)
    core.start().await?;

    // 4. Run until shutdown
    tokio::signal::ctrl_c().await?;
    core.stop().await?;

    Ok(())
}
```

### Middleware Features (Optional)

DrasiLib supports optional middleware for transforming data between sources and queries. **All middleware are disabled by default** to minimize dependencies.

```toml
[dependencies]
drasi-lib = { version = "0.4", features = ["middleware-decoder", "middleware-promote"] }
```

| Feature | Description |
|---------|-------------|
| `middleware-jq` | JQ query language transformations (requires build tools) |
| `middleware-decoder` | Decode base64, hex, URL-encoded strings |
| `middleware-map` | JSONPath-based property mapping |
| `middleware-parse-json` | Parse JSON strings into structured objects |
| `middleware-promote` | Promote nested properties to top level |
| `middleware-relabel` | Transform element labels |
| `middleware-unwind` | Unwind arrays into multiple elements |
| `middleware-all` | Enable all middleware (convenience) |

The `middleware-jq` feature compiles jq from source. Install build tools first:
- **macOS:** `brew install autoconf automake libtool`
- **Ubuntu/Debian:** `sudo apt-get install autoconf automake libtool flex bison`

---

## Builder API

Create a DrasiLib instance with `DrasiLib::builder()` and configure using the fluent API.

### Builder Methods

| Method | Description | Default |
|--------|-------------|---------|
| `.with_id(id)` | Set instance identifier (used for logging) | UUID |
| `.with_source(source)` | Add a source plugin instance (ownership transferred) | -- |
| `.with_reaction(reaction)` | Add a reaction plugin instance (ownership transferred) | -- |
| `.with_query(config)` | Add a query configuration (use `Query` builder) | -- |
| `.with_priority_queue_capacity(n)` | Default event queue capacity | `10000` |
| `.with_dispatch_buffer_capacity(n)` | Default channel buffer capacity | `1000` |
| `.add_storage_backend(config)` | Define a named storage backend (RocksDB, Redis) | -- |
| `.with_index_provider(provider)` | Set the index backend plugin for persistent storage | In-memory |
| `.with_state_store_provider(provider)` | Set the state store for plugin state persistence | In-memory |
| `.build().await?` | Build and initialize the instance | -- |

### Multiple Sources

```rust
DrasiLib::builder()
    .with_source(source1)
    .with_source(source2)
    .with_source(source3)
    // ...
```

### Storage Backends

For persistent query state (survives restarts), configure a storage backend and an index provider:

```rust
use drasi_lib::{StorageBackendConfig, StorageBackendSpec};
use drasi_index_rocksdb::RocksDbIndexPlugin;

let core = DrasiLib::builder()
    .with_id("my-app")
    .add_storage_backend(StorageBackendConfig {
        id: "rocks".to_string(),
        spec: StorageBackendSpec::RocksDb {
            path: "/data/drasi".to_string(),
            enable_archive: false,
            direct_io: false,
        },
    })
    .with_index_provider(Arc::new(RocksDbIndexPlugin::new()))
    .with_source(source)
    .with_query(
        Query::cypher("my-query")
            .query("MATCH (n:Sensor) RETURN n")
            .from_source("sensors")
            .with_storage_backend(StorageBackendRef::Named("rocks".into()))
            .build()
    )
    .build()
    .await?;
```

---

## Query Builder

The `Query` builder creates query configurations with a fluent API.

### Creating Queries

```rust
// Cypher query (primary query language)
Query::cypher("my-query")
    .query("MATCH (n:Person) WHERE n.age > 21 RETURN n")
    .from_source("people-source")
    .build()

// GQL query (ISO 9074:2024 -- not GraphQL)
Query::gql("my-gql-query")
    .query("MATCH (n:Person) RETURN n.name")
    .from_source("people-source")
    .build()
```

### Query Builder Methods

| Method | Description | Default |
|--------|-------------|---------|
| `.query(str)` | Set the query string | Required |
| `.from_source(id)` | Subscribe to a source | Required |
| `.from_source_with_pipeline(id, pipeline)` | Subscribe with middleware pipeline | -- |
| `.with_middleware(config)` | Add middleware transformation | `[]` |
| `.auto_start(bool)` | Start automatically when `core.start()` is called | `true` |
| `.enable_bootstrap(bool)` | Load initial data from sources on start | `true` |
| `.with_bootstrap_buffer_size(size)` | Bootstrap buffer size | `10000` |
| `.with_joins(joins)` | Configure synthetic joins for multi-source queries | `None` |
| `.with_priority_queue_capacity(cap)` | Override queue capacity for this query | Inherited |
| `.with_dispatch_buffer_capacity(cap)` | Override buffer capacity for this query | Inherited |
| `.with_dispatch_mode(mode)` | Set dispatch mode (`Channel` or `Broadcast`) | `Channel` |
| `.with_storage_backend(ref)` | Use a specific storage backend | In-memory |
| `.build()` | Build the `QueryConfig` | -- |

### Multi-Source Queries with Joins

When a query spans multiple sources, use synthetic joins to define how elements relate:

```rust
use drasi_lib::config::{QueryJoinConfig, QueryJoinKey};

Query::cypher("cross-source")
    .query(r#"
        MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
        WHERE o.status = 'pending' AND o.total > 1000
        RETURN o.id, c.email, o.total
    "#)
    .from_source("orders-db")
    .from_source("customers-db")
    .with_joins(vec![QueryJoinConfig {
        id: "PLACED_BY".to_string(),
        keys: vec![
            QueryJoinKey { label: "Order".into(), property: "customer_id".into() },
            QueryJoinKey { label: "Customer".into(), property: "id".into() },
        ],
    }])
    .build()
```

---

## Runtime Management

After building, manage the DrasiLib instance with these methods:

### Lifecycle

```rust
core.start().await?;             // Start all auto-start components (Sources -> Queries -> Reactions)
core.stop().await?;              // Stop all components (Reactions -> Queries -> Sources)
let running = core.is_running().await;
```

### Individual Component Control

```rust
// Start / Stop
core.start_source("my-source").await?;
core.stop_source("my-source").await?;
core.start_query("my-query").await?;
core.stop_query("my-query").await?;
core.start_reaction("my-reaction").await?;
core.stop_reaction("my-reaction").await?;

// Add at runtime (auto-starts if server is running and auto_start=true)
core.add_source(new_source).await?;
core.add_query(query_config).await?;
core.add_reaction(new_reaction).await?;

// Hot-swap (preserves graph node, edges, and event history)
core.update_source("my-source", replacement_source).await?;
core.update_query("my-query", new_query_config).await?;
core.update_reaction("my-reaction", replacement_reaction).await?;

// Remove (cleanup=true calls deprovision() on the component)
core.remove_source("my-source", false).await?;
core.remove_query("my-query").await?;
core.remove_reaction("my-reaction", true).await?;
```

### Inspection

```rust
// List components with status
let sources: Vec<(String, ComponentStatus)> = core.list_sources().await?;
let queries = core.list_queries().await?;
let reactions = core.list_reactions().await?;

// Get individual component status
let status = core.get_source_status("my-source").await?;

// Get detailed component info
let info = core.get_source_info("my-source").await?;
let info = core.get_query_info("my-query").await?;
let info = core.get_reaction_info("my-reaction").await?;

// Get current query result set (snapshot)
let results: Vec<serde_json::Value> = core.get_query_results("my-query").await?;

// Get current configuration snapshot
let config = core.get_current_config().await?;
```

### Component Dependency Graph

```rust
// Full graph snapshot (serializable to JSON)
let snapshot: GraphSnapshot = core.get_graph().await;
println!("Components: {}", snapshot.nodes.len());

// Dependency queries
let dependents = core.get_dependents("orders-db").await;
let dependencies = core.get_dependencies("my-query").await;

// Check if safe to remove (fails if other components depend on it)
core.can_remove_component("orders-db").await?;
```

### Lifecycle Events

```rust
// Subscribe to ALL component events (global broadcast)
let mut receiver = core.subscribe_all_component_events();

// Subscribe to events for a specific component (history + live stream)
let (history, mut rx) = core.subscribe_source_events("my-source").await?;
let (history, mut rx) = core.subscribe_query_events("my-query").await?;
let (history, mut rx) = core.subscribe_reaction_events("my-reaction").await?;

// Process events
while let Ok(event) = rx.recv().await {
    println!("{}: {:?}", event.component_id, event.status);
}
```

---

## Core Concepts

### Sources

Sources ingest data from external systems and emit graph elements (nodes and relationships). They are trait-based plugins: you create source instances externally and pass them to DrasiLib.

**Available source plugins:** PostgreSQL, HTTP, gRPC, Mock, MSSQL, Platform, Application (see `components/sources/`).

### Queries

Continuous queries define what changes matter using Cypher or GQL. They produce three types of result changes:

| Change Type | Meaning |
|-------------|---------|
| **Added** | A new row matches the query criteria |
| **Updated** | An existing row still matches but values changed (includes before/after) |
| **Deleted** | A row no longer matches the query criteria |

**Limitation:** `ORDER BY`, `TOP`, and `LIMIT` are not supported in continuous queries.

### Reactions

Reactions receive query result changes and trigger actions (webhooks, logging, database writes, etc.).

**Available reaction plugins:** HTTP, gRPC, gRPC-Adaptive, SSE, Log, Platform, Profiler, StoredProc (PostgreSQL/MySQL/MSSQL), Application (see `components/reactions/`).

### Bootstrap Providers

Bootstrap providers deliver initial data to queries before streaming begins. When a query starts, it bootstraps from each source to build an initial result set, then processes live changes.

---

## Component Dependency Graph

DrasiLib maintains an internal component dependency graph backed by [petgraph](https://docs.rs/petgraph/). The graph is the **single source of truth** for all component metadata, relationships, runtime instances, and lifecycle events.

### Graph Structure

```
Instance ("my-app")
+-- Owns -> Source: "orders_db"
|             +-- Feeds -> Query: "active_orders"
+-- Owns -> Query: "active_orders"
|             +-- Feeds -> Reaction: "webhook"
+-- Owns -> Reaction: "webhook"
```

### Relationship Types

| From | Relationship | To | Reverse |
|------|-------------|-----|---------|
| Instance | Owns | Component | OwnedBy |
| Source | Feeds | Query | SubscribesTo |
| Query | Feeds | Reaction | SubscribesTo |
| BootstrapProvider | Bootstraps | Source | BootstrappedBy |
| IdentityProvider | Authenticates | Component | AuthenticatedBy |

### Component Status Lifecycle

```
Stopped --> Starting --> Running --> Stopping --> Stopped
               |            |            |
               v            v            v
             Error        Error        Error

Error --> Starting (retry) | Stopped (reset)
```

---

## Dispatch Modes

| Mode | Backpressure | Message Loss | Best For |
|------|-------------|--------------|----------|
| **Channel** (default) | Yes | None | Different subscriber speeds, critical data |
| **Broadcast** | No | Possible | High fanout, uniform speeds |

```rust
Query::cypher("my-query")
    .with_dispatch_mode(DispatchMode::Channel)    // Default
    .with_dispatch_mode(DispatchMode::Broadcast)  // Alternative
    .build()
```

---

## Logging

DrasiLib provides component-aware logging built on [tracing](https://docs.rs/tracing/). Logging is initialized automatically when you call `.build()`.

### Subscribing to Component Logs

```rust
let (history, mut receiver) = core.subscribe_source_logs("my-source").await?;
let (history, mut receiver) = core.subscribe_query_logs("my-query").await?;
let (history, mut receiver) = core.subscribe_reaction_logs("my-reaction").await?;

for log in &history {
    println!("[{}] {}: {}", log.level, log.component_id, log.message);
}

while let Ok(log) = receiver.recv().await {
    println!("[LIVE] {}: {}", log.component_id, log.message);
}
```

### Log Message Structure

```rust
pub struct LogMessage {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,            // Trace, Debug, Info, Warn, Error
    pub message: String,
    pub instance_id: String,        // DrasiLib instance that owns the component
    pub component_id: String,       // e.g., "my-source"
    pub component_type: ComponentType,  // Source, Query, or Reaction
}
```

Control verbosity with `RUST_LOG`: `RUST_LOG=info cargo run` (default), `RUST_LOG=debug cargo run`.

---

## State Store Providers

State store providers allow plugins to persist runtime state that survives restarts.

```rust
// Default: in-memory (no persistence)
let core = DrasiLib::builder().with_id("my-app").build().await?;

// Persistent: redb
use drasi_state_store_redb::RedbStateStoreProvider;
let core = DrasiLib::builder()
    .with_id("my-app")
    .with_state_store_provider(Arc::new(RedbStateStoreProvider::new("/data/state.redb")?))
    .build()
    .await?;
```

---

## Configuration via YAML

Queries can be defined in YAML and loaded programmatically:

```yaml
id: my-server
queries:
  - id: high-temp-alerts
    query: "MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s"
    queryLanguage: Cypher
    sources:
      - source_id: sensors
    auto_start: true
    enableBootstrap: true
```

```rust
let config: DrasiLibConfig = serde_yaml::from_str(&yaml_str)?;
config.validate()?;
let mut builder = DrasiLib::builder().with_id(&config.id);
for q in &config.queries { builder = builder.with_query(q.clone()); }
let core = builder.with_source(source).with_reaction(reaction).build().await?;
```

---

## Developer Guides

- **[Source Developer Guide](../components/sources/README.md)**
- **[Reaction Developer Guide](../components/reactions/README.md)**
- **[Bootstrap Provider Guide](../components/bootstrappers/README.md)**
- **[State Store Provider Guide](../components/state_stores/README.md)**

---

## License

Apache License 2.0

## Related Projects

- [Drasi](https://drasi.io/) -- Project documentation and guides
- [Drasi Platform](https://github.com/drasi-project/drasi-platform) -- Kubernetes deployment platform
- [Drasi Server](https://github.com/drasi-project/drasi-server) -- Single-process/Docker deployment
- [Drasi Core](https://github.com/drasi-project/drasi-core) -- Continuous query engine (this repo)
