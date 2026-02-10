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

### Installation

```toml
[dependencies]
drasi-lib = "0.3"
tokio = { version = "1", features = ["full"] }
```

### Middleware Features (Optional)

DrasiLib supports optional middleware for data transformation. **All middleware are disabled by default.**

Enable only the middleware you need:

```toml
[dependencies]
drasi-lib = { version = "0.3", features = ["middleware-jq", "middleware-decoder"] }
```

**Available Middleware Features:**

- **`middleware-jq`** - JQ query language transformations (requires build tools, see below)
- **`middleware-decoder`** - Decode encoded strings (base64, hex, URL encoding)
- **`middleware-map`** - JSONPath-based property mapping
- **`middleware-parse-json`** - Parse JSON strings into structured objects
- **`middleware-promote`** - Promote nested properties to top level
- **`middleware-relabel`** - Transform element labels
- **`middleware-unwind`** - Unwind arrays into multiple elements
- **`middleware-all`** - Enable all middleware (convenience feature)

**JQ Middleware Build Requirements:**

The `middleware-jq` feature compiles jq from source and requires build tools:

**macOS:**
```bash
brew install autoconf automake libtool
```

**Ubuntu/Debian:**
```bash
sudo apt-get install autoconf automake libtool flex bison
```

**Note:** If you don't use middleware, or only use non-jq middleware, you don't need these build tools.

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
use drasi_source_mock::{MockSource, MockSourceConfig};
use drasi_reaction_log::{LogReaction, LogReactionConfig};

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
        .with_query(
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

### DrasiLibBuilder API Reference

Create a builder with `DrasiLib::builder()` and configure using the fluent API.

#### `with_id(id: impl Into<String>)`

Set a unique identifier for this DrasiLib instance. Used for logging and debugging.

```rust
DrasiLib::builder()
    .with_id("my-application")
```

#### `with_source(source: impl Source + 'static)`

Add a source plugin instance. **Ownership is transferred** to DrasiLib.

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

#### `with_query(config: QueryConfig)`

Add a query configuration. Use the `Query` builder to create configurations:

```rust
DrasiLib::builder()
    .with_query(
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
use drasi_lib::StorageBackendConfig;

DrasiLib::builder()
    .add_storage_backend(StorageBackendConfig {
        id: "rocksdb-backend".to_string(),
        spec: StorageBackendSpec::RocksDb { path: "./data".to_string() },
    })
```

#### `build() -> Result<DrasiLib>`

Build the DrasiLib instance. This validates configuration, creates all components, and initializes the system.

```rust
let core = DrasiLib::builder()
    .with_id("my-app")
    .with_source(source)
    .with_query(query)
    .build()
    .await?;

// Now start processing
core.start().await?;
```

---

## Method 2: Config Struct Initialization

For scenarios where you need to load configuration from YAML/JSON files or construct configuration programmatically without the builder.

### DrasiLibConfig Structure

```rust
use drasi_lib::{DrasiLibConfig, QueryConfig, RuntimeConfig};
use std::sync::Arc;

// Create configuration directly
let config = DrasiLibConfig {
    id: "my-server".to_string(),
    priority_queue_capacity: Some(50000),
    dispatch_buffer_capacity: Some(5000),
    storage_backends: vec![],
    queries: vec![
        QueryConfig {
            id: "my-query".to_string(),
            query: "MATCH (n:Person) RETURN n".to_string(),
            query_language: QueryLanguage::Cypher,
            sources: vec![
                SourceSubscriptionConfig {
                    source_id: "my-source".to_string(),
                    pipeline: vec![],
                }
            ],
            middleware: vec![],
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,  // Inherits from global
            dispatch_buffer_capacity: None, // Inherits from global
            dispatch_mode: None,
            storage_backend: None,
        },
    ],
};

// Validate the configuration
config.validate()?;
```

### YAML Configuration Format

```yaml
# DrasiLibConfig structure
id: my-server
priority_queue_capacity: 50000      # Optional, default: 10000
dispatch_buffer_capacity: 5000      # Optional, default: 1000

storage_backends:                    # Optional
  - id: rocksdb-backend
    spec:
      type: rocksdb
      path: ./data

queries:
  - id: high-temp-alerts
    query: |
      MATCH (s:Sensor)
      WHERE s.temperature > 75
      RETURN s.id, s.temperature, s.location
    queryLanguage: Cypher           # Optional, default: Cypher
    sources:
      - source_id: sensors
        pipeline: []                # Optional middleware pipeline
    auto_start: true                # Optional, default: true
    enableBootstrap: true           # Optional, default: true
    bootstrapBufferSize: 10000      # Optional, default: 10000

  - id: complex-join-query
    query: |
      MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
      WHERE o.status = 'pending'
      RETURN o.id, c.email, o.total
    sources:
      - source_id: orders
      - source_id: customers
    joins:                          # Synthetic joins for cross-source queries
      - id: PLACED_BY
        keys:
          - label: Order
            property: customer_id
          - label: Customer
            property: id
```

### Loading from YAML and Initializing

```rust
use drasi_lib::{DrasiLibConfig, RuntimeConfig};
use std::sync::Arc;

// Load YAML configuration
let yaml_str = std::fs::read_to_string("config.yaml")?;
let config: DrasiLibConfig = serde_yaml::from_str(&yaml_str)?;

// Validate configuration
config.validate()?;

// Convert to RuntimeConfig (applies defaults to queries)
let runtime_config = Arc::new(RuntimeConfig::from(config));

// Note: DrasiLib::new() is internal. For config-based initialization,
// use the builder pattern and add sources/reactions programmatically:

let mut core = DrasiLib::builder()
    .with_id(&runtime_config.id);

// Add queries from config
for query_config in &runtime_config.queries {
    core = core.with_query(query_config.clone());
}

// Add sources and reactions as plugin instances
// (Sources/reactions must be created from plugin types - they cannot be in YAML)
let source = create_source_from_external_config()?;
let reaction = create_reaction_from_external_config()?;

let core = core
    .with_source(source)
    .with_reaction(reaction)
    .build()
    .await?;

core.start().await?;
```

### Configuration Fields Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | `String` | UUID | Unique identifier for this instance |
| `priority_queue_capacity` | `Option<usize>` | `10000` | Default event queue capacity |
| `dispatch_buffer_capacity` | `Option<usize>` | `1000` | Default channel buffer capacity |
| `storage_backends` | `Vec<StorageBackendConfig>` | `[]` | Storage backend definitions |
| `queries` | `Vec<QueryConfig>` | `[]` | Query configurations |

### QueryConfig Fields Reference

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `id` | `String` | Required | Unique query identifier |
| `query` | `String` | Required | Cypher or GQL query string |
| `query_language` | `QueryLanguage` | `Cypher` | Query language (`Cypher` or `GQL`) |
| `sources` | `Vec<SourceSubscriptionConfig>` | `[]` | Source subscriptions |
| `middleware` | `Vec<SourceMiddlewareConfig>` | `[]` | Middleware configurations |
| `auto_start` | `bool` | `true` | Start automatically with DrasiLib |
| `joins` | `Option<Vec<QueryJoinConfig>>` | `None` | Synthetic join definitions |
| `enable_bootstrap` | `bool` | `true` | Load initial data from sources |
| `bootstrap_buffer_size` | `usize` | `10000` | Bootstrap buffer size |
| `priority_queue_capacity` | `Option<usize>` | Inherited | Override queue capacity |
| `dispatch_buffer_capacity` | `Option<usize>` | Inherited | Override buffer capacity |
| `dispatch_mode` | `Option<DispatchMode>` | `Channel` | Event dispatch mode |
| `storage_backend` | `Option<StorageBackendRef>` | Default | Storage backend reference |

---

## Query Builder

The `Query` builder creates query configurations with a fluent API.

### `Query::cypher(id: impl Into<String>)`

Create a new Cypher query builder:

```rust
Query::cypher("my-query")
    .query("MATCH (n:Person) WHERE n.age > 21 RETURN n")
    .from_source("people-source")
    .build()
```

### `Query::gql(id: impl Into<String>)`

Create a new GQL (GraphQL) query builder:

```rust
Query::gql("my-gql-query")
    .query("MATCH (n:Person) RETURN n.name")
    .from_source("people-source")
    .build()
```

### Query Builder Methods

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

### Complete Query Example

```rust
use drasi_lib::{Query, DispatchMode};

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

---

## Runtime Management

After building, use these methods to manage the DrasiLib instance:

```rust
// Lifecycle
core.start().await?;           // Start all auto-start components
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
core.add_query(query_config).await?;

// Remove components
core.remove_source("my-source").await?;
core.remove_query("my-query").await?;
core.remove_reaction("my-reaction").await?;

// Inspection
let sources = core.list_sources().await?;
let queries = core.list_queries().await?;
let reactions = core.list_reactions().await?;
let status = core.get_source_status("my-source").await?;
let results = core.get_query_results("my-query").await?;

// Check running state
let is_running = core.is_running().await;

// Get current configuration
let config = core.get_current_config().await?;
```

---

## Core Concepts

### Sources

Sources ingest data from external systems and emit graph elements (nodes and relationships). DrasiLib provides a trait-based plugin architecture.

**Important**: Sources are plugins that must be instantiated externally and passed to DrasiLib. DrasiLib has no awareness of which source plugins exist.

**Available Source Plugins:** See `components/sources/` for PostgreSQL, HTTP, gRPC, Mock, Platform, and Application sources.

### Queries

Queries define what changes matter using Cypher. They track **three types of results:**
- `Adding` - New rows matching the query
- `Updating` - Existing rows with changed values
- `Removing` - Rows that no longer match

**Limitation:** ORDER BY, TOP, and LIMIT are not supported in continuous queries.

### Reactions

Reactions respond to query results by triggering actions (webhooks, logging, etc.).

**Important**: Like sources, reactions are plugins that must be instantiated externally.

**Available Reaction Plugins:** See `components/reactions/` for HTTP, gRPC, SSE, Log, Platform, and Profiler reactions.

### Bootstrap Providers

Bootstrap providers deliver initial data to queries before streaming begins. Any source can use any bootstrap provider, enabling flexible scenarios like "bootstrap from database, stream changes via HTTP."

---

## Dispatch Modes

DrasiLib supports two dispatch modes for event routing:

| Mode | Backpressure | Message Loss | Best For |
|------|--------------|--------------|----------|
| **Channel** (default) | Yes | None | Different subscriber speeds, critical data |
| **Broadcast** | No | Possible | High fanout (10+ subscribers), uniform speeds |

Configure per-query:

```rust
use drasi_lib::DispatchMode;

Query::cypher("my-query")
    .with_dispatch_mode(DispatchMode::Channel)  // Default
    // or
    .with_dispatch_mode(DispatchMode::Broadcast)
    .build()
```

---

## Developer Guides

For detailed information on building plugins:

- **[Source Developer Guide](../components/sources/README.md)** - Creating custom data sources
- **[Reaction Developer Guide](../components/reactions/README.md)** - Creating custom reactions
- **[Bootstrap Provider Guide](../components/bootstrappers/README.md)** - Creating bootstrap providers
- **[State Store Provider Guide](../components/state_stores/README.md)** - Creating state store providers

---

## State Store Providers

State store providers allow plugins (Sources, BootstrapProviders, and Reactions) to persist runtime state that survives restarts of DrasiLib.

### Default (Memory) Provider

By default, DrasiLib uses an in-memory state store that does not persist across restarts:

```rust
let core = DrasiLib::builder()
    .with_id("my-app")
    .build()  // Uses MemoryStateStoreProvider by default
    .await?;
```

### Redb Provider

For ACID-compliant persistent storage, use the redb state store provider:

```rust
use drasi_state_store_redb::RedbStateStoreProvider;
use std::sync::Arc;

let state_store = RedbStateStoreProvider::new("/data/state.redb")?;
let core = DrasiLib::builder()
    .with_id("my-app")
    .with_state_store_provider(Arc::new(state_store))
    .build()
    .await?;
```

### Using State Store in Plugins

When a Source or Reaction is added to DrasiLib, a runtime context is provided via the `initialize()` method. The context contains all DrasiLib-provided services, including the state store (if configured):

```rust
use drasi_lib::{Source, SourceRuntimeContext};

#[async_trait]
impl Source for MySource {
    async fn initialize(&self, context: SourceRuntimeContext) {
        // Store the context for later use
        self.base.initialize(context).await;

        // Access state store from context (may be None if not configured)
        if let Some(state_store) = self.base.state_store().await {
            // Use state_store for persistent state
        }
    }
}
```

For reactions, use `ReactionRuntimeContext` which also provides access to `QueryProvider`:

```rust
use drasi_lib::{Reaction, ReactionRuntimeContext};

#[async_trait]
impl Reaction for MyReaction {
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;

        // Access query provider for subscribing to query results
        let query_provider = self.base.query_provider().await;
    }
}
```

See the **[State Store Provider Guide](../components/state_stores/README.md)** for full documentation.

---

## Troubleshooting

### "Source/Reaction not found"

Ensure you've added the instance to DrasiLib before referencing it in queries:

```rust
let source = MySource::new("my-source", config)?;
let core = DrasiLib::builder()
    .with_source(source)  // Must add source before queries reference it
    .with_query(
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
- Check `dispatch_buffer_capacity` and `priority_queue_capacity` settings

---

## Logging

DrasiLib provides a component-aware logging system that routes logs to per-component streams, enabling you to subscribe to and monitor logs from specific sources, queries, and reactions.

### Initializing Logging

**Important:** You must initialize DrasiLib's logging **before** any other logger is set up in your application.

#### Option 1: Default Logger (stderr output)

```rust
use drasi_lib::init_logging;

fn main() {
    // Initialize DrasiLib logging first, before any other logger
    init_logging();
    
    // Now create and use DrasiLib...
}
```

#### Option 2: Specify Log Level

```rust
use drasi_lib::init_logging_with_level;

fn main() {
    init_logging_with_level(log::LevelFilter::Debug);
    
    // Now create and use DrasiLib...
}
```

#### Option 3: Wrap Your Own Logger

If you need custom log formatting, file output, or other features from your preferred logging framework, you can wrap your logger with DrasiLib's component-aware logging:

```rust
use drasi_lib::init_logging_with_logger;

fn main() {
    // Create your logger but DON'T install it directly
    let my_logger = env_logger::Builder::from_default_env()
        .format_timestamp_millis()
        .build();
    
    // Let DrasiLib wrap it with component logging support
    init_logging_with_logger(my_logger, log::LevelFilter::Debug);
    
    // Now both your logger AND component log streaming work!
}
```

### Non-Panicking Variants

If you need to handle initialization failures gracefully:

```rust
use drasi_lib::{try_init_logging, try_init_logging_with_logger};

// Returns Err if another logger is already set
if let Err(e) = try_init_logging() {
    eprintln!("Warning: Could not initialize Drasi logging: {}", e);
}
```

### Subscribing to Component Logs

Once DrasiLib is running, you can subscribe to logs from specific components:

```rust
// Subscribe to source logs
let (history, mut receiver) = core.subscribe_source_logs("my-source").await?;

// `history` contains recent log messages
for log in history {
    println!("[{}] {}: {}", log.level, log.component_id, log.message);
}

// `receiver` streams new log messages in real-time
while let Ok(log) = receiver.recv().await {
    println!("[LIVE] {}: {}", log.level, log.message);
}
```

Similar methods exist for queries and reactions:

```rust
let (history, receiver) = core.subscribe_query_logs("my-query").await?;
let (history, receiver) = core.subscribe_reaction_logs("my-reaction").await?;
```

### How Component Logging Works

1. **Standard `log::info!()`, `log::debug!()`, etc. macros work automatically** within component contexts
2. When code runs inside a source, query, or reaction task, logs are routed to that component's log stream
3. Logs are also forwarded to your logger (or stderr) for normal output
4. Subscribers receive logs in real-time via broadcast channels

### Log Message Structure

```rust
pub struct LogMessage {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,        // Trace, Debug, Info, Warn, Error
    pub message: String,
    pub component_id: String,   // e.g., "my-source", "my-query"
    pub component_type: ComponentType,  // Source, Query, or Reaction
}
```

---

## License

Apache License 2.0

## Related Projects

- [Drasi Platform](https://github.com/drasi-project/drasi-platform) - Complete deployment platform
- [Drasi Core](https://github.com/drasi-project/drasi-core) - Continuous query engine
