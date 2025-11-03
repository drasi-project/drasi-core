# Drasi Server-Core Architecture

This document provides a comprehensive overview of the drasi-core server-core architecture as it exists today.

**Last Updated:** 2025-11-03
**Version:** 7.0 - Current Architecture Snapshot

---

## Table of Contents

1. [Overview](#overview)
2. [Core Architecture](#core-architecture)
3. [Component Types](#component-types)
4. [Event Distribution System](#event-distribution-system)
5. [Configuration System](#configuration-system)
6. [Bootstrap Architecture](#bootstrap-architecture)
7. [Component Lifecycle](#component-lifecycle)
8. [API Design](#api-design)
9. [Testing Infrastructure](#testing-infrastructure)
10. [Areas for Improvement](#areas-for-improvement)

---

## Overview

The server-core is a Rust library for reactive, event-driven continuous query processing over streaming graph data. It processes data changes from various sources through Cypher/GQL queries and delivers results to reactions. The library is designed to be embedded in applications and provides both programmatic and configuration-based APIs.

### Key Design Principles

1. **Direct Subscriptions**: Components subscribe directly to each other without centralized routers
2. **Trait-Based Abstraction**: All major components defined via traits for extensibility
3. **Zero-Copy Event Sharing**: Events distributed via `Arc<T>` to minimize allocations
4. **Configurable Dispatch Modes**: Channel (default) or Broadcast modes for event distribution
5. **Pluggable Bootstrap**: Universal bootstrap provider system decoupled from source types
6. **Three-Level Configuration**: Component → Global → Hardcoded default hierarchy
7. **Async-First Design**: Built on Tokio for high-performance async I/O

### System Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        DrasiServerCore                            │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │
│  │ SourceManager│  │ QueryManager │  │ReactionManager│          │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘          │
│         │                  │                  │                   │
│   ┌─────▼──────┐    ┌─────▼──────┐    ┌─────▼──────┐           │
│   │   Sources  │───▶│   Queries  │───▶│ Reactions  │           │
│   └────────────┘    └────────────┘    └────────────┘           │
│         │                  │                  │                   │
│    [Dispatchers]     [Dispatchers]    [Priority Queues]         │
│         │                  │                  │                   │
│    Bootstrap          Priority           Result                  │
│    Providers           Queues          Processing               │
└──────────────────────────────────────────────────────────────────┘
```

---

## Core Architecture

### DrasiServerCore

The central orchestrator managing all components:

```rust
pub struct DrasiServerCore {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<RwLock<SourceManager>>,
    query_manager: Arc<RwLock<QueryManager>>,
    reaction_manager: Arc<RwLock<ReactionManager>>,
    handle_registry: HandleRegistry,
    initialized: bool,
    running: bool,
}
```

Key responsibilities:
- Component lifecycle management (create, start, stop, remove)
- Configuration management
- Handle registry for programmatic access
- Coordination between managers
- Public API surface

### Component Managers

Each manager follows similar patterns:

**SourceManager** (`src/sources/manager.rs`):
- Creates sources based on type (postgres, http, grpc, mock, platform, application)
- Manages source lifecycle
- Provides introspection APIs
- Handles bootstrap provider integration

**QueryManager** (`src/queries/manager.rs`):
- Creates queries with appropriate language parser (Cypher/GQL)
- Manages direct subscriptions to sources
- Handles query compilation and execution
- Coordinates bootstrap and streaming phases

**ReactionManager** (`src/reactions/manager.rs`):
- Creates reactions based on type
- Manages subscriptions to queries
- Handles reaction lifecycle
- Supports custom reaction types via registry

### Base Abstractions

Common functionality extracted into base classes:

**SourceBase** (`src/sources/base.rs`):
```rust
pub struct SourceBase {
    pub config: SourceConfig,
    pub status: Arc<RwLock<ComponentStatus>>,
    pub(crate) dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
    pub event_tx: ComponentEventSender,
    pub task_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    pub shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
}
```

Provides:
- Dispatcher lifecycle management
- Bootstrap delegation
- Event dispatching with profiling metadata
- Status tracking
- Helper methods like `test_subscribe()` and `dispatch_from_task()`

**QueryBase** (`src/queries/base.rs`):
- Similar structure to SourceBase
- Manages result dispatchers
- Handles reaction subscriptions
- Provides common lifecycle management

**ReactionBase** (`src/reactions/base.rs`):
```rust
pub struct ReactionBase {
    pub config: ReactionConfig,
    pub status: Arc<RwLock<ComponentStatus>>,
    pub event_tx: ComponentEventSender,
    pub priority_queue: PriorityQueue<QueryResult>,
    pub subscription_tasks: Arc<RwLock<Vec<JoinHandle<()>>>>,
    pub processing_task: Arc<RwLock<Option<JoinHandle<()>>>>,
}
```

Provides:
- Query subscription management via `subscribe_to_queries()`
- Priority queue for timestamp-ordered result processing
- Task lifecycle management (subscription forwarders and processing tasks)
- Status tracking with `get_status()` and `set_status_with_event()`
- Common cleanup via `stop_common()` (aborts tasks, drains queue)
- Used by all 9 reaction implementations via composition

---

## Component Types

### Sources

All sources implement the `Source` trait and utilize `SourceBase`:

| Type | Purpose | Key Features |
|------|---------|--------------|
| **ApplicationSource** | Programmatic event injection | In-memory event storage, replay bootstrap |
| **MockSource** | Test data generation | Configurable event generation, test bootstrap |
| **PostgresReplicationSource** | PostgreSQL WAL streaming | Logical replication, SCRAM auth, snapshot bootstrap |
| **HttpSource** | HTTP endpoint | Adaptive format support, polling/push modes |
| **GrpcSource** | gRPC streaming | Protobuf support, bidirectional streaming |
| **PlatformSource** | Redis Streams | Consumer groups, exactly-once delivery |

### Queries

**DrasiQuery** implementation:
- Uses `drasi-core` evaluation engine
- Supports Cypher and GQL
- Priority queue for timestamp-ordered processing
- Direct source subscriptions
- Bootstrap phase tracking per source
- Synthetic join support for multi-source queries

### Reactions

All reactions process results through priority queues:

| Type | Purpose | Key Features |
|------|---------|--------------|
| **ApplicationReaction** | Programmatic consumption | Async channel-based API |
| **HttpReaction** | Webhook delivery | Batching, retry logic |
| **GrpcReaction** | gRPC streaming | Adaptive format support |
| **SseReaction** | Server-Sent Events | Browser-compatible streaming |
| **LogReaction** | Stdout logging | Debugging and monitoring |
| **PlatformReaction** | Redis Streams publishing | Platform integration |
| **ProfilerReaction** | Performance metrics | Latency tracking, throughput measurement |

---

## Event Distribution System

### Dispatcher Architecture

The trait-based dispatcher system provides flexible event distribution:

```rust
#[async_trait]
pub trait ChangeDispatcher<T>: Send + Sync {
    async fn dispatch_change(&self, change: Arc<T>) -> Result<()>;
    async fn dispatch_changes(&self, changes: Vec<Arc<T>>) -> Result<()>;
    fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>>;
}

#[async_trait]
pub trait ChangeReceiver<T>: Send + Sync {
    async fn recv(&mut self) -> Result<Arc<T>>;
}
```

### Dispatch Modes

**Channel Mode (Default)**:
- Dedicated MPSC channel per subscriber
- Better isolation and guaranteed delivery
- Created lazily on subscription
- No message loss concerns

**Broadcast Mode**:
- Single shared broadcast channel
- Memory efficient for many subscribers
- Risk of lag if receiver can't keep up
- Created eagerly at initialization

### Event Types

```rust
pub enum SourceEvent {
    Change(SourceChange),           // Data change event
    Control(SourceControl),         // Flow control
    BootstrapStart { query_id },    // Bootstrap phase marker
    BootstrapEnd { query_id },      // Bootstrap completion
}

pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: DateTime<Utc>,
    pub profiling: Option<ProfilingMetadata>,
}
```

### Priority Queue Processing

Generic priority queue for timestamp-ordered processing:

```rust
pub struct PriorityQueue<T: Timestamped> {
    queue: Arc<Mutex<BinaryHeap<Reverse<Arc<T>>>>>,
    capacity: usize,
    metrics: PriorityQueueMetrics,
}
```

Features:
- Min-heap (oldest first) ordering
- Bounded capacity with backpressure
- Metrics tracking (enqueued, dequeued, dropped)
- Used by both queries and reactions

---

## Configuration System

### Three-Level Hierarchy

1. **Component-Specific** (highest priority)
   - Set in individual component configuration
   - Overrides all defaults

2. **Global Server Settings** (medium priority)
   - Set in `server_core` configuration section
   - Applied when component doesn't specify

3. **Hardcoded Defaults** (lowest priority)
   - Fallback values in code
   - Ensure system always has valid configuration

### Configuration Schema

```yaml
server_core:
  id: "server-id"
  priority_queue_capacity: 10000      # Global default
  dispatch_buffer_capacity: 1000      # Global default

sources:
  - id: "source1"
    source_type: "postgres"
    auto_start: true                  # Default: true
    dispatch_mode: "channel"          # channel | broadcast
    dispatch_buffer_capacity: 2000    # Override global
    bootstrap_provider:               # Optional
      type: "postgres"
    properties:
      host: "localhost"
      database: "mydb"

queries:
  - id: "query1"
    query: "MATCH (n) RETURN n"
    query_language: "Cypher"          # Cypher | GQL
    sources: ["source1"]
    enable_bootstrap: true            # Default: true
    priority_queue_capacity: 15000    # Override global
    dispatch_mode: "channel"
    joins:                           # Optional synthetic joins
      - type: "RELATES_TO"
        start_node_labels: ["Person"]
        end_node_labels: ["Project"]

reactions:
  - id: "reaction1"
    reaction_type: "log"
    queries: ["query1"]
    priority_queue_capacity: 20000    # Override global
```

### Runtime Configuration

The `RuntimeConfig` applies the hierarchy during initialization:

```rust
impl From<DrasiServerCoreConfig> for RuntimeConfig {
    fn from(config: DrasiServerCoreConfig) -> Self {
        // Extract global defaults
        // Apply to components without overrides
        // Preserve component-specific settings
    }
}
```

---

## Bootstrap Architecture

### Universal Provider System

Bootstrap is **completely decoupled** from source types - any source can use any bootstrap provider:

```
┌─────────────────────────────────────────────────┐
│  Source (ANY type)                              │
│    └── bootstrap_provider: Option<Config>       │
└────────────────┬────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────┐
│  BootstrapProviderFactory                       │
│    creates provider based on config.type        │
└────────────────┬────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────┐
│  BootstrapProvider Implementations:             │
│    - PostgresBootstrapProvider                  │
│    - ScriptFileBootstrapProvider                │
│    - ApplicationBootstrapProvider               │
│    - PlatformBootstrapProvider                  │
│    - NoOpBootstrapProvider                      │
└─────────────────────────────────────────────────┘
```

### Provider Types

| Provider | Use Case | Key Features |
|----------|----------|--------------|
| **PostgreSQL** | Database snapshots | COPY protocol, LSN coordination |
| **ScriptFile** | Test data, migrations | JSONL format, multi-file support |
| **Application** | Event replay | In-memory event storage |
| **Platform** | Remote Drasi | HTTP streaming from Query API |
| **NoOp** | No bootstrap needed | Returns immediately |

### Bootstrap Flow

1. Query subscribes with `enable_bootstrap=true`
2. Source creates dedicated bootstrap channel
3. Bootstrap provider spawned as async task
4. Events sent via dedicated MPSC channel
5. Query processes bootstrap before streaming
6. All sources must complete bootstrap before streaming begins

### Mix-and-Match Example

```yaml
# HTTP source with PostgreSQL bootstrap
sources:
  - id: "hybrid_source"
    source_type: "http"           # Streams via HTTP
    bootstrap_provider:
      type: "postgres"             # Bootstraps from PostgreSQL
    properties:
      # Properties used by both source and provider
      host: "localhost"
      database: "mydb"
      port: 9000                   # HTTP port
```

---

## Component Lifecycle

### Status State Machine

```rust
pub enum ComponentStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}
```

Transitions:
- `Stopped → Starting → Running` (normal start)
- `Running → Stopping → Stopped` (normal stop)
- `Any → Error` (on failure)

### Two-Phase Lifecycle

**Phase 1: Initialization** (one-time)
```rust
let mut core = DrasiServerCore::from_config(config)?;
core.initialize().await?;
```

**Phase 2: Start/Stop** (repeatable)
```rust
core.start().await?;    // Start auto_start components
// ... running ...
core.stop().await?;     // Stop all components
core.start().await?;    // Can restart
```

### Dynamic Management

Components can be added/removed at runtime:

```rust
// Add components
core.create_source(config).await?;
core.create_query(config).await?;
core.create_reaction(config).await?;

// Remove components
core.remove_source("id").await?;
core.remove_query("id").await?;
core.remove_reaction("id").await?;

// Individual control
core.start_source("id").await?;
core.stop_source("id").await?;
```

---

## API Design

### Builder Pattern API

Fluent builders for programmatic configuration:

```rust
let core = DrasiServerCore::builder()
    .with_id("my-server")
    .with_priority_queue_capacity(20000)
    .with_dispatch_buffer_capacity(2000)
    .add_source(
        Source::postgres("pg-source")
            .with_property("host", "localhost")
            .with_property("database", "mydb")
            .with_bootstrap(
                BootstrapProvider::postgres()
                    .with_tables(vec!["users", "orders"])
            )
            .with_dispatch_mode(DispatchMode::Channel)
            .build()
    )
    .add_query(
        Query::cypher("query1")
            .query("MATCH (n:User) RETURN n")
            .from_source("pg-source")
            .with_priority_queue_capacity(15000)
            .build()
    )
    .add_reaction(
        Reaction::application("app-reaction")
            .subscribe_to("query1")
            .build()
    )
    .build()
    .await?;
```

### Properties Builder

Type-safe property construction:

```rust
let props = Properties::new()
    .with_string("host", "localhost")
    .with_number("port", 5432)
    .with_bool("ssl", true)
    .with_value("custom", json!({"nested": "value"}))
    .build();
```

### Handle System

Programmatic access to application components:

```rust
// Get application source handle
let source_handle = core.source_handle("app-source").await?;
source_handle.publish(change).await?;

// Get application reaction handle
let reaction_handle = core.reaction_handle("app-reaction").await?;
let result = reaction_handle.recv().await?;
```

### Public API Surface

```rust
impl DrasiServerCore {
    // Lifecycle
    pub async fn initialize(&mut self) -> Result<()>
    pub async fn start(&mut self) -> Result<()>
    pub async fn stop(&mut self) -> Result<()>

    // Component Management
    pub async fn create_source(&mut self, config: SourceConfig) -> Result<()>
    pub async fn remove_source(&mut self, id: &str) -> Result<()>
    pub async fn start_source(&mut self, id: &str) -> Result<()>
    pub async fn stop_source(&mut self, id: &str) -> Result<()>

    // Introspection
    pub async fn list_sources(&self) -> Vec<String>
    pub async fn get_source_info(&self, id: &str) -> Result<SourceInfo>
    pub async fn get_source_status(&self, id: &str) -> Result<ComponentStatus>

    // Handle Access
    pub async fn source_handle(&self, id: &str) -> Result<ApplicationSourceHandle>
    pub async fn reaction_handle(&self, id: &str) -> Result<ApplicationReactionHandle>

    // Configuration
    pub fn get_current_config(&self) -> DrasiServerCoreConfig
}
```

---

## Testing Infrastructure

### Test Categories

**Unit Tests** (in module files):
- Component-specific logic
- Dispatcher implementations
- Configuration parsing
- Event handling

**Integration Tests** (`tests/`):
- End-to-end workflows
- Multi-component interaction
- Bootstrap scenarios
- Platform integration

**Example Programs** (`examples/`):
- Profiling demonstrations
- Configuration examples
- Component lifecycle
- Integration patterns

### Test Support Infrastructure

**Test Utilities**:
- `test_support/helpers.rs` - Common test functions
- `test_support/redis_helpers.rs` - Redis testing utilities
- Mock implementations for testing

**Fixture Data**:
- `tests/fixtures/bootstrap_scripts/` - JSONL test data
- `examples/data/` - Example datasets
- `examples/configs/` - Configuration examples

### Test Coverage Areas

Well-covered:
- Basic source/query/reaction lifecycle
- Configuration parsing and validation
- Bootstrap provider implementations
- Dispatcher modes and event flow
- Priority queue operations

Areas needing more coverage:
- Error recovery scenarios
- Network failure handling
- Performance under load
- Concurrent operations
- Edge cases in bootstrap/streaming transitions

---

## Areas for Improvement

### Design and Architecture

1. **Error Handling Consistency**
   - Mixed use of `anyhow::Result` and custom `DrasiError`
   - Recommendation: Standardize on `DrasiError` with proper error context
   - Add error recovery strategies for transient failures

2. **Resource Management**
   - No explicit resource limits (memory, connections)
   - Recommendation: Add resource pooling and limits
   - Implement backpressure throughout the pipeline

3. **Observability**
   - Limited metrics beyond basic profiling
   - Recommendation: Add comprehensive metrics (Prometheus-style)
   - Structured logging with correlation IDs
   - Distributed tracing support

### Code Quality

1. **Documentation**
   - Many public APIs lack comprehensive documentation
   - Recommendation: Add rustdoc comments with examples
   - Document error conditions and edge cases

2. **Type Safety**
   - Heavy use of `HashMap<String, Value>` for properties
   - Recommendation: Consider typed configuration structs
   - Use newtype pattern for IDs (SourceId, QueryId, etc.)

3. **Async Patterns**
   - Some blocking operations in async contexts
   - Recommendation: Review and fix all `block_in_place` usage
   - Consider using `tokio::task::spawn_blocking` appropriately

### Idiomatic Rust

1. **Clone Usage**
   - Excessive cloning of `Arc<RwLock<T>>` structures
   - Recommendation: Review Arc usage, consider `Arc<T>` without RwLock where possible
   - Use `Cow` for strings that are rarely modified

2. **Error Types**
   - String errors in some places (`Result<T, String>`)
   - Recommendation: Always use proper error types
   - Implement `std::error::Error` for all custom errors

3. **Lifetime Management**
   - Some unnecessary allocations
   - Recommendation: Use borrowing where possible
   - Consider zero-copy parsing for large data

### Test Coverage Improvements

1. **Stress Testing**
   - No load testing or stress tests
   - Recommendation: Add benchmark suite
   - Test with millions of events
   - Memory leak detection tests

2. **Failure Scenarios**
   - Limited error recovery testing
   - Recommendation: Test network partitions
   - Test partial failures in multi-component systems
   - Chaos engineering tests

3. **Property-Based Testing**
   - Currently only example-based tests
   - Recommendation: Add proptest for complex logic
   - Fuzz testing for parsers
   - Invariant checking

### Performance Optimizations

1. **Memory Usage**
   - No memory pooling or recycling
   - Recommendation: Implement object pools for frequently allocated types
   - Consider arena allocation for query processing

2. **Concurrency**
   - Sequential processing in some areas
   - Recommendation: Identify and parallelize independent operations
   - Consider work-stealing for query execution

3. **I/O Optimization**
   - Some inefficient I/O patterns
   - Recommendation: Batch I/O operations where possible
   - Use vectored I/O for network operations
   - Consider io_uring for Linux deployments

### Security Considerations

1. **Input Validation**
   - Limited validation on external inputs
   - Recommendation: Add comprehensive input validation
   - Sanitize all user-provided data
   - Rate limiting for API endpoints

2. **Authentication/Authorization**
   - No built-in auth mechanisms
   - Recommendation: Add pluggable auth system
   - Support for mTLS, JWT, API keys
   - Role-based access control

3. **Audit Logging**
   - No audit trail for operations
   - Recommendation: Add audit logging for all mutations
   - Track who/what/when for compliance
   - Secure storage of audit logs

---

## Summary

The drasi-core server-core provides a robust foundation for event-driven continuous query processing with:

**Strengths**:
- Clean trait-based architecture
- Flexible dispatch system
- Universal bootstrap providers
- Comprehensive builder API
- Good test coverage for core functionality

**Key Innovations**:
- Decoupled bootstrap from streaming
- Direct subscription model without routers
- Zero-copy event distribution
- Three-level configuration hierarchy

**Areas for Growth**:
- Enhanced observability and metrics
- Improved error handling and recovery
- Performance optimizations
- Security hardening
- More comprehensive testing

The architecture is well-designed for its current use cases and provides a solid foundation for future enhancements. The recommendations above would help evolve it into a production-grade system suitable for demanding enterprise deployments.