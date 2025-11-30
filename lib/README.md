# DrasiLib

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

DrasiLib is a Rust library for building change-driven solutions that detect and react to data changes with precision.

## Overview

DrasiLib enables real-time change detection using **continuous queries** that maintain live result sets. Unlike traditional event-driven systems, you declare *what changes matter* using Cypher queries, and DrasiLib handles the complexity of tracking state and detecting meaningful transitions.

```
Sources → Continuous Queries → Reactions
   ↓              ↓                 ↓
Data In    Change Detection    Actions Out
```

**Key Benefits:**
- **Declarative**: Define change detection with Cypher queries, not custom code
- **Precise**: Get before/after states for every change, not just raw events
- **Pluggable**: Use existing source/reaction plugins or build your own
- **Scalable**: Built-in backpressure, priority queues, and dispatch modes

## Getting Started

The primary way to use DrasiLib is through the **Builder API**. The builder provides a fluent interface for configuring sources, queries, and reactions.

### Installation

```toml
[dependencies]
drasi-lib = { path = "path/to/drasi-lib" }
tokio = { version = "1", features = ["full"] }
```

### Basic Example

```rust
use drasi_lib::{DrasiLib, Query};
use drasi_plugin_mock::{MockSource, MockSourceConfig};
use drasi_plugin_log_reaction::{LogReaction, LogReactionConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Create source plugin instance
    let source = MockSource::new("sensors", MockSourceConfig {
        data_type: "sensor".to_string(),
        interval_ms: 1000,
    })?;

    // 2. Create reaction plugin instance
    let reaction = LogReaction::new(
        "alerts",
        vec!["high-temp".to_string()],
        LogReactionConfig::default(),
    );

    // 3. Build DrasiLib using the builder
    let core = DrasiLib::builder()
        .with_id("my-app")
        .with_source(source)      // Ownership transferred
        .with_reaction(reaction)  // Ownership transferred
        .add_query(
            Query::cypher("high-temp")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s")
                .from_source("sensors")
                .build()
        )
        .build()
        .await?;

    // 4. Start processing
    core.start().await?;

    // Run until shutdown
    tokio::signal::ctrl_c().await?;
    core.stop().await?;

    Ok(())
}
```

## Builder API Reference

### DrasiLibBuilder

Create a builder with `DrasiLib::builder()` and configure your instance using the fluent API.

#### `with_id(id: impl Into<String>)`

Set a unique identifier for this DrasiLib instance. Used for logging and debugging.

```rust
DrasiLib::builder()
    .with_id("my-application")
```

#### `with_source(source: impl Source + 'static)`

Add a source plugin instance. **Ownership is transferred** to DrasiLib - you cannot use the source after calling this method.

```rust
let source = MockSource::new("my-source", config)?;
DrasiLib::builder()
    .with_source(source)  // source is moved here
```

You can add multiple sources by chaining:

```rust
DrasiLib::builder()
    .with_source(source1)
    .with_source(source2)
    .with_source(source3)
```

#### `with_reaction(reaction: impl Reaction + 'static)`

Add a reaction plugin instance. **Ownership is transferred** to DrasiLib.

```rust
let reaction = LogReaction::new("my-reaction", vec!["query1".into()], config);
DrasiLib::builder()
    .with_reaction(reaction)  // reaction is moved here
```

#### `add_query(config: QueryConfig)`

Add a query configuration. Use the `Query` builder to create configurations:

```rust
DrasiLib::builder()
    .add_query(
        Query::cypher("my-query")
            .query("MATCH (n:Person) RETURN n")
            .from_source("my-source")
            .build()
    )
```

#### `with_priority_queue_capacity(capacity: usize)`

Set the default priority queue capacity for all components. Controls how many events can be buffered before backpressure is applied.

```rust
DrasiLib::builder()
    .with_priority_queue_capacity(50000)  // Default: 10000
```

#### `with_dispatch_buffer_capacity(capacity: usize)`

Set the default dispatch buffer capacity for event routing channels.

```rust
DrasiLib::builder()
    .with_dispatch_buffer_capacity(5000)  // Default: 1000
```

#### `add_storage_backend(config: StorageBackendConfig)`

Add a storage backend for persistent query state (RocksDB, Redis/Garnet).

```rust
DrasiLib::builder()
    .add_storage_backend(StorageBackendConfig::rocksdb("./data"))
```

#### `build() -> Result<DrasiLib>`

Build the DrasiLib instance. This validates configuration, creates all components, and initializes the system. The instance is ready to start after building.

```rust
let core = DrasiLib::builder()
    .with_id("my-app")
    .with_source(source)
    .add_query(query)
    .build()
    .await?;

// Now start processing
core.start().await?;
```

### Query Builder

The `Query` builder creates query configurations with a fluent API.

#### `Query::cypher(id: impl Into<String>)`

Create a new Cypher query builder:

```rust
Query::cypher("my-query")
    .query("MATCH (n:Person) WHERE n.age > 21 RETURN n")
    .from_source("people-source")
    .build()
```

#### `Query::gql(id: impl Into<String>)`

Create a new GQL (GraphQL) query builder:

```rust
Query::gql("my-gql-query")
    .query("MATCH (n:Person) RETURN n.name")
    .from_source("people-source")
    .build()
```

#### Query Builder Methods

| Method | Description | Default |
|--------|-------------|---------|
| `.query(str)` | Set the query string | Required |
| `.from_source(id)` | Subscribe to a source | Required (at least one) |
| `.from_source_with_pipeline(id, pipeline)` | Subscribe with middleware pipeline | - |
| `.with_middleware(config)` | Add middleware transformation | `[]` |
| `.auto_start(bool)` | Start automatically with DrasiLib | `true` |
| `.enable_bootstrap(bool)` | Load initial data from sources | `true` |
| `.with_bootstrap_buffer_size(size)` | Bootstrap buffer size | `10000` |
| `.with_joins(joins)` | Configure synthetic joins | None |
| `.with_priority_queue_capacity(cap)` | Override queue capacity | Inherited |
| `.with_dispatch_buffer_capacity(cap)` | Override buffer capacity | Inherited |
| `.with_dispatch_mode(mode)` | Set dispatch mode | `Channel` |
| `.with_storage_backend(ref)` | Use specific storage backend | Default |

#### Complete Query Example

```rust
Query::cypher("complex-query")
    .query(r#"
        MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
        WHERE o.status = 'pending' AND o.total > 1000
        RETURN o.id, c.email, o.total
    "#)
    .from_source("orders")
    .from_source("customers")
    .auto_start(true)
    .enable_bootstrap(true)
    .with_priority_queue_capacity(20000)
    .with_dispatch_mode(DispatchMode::Channel)
    .build()
```

## Alternative: Configuration Files

While the builder API is the primary approach, DrasiLib can also load query configurations from YAML files. Note that **sources and reactions must still be added programmatically**.

```rust
// Load queries from YAML, add sources/reactions programmatically
let core = DrasiLib::from_config_file("config.yaml").await?;

let source = MySource::new("sensors", config)?;
let reaction = MyReaction::new("alerts", vec!["query1".into()]);

core.add_source(source).await?;
core.add_reaction(reaction).await?;

core.start().await?;
```

Example `config.yaml`:

```yaml
server_core:
  id: my-app
  priority_queue_capacity: 10000
  dispatch_buffer_capacity: 1000

queries:
  - id: high-temp
    query: "MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s"
    source_subscriptions:
      - source_id: sensors
    enable_bootstrap: true
    auto_start: true
```

## Runtime Management

After building, use these methods to manage the DrasiLib instance:

```rust
// Lifecycle
core.start().await?;           // Start all components
core.stop().await?;            // Stop all components

// Component control
core.start_source("my-source").await?;
core.stop_source("my-source").await?;
core.start_query("my-query").await?;
core.stop_query("my-query").await?;
core.start_reaction("my-reaction").await?;
core.stop_reaction("my-reaction").await?;

// Add components at runtime
core.add_source(new_source).await?;
core.add_reaction(new_reaction).await?;

// Remove components
core.remove_source("my-source").await?;
core.remove_query("my-query").await?;
core.remove_reaction("my-reaction").await?;

// Inspection
let sources = core.list_sources().await?;
let queries = core.list_queries().await?;
let reactions = core.list_reactions().await?;
let status = core.get_source_status("my-source").await?;
```

## Core Concepts

### Sources

Sources ingest data from external systems and emit graph elements (nodes and relationships). DrasiLib provides a trait-based plugin architecture.

**Available Source Plugins:** See `components/sources/` for PostgreSQL, HTTP, gRPC, Mock, Platform, and Application sources.

### Queries

Queries define what changes matter using Cypher. They track **three types of results:**
- `Adding` - New rows matching the query
- `Updating` - Existing rows with changed values
- `Removing` - Rows that no longer match

**Limitation:** ORDER BY, TOP, and LIMIT are not supported in continuous queries.

### Reactions

Reactions respond to query results by triggering actions (webhooks, logging, etc.).

**Available Reaction Plugins:** See `components/reactions/` for HTTP, gRPC, SSE, Log, and Application reactions.

### Bootstrap Providers

Bootstrap providers deliver initial data to queries before streaming begins. Any source can use any bootstrap provider.

## Dispatch Modes

DrasiLib supports two dispatch modes for event routing:

| Mode | Backpressure | Message Loss | Best For |
|------|--------------|--------------|----------|
| **Channel** (default) | Yes | None | Different subscriber speeds, critical data |
| **Broadcast** | No | Possible | High fanout (10+ subscribers), uniform speeds |

## Developer Guides

For detailed information on building plugins:

- **[Source Developer Guide](../components/sources/README.md)** - Creating custom data sources
- **[Reaction Developer Guide](../components/reactions/README.md)** - Creating custom reactions
- **[Bootstrap Provider Guide](../components/bootstrap/README.md)** - Creating bootstrap providers

## Troubleshooting

### "Source/Reaction not found"

Ensure you've added the instance to DrasiLib before referencing it in queries:

```rust
let source = MySource::new("my-source", config)?;
let core = DrasiLib::builder()
    .with_source(source)  // Must add source before queries reference it
    .add_query(
        Query::cypher("my-query")
            .from_source("my-source")  // References the source above
            .build()
    )
    .build()
    .await?;
```

### Events not being received

- **Broadcast mode**: May lose events if receivers are slow
- **Channel mode**: Provides backpressure but may block senders
- Check `dispatch_buffer_capacity` settings

### Debug Logging

```rust
std::env::set_var("RUST_LOG", "drasi_lib=debug");
env_logger::init();
```

## License

Apache License 2.0

## Related Projects

- [Drasi Platform](https://github.com/drasi-project/drasi-platform) - Complete deployment platform
- [Drasi Core](https://github.com/drasi-project/drasi-core) - Continuous query engine
