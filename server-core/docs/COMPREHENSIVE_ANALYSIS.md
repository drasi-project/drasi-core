# DrasiServerCore Comprehensive Codebase Analysis

## Executive Summary

DrasiServerCore is a ~41K line Rust library that implements a reactive, event-driven architecture for continuous data change processing. It provides the foundational components for real-time change detection and response through a **Source → Query → Reaction** pattern using graph-based queries (Cypher/GQL).

**Key Characteristics**:
- Library-only design (no standalone binary)
- Async/await with Tokio for concurrency
- Pluggable bootstrap provider system (independent from sources)
- Type-safe fluent builder API
- Support for YAML/JSON configuration files
- Thread-safe and cloneable components
- ~41,000 lines of production Rust code
- Extensive test coverage with integration tests

---

## 1. Main Source Code Structure

```
server-core/src/
├── lib.rs                  # Public API surface re-exports
├── application.rs          # ApplicationHandle for app integration
├── server_core.rs          # DrasiServerCore main struct (400+ lines)
├── api/                    # Public builder/fluent API (800+ lines total)
│   ├── builder.rs         # DrasiServerCoreBuilder
│   ├── source.rs          # SourceBuilder
│   ├── query.rs           # QueryBuilder
│   ├── reaction.rs        # ReactionBuilder
│   ├── handles.rs         # HandleRegistry
│   ├── properties.rs      # Properties type
│   ├── error.rs           # DrasiError, Result
│   └── *_test.rs          # Unit tests for each
├── config/                 # Configuration management (400+ lines)
│   ├── schema.rs          # YAML/JSON configuration types
│   ├── typed.rs           # Type-safe config variants
│   ├── runtime.rs         # Runtime config wrapping
│   └── *_test.rs          # Config validation tests
├── sources/               # Data ingestion layer (2000+ lines)
│   ├── mod.rs            # Publisher trait, source exports
│   ├── base.rs           # SourceBase (common functionality)
│   ├── manager.rs        # SourceManager, Source trait
│   ├── application/      # ApplicationSource (programmatic injection)
│   ├── postgres/         # PostgreSQL replication source (600+ lines)
│   ├── http/             # HTTP endpoint source
│   ├── grpc/             # gRPC source
│   ├── mock/             # Mock data generator
│   ├── platform/         # Redis Streams integration
│   └── *_test.rs         # Source tests
├── queries/              # Continuous query processing (1500+ lines)
│   ├── mod.rs           # Query exports
│   ├── base.rs          # QueryBase (common functionality)
│   ├── manager.rs       # QueryManager, Query trait (400+ lines)
│   ├── label_extractor.rs # Extract labels from queries
│   ├── priority_queue.rs   # Timestamp-ordered priority queue
│   └── *_test.rs        # Query tests
├── reactions/            # Output destination layer (2000+ lines)
│   ├── mod.rs           # Reaction exports
│   ├── base.rs          # ReactionBase (common functionality)
│   ├── manager.rs       # ReactionManager, Reaction trait
│   ├── application/     # ApplicationReaction (programmatic consumption)
│   ├── http/            # HTTP webhook delivery
│   ├── grpc/            # gRPC streaming delivery
│   ├── http_adaptive.rs # Adaptive HTTP reaction
│   ├── grpc_adaptive.rs # Adaptive gRPC reaction
│   ├── log/             # Debug logging reaction
│   ├── platform/        # Redis Streams publishing
│   ├── profiler/        # Performance profiling reaction
│   ├── sse/             # Server-Sent Events reaction
│   └── *_test.rs        # Reaction tests
├── bootstrap/            # Bootstrap provider system (600+ lines)
│   ├── mod.rs           # BootstrapProvider trait, BootstrapContext
│   ├── script_reader.rs # JSONL file parsing
│   ├── script_types.rs  # Script record types
│   └── providers/       # Bootstrap implementations
│       ├── mod.rs
│       ├── noop.rs      # No-op provider (default)
│       ├── postgres.rs  # PostgreSQL snapshot bootstrap
│       ├── script_file.rs # JSONL file bootstrap
│       ├── application.rs # Application event replay
│       ├── platform.rs  # Remote Query API bootstrap
│       └── *_test.rs
├── channels/             # Async event routing (600+ lines)
│   ├── mod.rs           # Channel exports
│   ├── dispatcher.rs    # ChangeDispatcher trait (Broadcast/Channel modes)
│   ├── events.rs        # Event types (SourceEventWrapper, QueryResult, etc.)
│   ├── priority_queue.rs # Priority queue implementation
│   └── *_test.rs        # Event tests
├── routers/              # Event routing (mostly removed)
├── utils/                # Utilities (logging, adaptive batching)
├── profiling/            # Performance profiling metadata
└── test_support/         # Test utilities (Redis helpers, etc.)
```

**Total Lines**: ~41,000 lines of Rust code
**Modules**: 16+ major modules with clear separation of concerns
**Tests**: Extensive unit and integration tests throughout

---

## 2. Key Abstractions and Components

### 2.1 Core Abstraction: Source → Query → Reaction

```
Data Source          Continuous Query      Reaction
    ↓                      ↓                   ↓
[Change Events] → [Pattern Matching] → [Action Trigger]
```

Every component (Source, Query, Reaction) has:
- Independent lifecycle (Stopped → Starting → Running → Stopping)
- Configuration (SourceConfig, QueryConfig, ReactionConfig)
- Status tracking (ComponentStatus enum)
- Event channel for lifecycle events

### 2.2 Source Implementations

**All sources extend `SourceBase` and implement the `Source` trait**:

1. **ApplicationSource** - Programmatic event injection
   - Used when application code directly feeds data
   - Provides `ApplicationSourceHandle` for type-safe event building
   - Methods: `send_node_insert()`, `send_node_update()`, `send_relation_insert()`, etc.

2. **PostgresReplicationSource** - WAL-based change capture
   - Connects to PostgreSQL replication slots
   - Decodes logical replication stream
   - Supports SSL/SCRAM authentication
   - 600+ lines of protocol handling

3. **HttpSource** - HTTP endpoint for incoming changes
   - Receives POST requests with change events
   - Parses JSON/custom formats
   - Includes adaptive version for performance tuning

4. **GrpcSource** - gRPC service endpoint
   - Bidirectional streaming with gRPC
   - Protocol buffer based
   - Handles backpressure

5. **PlatformSource** - Redis Streams integration
   - Consumes from Redis Streams using consumer groups
   - Enables federation with external Drasi Platform
   - Exactly-once delivery semantics

6. **MockSource** - Built-in test data generator
   - Generates synthetic sensor/counter data
   - Configurable interval and data type
   - Perfect for examples and testing

### 2.3 Query Processing

**QueryManager** manages all queries; each implements `Query` trait:

- **Continuous Query Processing**:
  - Subscribes to one or more sources
  - Receives SourceChangeEvents from sources
  - Evaluates Cypher/GQL against in-memory graph
  - Detects result deltas (what changed)
  - Dispatches changed results to subscribers (reactions)

- **Key Features**:
  - Bootstrap support (send initial data on subscription)
  - Multi-source joins
  - Priority queue for timestamp-ordered delivery
  - Label extraction for bootstrap filtering
  - Error handling and recovery

- **Limitations**:
  - ORDER BY, TOP, LIMIT clauses NOT supported
  - Designed for change-driven, not result-driven queries

### 2.4 Reaction Implementations

**ReactionManager** manages all reactions; each implements `Reaction` trait:

1. **ApplicationReaction** - Programmatic result consumption
   - Provides `ApplicationReactionHandle`
   - Async subscription channels
   - Custom `SubscriptionOptions`

2. **HttpReaction** - POST results to webhooks
   - Configurable endpoint URL
   - Request formatting options
   - Retry logic

3. **AdaptiveHttpReaction** - Intelligent HTTP delivery
   - Auto-tunes batching and timing
   - Performance optimization

4. **GrpcReaction** - Stream results via gRPC
   - Bidirectional communication
   - Protocol buffer serialization

5. **AdaptiveGrpcReaction** - Intelligent gRPC delivery
   - Dynamic batching

6. **SseReaction** - Server-Sent Events
   - Browser-based real-time subscriptions

7. **PlatformReaction** - Redis Streams publishing
   - Publishes results back to Redis Streams
   - Enables reaction chaining

8. **LogReaction** - Debug logging
   - Configurable log level
   - Useful for examples and testing

9. **ProfilerReaction** - Performance metrics
   - Collects timing data
   - Profiling information

### 2.5 Bootstrap Provider System

**Universal pluggable system** - ANY source can use ANY bootstrap provider:

```
Source Config → Bootstrap Provider Factory → BootstrapProvider
                                                   ↓
                                          Deliver initial data
```

**All Sources Support Bootstrap**:
- PostgresReplicationSource ✓
- HttpSource ✓
- GrpcSource ✓
- MockSource ✓
- PlatformSource ✓
- ApplicationSource ✓

**Bootstrap Provider Types**:

1. **NoOpProvider** (default)
   - Returns no data
   - Used when no bootstrap needed

2. **PostgresBootstrapProvider**
   - Reads full table snapshot
   - LSN coordination for consistent bootstrap
   - Supports multiple tables with key columns

3. **ScriptFileBootstrapProvider**
   - Reads JSONL (JSON Lines) files
   - Supports Node, Relation, Header records
   - Multi-file processing in sequence
   - Best for testing and development

4. **PlatformBootstrapProvider**
   - Fetches data from remote Drasi Query API
   - HTTP streaming
   - Enables cross-Drasi federation

5. **ApplicationBootstrapProvider**
   - Replays stored insert events
   - Internal use by ApplicationSource

**Key Architectural Principle**: 
Bootstrap is completely separated from streaming. You can bootstrap from PostgreSQL but stream changes via HTTP, or vice versa.

---

## 3. Public API Surface

### 3.1 Main Entry Point: DrasiServerCore

Located in `src/server_core.rs`:

```rust
pub struct DrasiServerCore {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    running: Arc<RwLock<bool>>,
    handles_registry: Arc<HandleRegistry>,
}
```

**Creation Methods**:
```rust
// Builder API
DrasiServerCore::builder()
    .with_id("server-id")
    .add_source(Source::application("events").build())
    .add_query(Query::cypher("q1").query("...").from_source("events").build())
    .add_reaction(Reaction::log("r1").subscribe_to("q1").build())
    .build()
    .await?

// From config file
DrasiServerCore::from_config_file("config.yaml").await?

// From config string
DrasiServerCore::from_config_str(yaml_str).await?
```

**Lifecycle Methods**:
```rust
core.start().await?
core.stop().await?
core.is_running().await -> bool
```

**Component Management**:
```rust
// Get handles for programmatic interaction
core.source_handle("id")? -> ApplicationSourceHandle
core.reaction_handle("id")? -> ApplicationReactionHandle

// Dynamic component management
core.create_source(SourceConfig).await?
core.create_query(QueryConfig).await?
core.create_reaction(ReactionConfig).await?

core.start_source("id").await?
core.stop_source("id").await?
core.remove_source("id").await?

core.start_query("id").await?
core.stop_query("id").await?
core.remove_query("id").await?

core.start_reaction("id").await?
core.stop_reaction("id").await?
core.remove_reaction("id").await?
```

**Monitoring**:
```rust
core.get_source_status("id").await? -> ComponentStatus
core.get_query_status("id").await? -> ComponentStatus
core.get_reaction_status("id").await? -> ComponentStatus

core.get_source_info("id").await? -> SourceRuntime
core.get_query_info("id").await? -> QueryRuntime
core.get_reaction_info("id").await? -> ReactionRuntime

core.list_sources().await? -> Vec<SourceRuntime>
core.list_queries().await? -> Vec<QueryRuntime>
core.list_reactions().await? -> Vec<ReactionRuntime>
```

### 3.2 Builder Types (Fluent API)

Located in `src/api/`:

```rust
// Source builder
Source::application("id")        // ApplicationSource
Source::postgres("id")           // PostgresReplicationSource
Source::http("id")               // HttpSource
Source::grpc("id")               // GrpcSource
Source::platform("id")           // PlatformSource
Source::mock("id")               // MockSource

// Query builder
Query::cypher("id")              // Cypher query
Query::gql("id")                 // GraphQL query

// Reaction builder
Reaction::application("id")      // ApplicationReaction
Reaction::http("id")             // HttpReaction
Reaction::grpc("id")             // GrpcReaction
Reaction::log("id")              // LogReaction
Reaction::sse("id")              // SseReaction
Reaction::platform("id")         // PlatformReaction
Reaction::profiler("id")         // ProfilerReaction
```

**Common Builder Methods**:
```rust
// All builders support
.with_id(String)
.with_property(key, value) / .with_properties(Properties)
.auto_start(bool)

// Source-specific
.with_dispatch_buffer_capacity(usize)
.with_dispatch_mode(DispatchMode)
.with_bootstrap_provider(BootstrapProviderConfig)

// Query-specific
.query(String)                   // The Cypher/GQL query text
.from_source(String)             // Single source
.from_sources(Vec<String>)       // Multiple sources
.with_priority_queue_capacity(usize)
.enable_bootstrap(bool)

// Reaction-specific
.subscribe_to(String)            // Single query
.subscribe_to_queries(Vec<String>) // Multiple queries
```

### 3.3 Configuration Types

Located in `src/config/`:

```rust
pub struct DrasiServerCoreConfig {
    pub server_core: Option<DrasiServerCoreSettings>,
    pub sources: Vec<SourceConfig>,
    pub queries: Vec<QueryConfig>,
    pub reactions: Vec<ReactionConfig>,
}

pub struct DrasiServerCoreSettings {
    pub id: Option<String>,
    pub priority_queue_capacity: Option<usize>,
    pub dispatch_buffer_capacity: Option<usize>,
}

pub struct SourceConfig {
    pub id: String,
    pub source_type: String,  // "postgres", "http", "grpc", "mock", "application", "platform"
    pub auto_start: Option<bool>,
    pub dispatch_mode: Option<DispatchMode>,
    pub dispatch_buffer_capacity: Option<usize>,
    pub bootstrap_provider: Option<BootstrapProviderConfig>,
    pub config: SourceSpecificConfig,  // Type-specific config
    pub properties: serde_json::Map<String, Value>,
}

pub struct QueryConfig {
    pub id: String,
    pub query: String,
    pub query_language: Option<QueryLanguage>,  // Cypher (default) or GQL
    pub sources: Vec<String>,
    pub auto_start: Option<bool>,
    pub dispatch_mode: Option<DispatchMode>,
    pub dispatch_buffer_capacity: Option<usize>,
    pub priority_queue_capacity: Option<usize>,
    pub enable_bootstrap: Option<bool>,
    pub bootstrap_buffer_size: Option<usize>,
}

pub struct ReactionConfig {
    pub id: String,
    pub reaction_type: String,  // "application", "http", "grpc", "log", etc.
    pub queries: Vec<String>,
    pub auto_start: Option<bool>,
    pub priority_queue_capacity: Option<usize>,
    pub config: ReactionSpecificConfig,  // Type-specific config
    pub properties: serde_json::Map<String, Value>,
}
```

### 3.4 Component Status and Monitoring

```rust
pub enum ComponentStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error,
}

pub struct SourceRuntime {
    pub id: String,
    pub source_type: String,
    pub status: ComponentStatus,
    pub error_message: Option<String>,
}

pub struct QueryRuntime {
    pub id: String,
    pub status: ComponentStatus,
    pub error_message: Option<String>,
}

pub struct ReactionRuntime {
    pub id: String,
    pub reaction_type: String,
    pub status: ComponentStatus,
    pub error_message: Option<String>,
}
```

### 3.5 Application Integration Handles

**ApplicationSourceHandle** - For injecting data:
```rust
pub struct ApplicationSourceHandle { /* ... */ }

impl ApplicationSourceHandle {
    pub async fn send_node_insert(
        &self, 
        id: String, 
        labels: Vec<String>,
        properties: ElementPropertyMap
    ) -> Result<()>

    pub async fn send_node_update(...) -> Result<()>
    pub async fn send_node_delete(...) -> Result<()>
    pub async fn send_relation_insert(...) -> Result<()>
    pub async fn send_relation_update(...) -> Result<()>
    pub async fn send_relation_delete(...) -> Result<()>
    pub async fn send_batch(changes: Vec<SourceChange>) -> Result<()>
}
```

**ApplicationReactionHandle** - For consuming results:
```rust
pub struct ApplicationReactionHandle { /* ... */ }

impl ApplicationReactionHandle {
    pub async fn subscribe_with_options(
        &self,
        options: SubscriptionOptions
    ) -> Result<QueryResultSubscription>

    pub async fn subscribe() -> Result<QueryResultSubscription>
}

pub struct QueryResultSubscription {
    pub async fn recv() -> Option<Arc<QueryResult>>
}
```

**PropertyMapBuilder** - For building properties:
```rust
pub struct PropertyMapBuilder { /* ... */ }

impl PropertyMapBuilder {
    pub fn new() -> Self
    pub fn with_string(self, key: &str, value: &str) -> Self
    pub fn with_integer(self, key: &str, value: i64) -> Self
    pub fn with_float(self, key: &str, value: f64) -> Self
    pub fn with_bool(self, key: &str, value: bool) -> Self
    pub fn build(self) -> ElementPropertyMap
}
```

---

## 4. Component Interactions and Data Flow

### 4.1 High-Level Data Flow

```
┌─────────────┐
│   Source    │  Data changes from external system
└──────┬──────┘
       │ (SourceChangeEvent)
       ↓
┌─────────────────────────────┐
│   Source Dispatcher         │  Routes to queries
│  (Broadcast or Channel)     │
└──────┬──────────────────────┘
       │
       ├→ [Query 1] → [Priority Queue] → [Reaction 1]
       ├→ [Query 2] → [Priority Queue] → [Reaction 2]
       └→ [Query 3] → [Priority Queue] → [Reaction 3]
```

### 4.2 Source → Query Flow

```
1. Query subscribes to Source
   query.start() → source.subscribe(query_id, enable_bootstrap, labels, relations)

2. Source creates subscription channels
   - Bootstrap channel (if enabled, delivers initial data)
   - Event channel (streams changes)
   
3. Query receives bootstrap phase
   - Gets initial data from bootstrap provider
   - Evaluates bootstrap data against query
   - Sends initial results to reactions

4. Query receives change events
   - For each SourceChangeEvent:
     - Evaluate change against continuous query
     - Track incremental results (diffs only)
     - Dispatch changed results to reactions
```

### 4.3 Query → Reaction Flow

```
1. Reaction subscribes to Query
   reaction.start() → query.subscribe(reaction_id)

2. Query creates subscription receiver
   - Returns broadcast receiver or channel receiver
   - Based on dispatch mode

3. Reaction receives results
   - Query emits QueryResult when state changes
   - Reaction batches/processes results
   - Delivers to external system
```

### 4.4 Event Types

Located in `src/channels/events.rs`:

```rust
// Source to Query
pub struct SourceEventWrapper {
    pub source_id: String,
    pub change: SourceChange,  // From drasi-core
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// Query to Reaction
pub struct QueryResult {
    pub query_id: String,
    pub results: Arc<HashMap<String, ElementPropertyMap>>,  // Row ID → properties
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// Bootstrap events
pub struct BootstrapEventWrapper {
    pub query_id: String,
    pub element: Element,  // Node or Relation
    pub timestamp: chrono::DateTime<chrono::Utc>,
}
```

### 4.5 Dispatch Modes

Two modes control how events are routed:

**Channel Mode** (default):
- Each subscriber gets dedicated channel
- Subscribers process independently
- Higher memory usage
- Better for low fanout (1-5 subscribers)

**Broadcast Mode**:
- Single shared channel with multiple receivers
- Subscribers share bandwidth
- Lower memory usage
- Better for high fanout (10+ subscribers)
- Slowest subscriber can backpressure others

---

## 5. Configuration System

### 5.1 Configuration File Format

YAML (or JSON) with four sections:

```yaml
server_core:          # Global server settings (optional)
  id: "my-server"
  priority_queue_capacity: 10000
  dispatch_buffer_capacity: 1000

sources:              # Data ingestion sources
  - id: "source-1"
    source_type: "postgres"  # or http, grpc, mock, application, platform
    auto_start: true
    # Source-specific properties...

queries:              # Continuous queries
  - id: "query-1"
    query: "MATCH (n:User) WHERE n.active RETURN n"
    queryLanguage: Cypher    # or GQL
    sources: ["source-1"]
    auto_start: true
    enableBootstrap: true
    # Query-specific properties...

reactions:            # Output destinations
  - id: "reaction-1"
    reaction_type: "log"  # or http, grpc, application, etc.
    queries: ["query-1"]
    auto_start: true
    # Reaction-specific properties...
```

### 5.2 Configuration Hierarchy

Performance settings cascade through three levels:

```
Global (server_core.*)
    ↓ (default for all components)
Component Type (source/query/reaction level)
    ↓ (default for that component)
Individual Component (component-specific)
    ↓ (final value)
```

Example:
```yaml
server_core:
  priority_queue_capacity: 50000     # Global default

queries:
  - id: query1
    priority_queue_capacity: 10000   # Override for this query
```

---

## 6. Lifecycle Management

### 6.1 Component Lifecycle States

```
┌────────────┐
│  Stopped   │  Initial state (before start)
└─────┬──────┘
      │ start()
      ↓
┌────────────┐
│  Starting  │  Initializing connections, loading bootstrap
└─────┬──────┘
      │ (transitions automatically)
      ↓
┌────────────┐
│  Running   │  Active processing
└─────┬──────┘
      │ stop()
      ↓
┌────────────┐
│  Stopping  │  Graceful shutdown
└─────┬──────┘
      │ (transitions automatically)
      ↓
┌────────────┐
│  Stopped   │  Stopped state
└────────────┘

Error state can occur in any state if error_message is set
```

### 6.2 Server Lifecycle

```
1. Creation: DrasiServerCore::builder().build().await
   - Config validation
   - Component initialization
   - Handle registry setup

2. Start: core.start().await
   - Start auto-start components
   - Sources begin listening
   - Queries begin subscribing
   - Reactions begin processing

3. Stop: core.stop().await
   - Graceful shutdown of all components
   - Cleanup of resources

4. Restart: core.start().await again
   - Remembers which components were running
   - Restarts them automatically
```

### 6.3 Component Start Dependencies

```
Source starts →  listening for changes
  ↓
Query starts →  subscribes to sources, gets bootstrap data
  ↓
Reaction starts → subscribes to queries
```

---

## 7. Main Data Flow Patterns

### 7.1 Bootstrap Flow (One-time Startup)

```
Query starts
  │
  ├→ For each subscribed source:
  │    ├→ Create subscription with enable_bootstrap=true
  │    └→ Source creates bootstrap channel
  │
  ├→ Source delegates to bootstrap provider
  │    └→ BootstrapProvider.bootstrap(request, channel)
  │
  └→ Query evaluates bootstrap data
      └→ Results sent to reactions
```

### 7.2 Change Processing Flow (Ongoing)

```
Application sends change event to Source
  │
  ↓
Source publishes to dispatcher
  │
  ├→ [Query 1 receives change]
  │    ├→ Evaluate against query pattern
  │    ├→ Track incremental results
  │    └→ Dispatch deltas to Priority Queue
  │
  └→ [Query N receives change]
       └→ (same)

Priority Queue orders by timestamp
  │
  ↓
Reaction receives ordered results
  │
  ├→ Batches results
  ├→ Formats for delivery (HTTP, gRPC, etc.)
  └→ Sends to external system
```

### 7.3 Multi-Source Join Flow

```
Query subscribes to multiple sources:
  - source-1 (table: orders)
  - source-2 (table: customers)

Pattern: MATCH (o:Order)-[rel:PlacedBy]->(c:Customer) RETURN o, c

Flow:
1. Query subscribes to both sources
2. Each source sends changes independently
3. Query evaluates join pattern for each change
4. Only emits results when join condition is met
```

---

## 8. Examples and Usage Patterns

### 8.1 Simple Application Example

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, PropertyMapBuilder};

#[tokio::main]
async fn main() -> Result<()> {
    // Create server
    let core = DrasiServerCore::builder()
        .add_source(Source::application("events").auto_start(true).build())
        .add_query(
            Query::cypher("active_users")
                .query("MATCH (u:User) WHERE u.active RETURN u")
                .from_source("events")
                .auto_start(true)
                .build()
        )
        .add_reaction(
            Reaction::application("results")
                .subscribe_to("active_users")
                .auto_start(true)
                .build()
        )
        .build()
        .await?;

    // Start processing
    core.start().await?;

    // Get handles
    let source_handle = core.source_handle("events")?;
    let reaction_handle = core.reaction_handle("results")?;

    // Inject data
    let props = PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .with_bool("active", true)
        .build();
    source_handle.send_node_insert("u1", vec!["User"], props).await?;

    // Consume results
    let mut sub = reaction_handle.subscribe().await?;
    while let Some(result) = sub.recv().await {
        println!("Result: {:?}", result);
    }

    core.stop().await?;
    Ok(())
}
```

### 8.2 Configuration File Example

```yaml
server_core:
  id: "orders-app"
  priority_queue_capacity: 50000

sources:
  - id: "postgres-orders"
    source_type: "postgres"
    auto_start: true
    host: localhost
    port: 5432
    database: orders
    user: postgres
    password: secret
    tables: ["orders", "customers"]
    table_keys:
      - table: orders
        key_columns: ["order_id"]
      - table: customers
        key_columns: ["customer_id"]
    bootstrap_provider:
      type: postgres

queries:
  - id: "late-orders"
    query: "MATCH (o:Order) WHERE o.days_late > 5 RETURN o"
    sources: ["postgres-orders"]
    auto_start: true
    enableBootstrap: true

reactions:
  - id: "alert-webhook"
    reaction_type: "http"
    queries: ["late-orders"]
    auto_start: true
    base_url: "https://api.example.com/alerts"
```

### 8.3 Test Data Bootstrap Example

```yaml
sources:
  - id: "test-data"
    source_type: "http"
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "./test_data.jsonl"

reactions:
  - id: "mock-test"
    reaction_type: "mock"
    queries: ["test-query"]
```

Script file content (test_data.jsonl):
```json
{"type": "Header", "elements": {}}
{"type": "Node", "id": "n1", "labels": ["User"], "properties": {"name": "Alice", "age": 30}}
{"type": "Node", "id": "n2", "labels": ["User"], "properties": {"name": "Bob", "age": 25}}
{"type": "Relation", "id": "r1", "type": "KNOWS", "from": "n1", "to": "n2", "properties": {}}
{"type": "Finish"}
```

---

## 9. Test Infrastructure

### 9.1 Test Structure

```
tests/
├── integration.rs                        # Library usage tests
├── multi_source_join_test.rs            # Multi-source query tests
├── multi_reaction_subscription_test.rs  # Multiple reactions
├── bootstrap_platform_provider.rs       # Bootstrap tests
├── platform_source_integration.rs       # Redis integration
├── platform_reaction_integration.rs     # Redis publishing
├── dispatch_buffer_capacity_hierarchy_test.rs
├── priority_queue_hierarchy_test.rs
├── config_validation.rs                 # Configuration validation
├── example_compilation_test.rs          # Example verification
└── profiling_integration_test.rs        # Performance tests
```

### 9.2 Test Support Utilities

Located in `src/test_support/`:
- `helpers.rs` - Test utilities
- `redis_helpers.rs` - Redis testcontainers support

Uses:
- `tokio-test` for async testing
- `testcontainers` for Redis/PostgreSQL
- `mockall` for mocking

---

## 10. Error Handling

### 10.1 Error Type

```rust
pub enum DrasiError {
    ConfigError(String),
    ComponentNotFound(String),
    ComponentAlreadyExists(String),
    SourceError(String),
    QueryError(String),
    ReactionError(String),
    BootstrapError(String),
    ValidationError(String),
    InternalError(String),
}

pub type Result<T> = std::result::Result<T, DrasiError>;
```

### 10.2 Error Handling Pattern

All operations return `Result<T>`:
- Component operations (start, stop, etc.)
- Configuration loading and validation
- Bootstrap operations
- Message sending (bounded channels)

Component errors are tracked in status:
```rust
pub struct ComponentRuntime {
    pub status: ComponentStatus,
    pub error_message: Option<String>,
}
```

---

## 11. Performance Considerations

### 11.1 Tuning Parameters

1. **priority_queue_capacity** (default: 10000)
   - Size of buffer for timestamp-ordered results
   - Larger = more memory, handles bursts better
   - Smaller = less memory, may drop events

2. **dispatch_buffer_capacity** (default: 1000)
   - Channel buffer between components
   - Larger = handles bursts, higher memory
   - Smaller = tighter backpressure, lower memory

3. **dispatch_mode** (Channel vs Broadcast)
   - Channel: Better for few subscribers
   - Broadcast: Better for many subscribers

### 11.2 Profiling

ProfilerReaction collects timing data:
```yaml
reactions:
  - id: "profiler"
    reaction_type: "profiler"
    queries: ["query1"]
```

---

## 12. Key Implementation Patterns

### 12.1 Arc<RwLock<T>> Pattern

Used throughout for shared mutable state:
```rust
pub struct Source {
    status: Arc<RwLock<ComponentStatus>>,
    dispatchers: Arc<RwLock<Vec<...>>>,
}
```

Allows:
- Multiple readers concurrently
- Single writer exclusive access
- Thread-safe sharing via Arc cloning

### 12.2 Trait Objects for Polymorphism

```rust
pub trait Source: Send + Sync { ... }

pub struct SourceManager {
    sources: Arc<RwLock<HashMap<String, Arc<dyn Source>>>>,
}
```

Enables:
- Runtime dispatch of different source types
- Type erasure in collections

### 12.3 Builder Pattern

Fluent API for ergonomic configuration:
```rust
Source::application("id")
    .with_property("key", value)
    .auto_start(true)
    .with_dispatch_buffer_capacity(5000)
    .build()
```

Benefits:
- Type-safe
- Chainable
- Clear intent
- IDE autocompletion

### 12.4 Tokio Async/Await

All I/O operations are async:
```rust
pub async fn start(&self) -> Result<()>
pub async fn send_node_insert(...) -> Result<()>
pub async fn recv() -> Option<QueryResult>
```

---

## 13. External Dependencies (Key)

Core functionality:
- **drasi-core**: Graph query engine, in-memory indexing
- **drasi-query-cypher**: Cypher parser and compiler
- **drasi-query-gql**: GraphQL parser
- **drasi-functions-cypher**: Built-in Cypher functions
- **drasi-functions-gql**: Built-in GraphQL functions

Async runtime:
- **tokio**: Async runtime with full features
- **async-trait**: Trait methods with async/await

PostgreSQL:
- **tokio-postgres**: Async PostgreSQL client
- **postgres-protocol**: Protocol handling
- **deadpool-postgres**: Connection pooling

gRPC:
- **tonic**: gRPC server/client
- **prost**: Protocol buffer serialization

Redis:
- **redis**: Client library with streams support

Web:
- **axum**: HTTP server framework (if used)
- **tower-http**: HTTP utilities
- **reqwest**: HTTP client

Serialization:
- **serde**: Serialization framework
- **serde_json**: JSON support
- **serde_yaml**: YAML support

Utilities:
- **anyhow**: Error handling
- **log/env_logger**: Logging
- **uuid**: ID generation
- **chrono**: Timestamp handling
- **ordered_float**: Float ordering

---

## Summary

**DrasiServerCore** is a sophisticated event-driven library that enables change-driven data processing. Its modular design, clean separation of concerns, and flexible component architecture make it ideal for embedding in larger systems. The library handles the complex task of managing asynchronous data flows, query evaluation, and result dispatch while exposing a clean, type-safe public API for application developers.

