# Drasi Server-Core Architecture

This document provides a comprehensive overview of the drasi-core server-core architecture as it exists today.

**Last Updated:** 2025-10-27
**Version:** 6.0 - Current Architecture Snapshot

---

## Table of Contents

1. [Overview](#overview)
2. [Core Abstractions](#core-abstractions)
3. [Component Types](#component-types)
4. [Communication Patterns](#communication-patterns)
5. [Configuration System](#configuration-system)
6. [Bootstrap Architecture](#bootstrap-architecture)
7. [Component Lifecycle](#component-lifecycle)
8. [File Structure](#file-structure)
9. [Design Rationale](#design-rationale)

---

## Overview

The server-core is a library for reactive, event-driven continuous query processing over streaming graph data. It is designed to be embedded in applications and provides programmatic APIs for event injection and result consumption.

### System Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     DrasiServerCore                              │
│  ┌────────────┐  ┌────────────┐  ┌────────────┐                │
│  │  Source    │  │   Query    │  │  Reaction  │                │
│  │  Manager   │  │  Manager   │  │  Manager   │                │
│  └────────────┘  └────────────┘  └────────────┘                │
└─────────────────────────────────────────────────────────────────┘
        │                  │                  │
   ┌────▼────┐       ┌─────▼─────┐    ┌──────▼──────┐
   │ Sources │──────▶│  Queries  │───▶│  Reactions  │
   └─────────┘       └───────────┘    └─────────────┘
        │                  │                  │
    Dispatchers       Dispatchers       Priority Queues
```

### Component Types

- **Sources**: Ingest data changes from various sources (Postgres, HTTP, gRPC, Redis, Application API, Mock)
- **Queries**: Process changes through continuous Cypher/GQL queries, emit results
- **Reactions**: Consume query results and take actions (Application API, HTTP, gRPC, SSE, Log, Platform)

### Key Principles

1. **Direct Subscriptions**: Queries subscribe directly to Sources; Reactions subscribe directly to Queries
2. **Trait-Based Dispatching**: All event distribution uses `ChangeDispatcher` and `ChangeReceiver` traits
3. **Configurable Dispatch Modes**: Channel (1-to-1, default) or Broadcast (1-to-N)
4. **Zero-Copy Messaging**: Events shared via `Arc<T>` to avoid serialization
5. **Priority Queue Processing**: Timestamp-ordered event processing in Queries and Reactions
6. **Pluggable Bootstrap**: Universal bootstrap provider system, completely separate from streaming
7. **Three-Level Configuration**: Component override → Global default → Hardcoded fallback

---

## Core Abstractions

### Dispatcher System

The dispatcher system provides flexible event distribution patterns through trait abstractions.

**File**: `src/channels/dispatcher.rs`

#### DispatchMode Enum

```rust
pub enum DispatchMode {
    Broadcast,  // 1-to-N fanout, single shared channel
    Channel,    // 1-to-1 dedicated channels (DEFAULT)
}

impl Default for DispatchMode {
    fn default() -> Self {
        DispatchMode::Channel
    }
}
```

#### Core Traits

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

#### Implementations

**BroadcastChangeDispatcher** (Broadcast Mode):
- Uses Tokio `broadcast::channel`
- Single channel shared by all subscribers
- All receivers get all messages
- Handles lag gracefully (logs warning, continues)
- Created once at component initialization
- Memory efficient for many subscribers

**ChannelChangeDispatcher** (Channel Mode):
- Uses Tokio `mpsc::channel`
- Dedicated channel per subscriber
- One-to-one communication
- No lag concerns
- Created on-demand during subscription (lazy)
- Better isolation between subscribers

### Base Classes

Common functionality for sources and queries is extracted into base classes.

**SourceBase** (`src/sources/base.rs`):
- Dispatcher lifecycle management
- Bootstrap subscription delegation
- Event dispatching with profiling
- Component status tracking
- Common start/stop functionality

**QueryBase** (`src/queries/base.rs`):
- Dispatcher lifecycle management
- Subscription handling for reactions
- Result dispatching with Arc wrapping
- Component status tracking
- Common lifecycle management

### Channel Types

**Broadcast Channels**:
- Type: `broadcast::Sender<Arc<T>>` / `broadcast::Receiver<Arc<T>>`
- Use: Dispatcher-based event distribution (when mode = Broadcast)
- Capacity: Configurable via `dispatch_buffer_capacity`

**MPSC Channels**:
- Type: `mpsc::Sender<Arc<T>>` / `mpsc::Receiver<Arc<T>>`
- Use: Dispatcher-based event distribution (when mode = Channel)
- Capacity: Configurable via `dispatch_buffer_capacity`

**Bootstrap Channels**:
- Type: `mpsc::Sender<BootstrapEvent>` / `mpsc::Receiver<BootstrapEvent>`
- Use: Dedicated bootstrap event delivery
- Capacity: Hardcoded to 1000
- Separate from streaming channels

**Priority Queues** (`src/channels/priority_queue.rs`):
- Generic `PriorityQueue<T where T: Timestamped>`
- Min-heap (oldest events first)
- Bounded capacity with backpressure
- Used by Queries and Reactions for ordered processing
- Capacity: Configurable via `priority_queue_capacity`

---

## Component Types

### Sources

All sources implement the `Source` trait:

```rust
#[async_trait]
pub trait Source: Send + Sync {
    async fn start(&mut self) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    async fn subscribe(&self, ...) -> Result<SubscriptionResponse>;
    fn get_config(&self) -> SourceConfig;
}
```

**Available Source Types**:

| Type | File | Description |
|------|------|-------------|
| **PostgresReplicationSource** | `src/sources/postgres` | PostgreSQL logical replication (WAL decoding) |
| **HttpSource** | `src/sources/http` | HTTP POST endpoint for change events |
| **GrpcSource** | `src/sources/grpc` | gRPC streaming for change events |
| **MockSource** | `src/sources/mock` | Test source with synthetic data |
| **PlatformSource** | `src/sources/platform` | Redis Streams consumer (platform SDK integration) |
| **ApplicationSource** | `src/sources/application` | Programmatic event injection via API |

**Common Characteristics**:
- All use `SourceBase` for dispatcher management
- All support configurable bootstrap providers
- All dispatch `Arc<SourceEventWrapper>` to subscribers
- Dispatch mode configurable per source (default: Channel)

### Queries

**DrasiQuery** (`src/queries/manager.rs`):

Core components:
- **QueryBase**: Common functionality (dispatchers, subscriptions, lifecycle)
- **ContinuousQuery**: drasi-core query evaluation engine
- **PriorityQueue**: Timestamp-ordered event processing
- **Source Subscriptions**: Direct subscriptions to each configured source
- **Bootstrap Tracking**: Per-source bootstrap phase tracking

**Query Languages**:
- **Cypher** (default): Graph pattern matching
- **GQL**: GraphQL queries compiled to Cypher

**Processing Flow**:
1. Subscribe to all configured sources (direct, no router)
2. Receive events from source dispatchers
3. Enqueue to priority queue (timestamp-ordered)
4. Dequeue and process through ContinuousQuery engine
5. Dispatch results to reactions via query dispatchers

**Bootstrap Integration**:
- Separate bootstrap receiver per source subscription
- Phases: NotStarted → InProgress → Completed
- All sources must complete bootstrap before streaming begins
- Bootstrap events processed with priority

**Synthetic Joins**:
- Configured via `joins` in query config
- Allows relationships between nodes from different sources
- Implemented at query evaluation level

### Reactions

All reactions implement the `Reaction` trait:

```rust
#[async_trait]
pub trait Reaction: Send + Sync {
    async fn start(&mut self, server_core: Arc<RwLock<DrasiServerCore>>) -> Result<()>;
    async fn stop(&mut self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> ReactionConfig;
}
```

**Available Reaction Types**:

| Type | File | Description |
|------|------|-------------|
| **ApplicationReaction** | `src/reactions/application` | Programmatic result consumption via API |
| **LogReaction** | `src/reactions/log` | Log results to stdout |
| **HttpReaction** | `src/reactions/http` | Send results via HTTP POST |
| **GrpcReaction** | `src/reactions/grpc` | Send results via gRPC |
| **SseReaction** | `src/reactions/sse` | Server-Sent Events for browsers |
| **PlatformReaction** | `src/reactions/platform` | Platform SDK integration |
| **ProfilerReaction** | `src/reactions/profiler` | Performance metrics |

**Common Characteristics**:
- All use priority queues for ordered result processing
- All subscribe directly to queries (no router)
- Receive `Arc<QueryResult>` from query dispatchers

---

## Communication Patterns

### Sources → Queries

**Direct Subscription Model**:

```
┌────────┐                                  ┌────────┐
│ Source │                                  │ Query  │
│        │◀─────── subscribe() ─────────────│        │
│        │                                  │        │
│        │──── SubscriptionResponse ───────▶│        │
│        │  (receiver + bootstrap_receiver) │        │
│        │                                  │        │
│        │                                  │        │
│ Dispatch                                  │ Receive│
│  Mode? │                                  │        │
└───┬────┘                                  └────────┘
    │
    ├─ Broadcast: Use shared BroadcastChangeDispatcher
    │             Returns receiver from shared channel
    │
    └─ Channel:   Create new ChannelChangeDispatcher
                  Returns receiver from dedicated channel
```

**Event Flow**:
```
Source Event → Arc::new(SourceEventWrapper)
            → dispatcher.dispatch_change(arc)
            → Dispatcher (broadcast or mpsc)
            → Receiver
            → Query Priority Queue
            → ContinuousQuery Processing
```

### Queries → Reactions

**Direct Subscription Model**:

```
┌────────┐                                  ┌──────────┐
│ Query  │                                  │ Reaction │
│        │◀───── subscribe() ───────────────│          │
│        │                                  │          │
│        │──── Receiver ────────────────────▶│          │
│        │                                  │          │
│        │                                  │          │
│ Dispatch                                  │ Receive  │
│  Mode? │                                  │          │
└───┬────┘                                  └──────────┘
    │
    ├─ Broadcast: Use shared BroadcastChangeDispatcher
    │             Returns receiver from shared channel
    │
    └─ Channel:   Create new ChannelChangeDispatcher
                  Returns receiver from dedicated channel
```

**Event Flow**:
```
Query Result → Arc::new(QueryResult)
            → dispatcher.dispatch_change(arc)
            → Dispatcher (broadcast or mpsc)
            → Receiver
            → Reaction Priority Queue
            → Reaction Processing
```

### Bootstrap Flow (Separate)

```
┌────────┐         ┌──────────────────┐        ┌────────┐
│ Source │────────▶│ Bootstrap        │───────▶│ Query  │
│        │ spawn   │ Provider Task    │ mpsc   │        │
│        │         │ (async)          │        │        │
└────────┘         └──────────────────┘        └────────┘
                           │
                           │ BootstrapContext
                           ▼
                   ┌─────────────────┐
                   │ Any Provider:   │
                   │ - Postgres      │
                   │ - ScriptFile    │
                   │ - Platform      │
                   │ - Application   │
                   │ - NoOp          │
                   └─────────────────┘
```

Bootstrap is **completely separate** from streaming:
- Dedicated `mpsc` channel per query subscription
- Processed before streaming events
- All sources must complete before streaming begins
- Never mixed with streaming events

---

## Configuration System

### Configuration Schema

**File**: `src/config/schema.rs`

```yaml
server_core:
  id: "server-uuid"                    # Server identifier
  priority_queue_capacity: 10000       # Global default for queries/reactions
  dispatch_buffer_capacity: 1000       # Global default for sources/queries

sources:
  - id: "source1"
    source_type: "postgres"             # postgres | http | grpc | mock | platform | application
    auto_start: true                    # Start automatically (default: true)
    dispatch_buffer_capacity: 2000      # Override global
    dispatch_mode: "channel"            # channel | broadcast (default: channel)
    bootstrap_provider:                 # Optional
      type: postgres                    # postgres | scriptfile | platform | application | noop
    properties: {}                      # Source-specific config

queries:
  - id: "query1"
    query: "MATCH (n) RETURN n"
    query_language: "Cypher"            # Cypher | GQL
    sources: ["source1"]
    auto_start: true
    enable_bootstrap: true              # Default: true
    bootstrap_buffer_size: 10000        # Default: 10000
    priority_queue_capacity: 15000      # Override global
    dispatch_buffer_capacity: 2000      # Override global
    dispatch_mode: "channel"            # channel | broadcast (default: channel)
    joins: []                           # Optional synthetic joins

reactions:
  - id: "reaction1"
    reaction_type: "log"                # log | http | grpc | sse | application | platform | profiler
    queries: ["query1"]
    auto_start: true
    priority_queue_capacity: 20000      # Override global
    properties: {}                      # Reaction-specific config
```

### Configuration Hierarchy

Three-level hierarchy for capacity configuration:

1. **Component-Specific Override** (highest priority)
   - Set in individual source/query/reaction config
   - Takes precedence over all defaults

2. **Global Server Configuration** (medium priority)
   - Set in `server_core` section
   - Applied to components without specific overrides

3. **Hardcoded Defaults** (lowest priority)
   - `dispatch_mode`: Channel
   - `dispatch_buffer_capacity`: 1000
   - `priority_queue_capacity`: 10000
   - `auto_start`: true
   - `enable_bootstrap`: true
   - `bootstrap_buffer_size`: 10000

### Runtime Configuration

**File**: `src/config/runtime.rs`

The `RuntimeConfig` is created from `DrasiServerCoreConfig`:

```rust
impl From<DrasiServerCoreConfig> for RuntimeConfig {
    fn from(config: DrasiServerCoreConfig) -> Self {
        // Extract global defaults
        let global_priority_queue = config.server_core
            .priority_queue_capacity
            .unwrap_or(10000);
        let global_dispatch_capacity = config.server_core
            .dispatch_buffer_capacity
            .unwrap_or(1000);

        // Apply to sources
        sources.map(|mut s| {
            if s.dispatch_buffer_capacity.is_none() {
                s.dispatch_buffer_capacity = Some(global_dispatch_capacity);
            }
            s
        })

        // Apply to queries (both capacities)
        // Apply to reactions (priority queue only)
    }
}
```

### API Builder Pattern

**Files**: `src/api/builder.rs`, `src/api/source.rs`, `src/api/query.rs`

Programmatic configuration:

```rust
let core = DrasiServerCore::builder()
    .with_id("my-server")
    .with_priority_queue_capacity(20000)
    .with_dispatch_buffer_capacity(2000)
    .add_source(
        Source::postgres("pg-source")
            .host("localhost")
            .database("mydb")
            .bootstrap_provider(BootstrapProvider::postgres())
            .build()
    )
    .add_query(
        Query::cypher("query1")
            .query("MATCH (n) RETURN n")
            .from_source("pg-source")
            .enable_bootstrap(true)
            .build()
    )
    .add_reaction(
        Reaction::application("reaction1")
            .subscribe_to("query1")
            .build()
    )
    .build()
    .await?;

core.initialize().await?;
core.start().await?;
```

---

## Bootstrap Architecture

### Universal Bootstrap Provider System

**File**: `src/bootstrap/mod.rs`

**Key Principle**: Bootstrap is completely separate from source type. ANY source can use ANY bootstrap provider.

### Architecture

```
┌──────────────────────────────────────────────────────────┐
│  Source (any type)                                       │
│    │                                                      │
│    └── bootstrap_provider: Option<BootstrapProviderConfig>│
└──────────────────────────────────────────────────────────┘
                        │
                        ▼
┌──────────────────────────────────────────────────────────┐
│  BootstrapProviderFactory                                 │
│    creates provider based on config.type                 │
└──────────────────────────────────────────────────────────┘
                        │
                        ▼
┌──────────────────────────────────────────────────────────┐
│  BootstrapProvider Trait                                  │
│    async fn bootstrap(                                    │
│        request: BootstrapRequest,                         │
│        context: BootstrapContext,                         │
│        event_tx: BootstrapEventSender                     │
│    ) -> Result<usize>                                     │
└──────────────────────────────────────────────────────────┘
```

### Provider Implementations

**File**: `src/bootstrap/providers/`

| Provider | File | Description |
|----------|------|-------------|
| **PostgresBootstrapProvider** | `postgres.rs` | PostgreSQL snapshot via `COPY` with LSN coordination |
| **ScriptFileBootstrapProvider** | `script_file.rs` | JSONL files with nodes/relations (ideal for testing) |
| **ApplicationBootstrapProvider** | `application.rs` | Replays stored insert events from ApplicationSource |
| **PlatformBootstrapProvider** | `platform.rs` | Bootstraps from remote Query API via HTTP streaming |
| **NoOpBootstrapProvider** | `noop.rs` | Returns no data (default when no provider configured) |

### Bootstrap Context

```rust
pub struct BootstrapContext {
    pub source_config: SourceConfig,      // Full source configuration
    pub source_properties: HashMap<...>,  // Source-specific properties
    pub sequence_counter: Arc<AtomicU64>, // Global sequence counter
}
```

Context provides access to source configuration so providers can use source properties (e.g., database credentials for PostgreSQL provider).

### Mix-and-Match Example

```yaml
# HTTP source with PostgreSQL bootstrap
sources:
  - id: http_with_pg_bootstrap
    source_type: http                # Streams changes via HTTP
    bootstrap_provider:
      type: postgres                 # Bootstraps from PostgreSQL
    properties:
      host: localhost
      database: mydb                 # Used by postgres bootstrap provider
      port: 9000                     # Used by http source
      user: dbuser
      password: dbpass
```

### Bootstrap Event Types

**ScriptFile Format** (JSONL):

```jsonl
{"record_type": "Header", "sequence_number": 0}
{"record_type": "Node", "labels": ["Person"], "properties": {"id": "1", "name": "Alice"}}
{"record_type": "Relation", "type": "KNOWS", "start_node_id": "1", "end_node_id": "2"}
{"record_type": "Comment", "comment": "Ignored line"}
{"record_type": "Label", "label": "Checkpoint 1"}
{"record_type": "Finish"}
```

### Bootstrap Flow

1. Query subscribes to source with `enable_bootstrap=true`
2. Source creates dedicated bootstrap channel for this query
3. Source spawns bootstrap task (async, if provider configured)
4. Bootstrap provider sends events to `bootstrap_tx`
5. Query receives on `bootstrap_rx`, processes separately from streaming
6. Query tracks bootstrap completion per source
7. Streaming begins only after ALL sources complete bootstrap

---

## Component Lifecycle

### Two-Phase Lifecycle

**Initialization** (one-time):
```rust
let mut core = DrasiServerCore::new(config);
core.initialize().await?;
```

Steps:
1. Create managers (SourceManager, QueryManager, ReactionManager)
2. Load sources/queries/reactions from config
3. Start event processors
4. Mark as initialized

**Start/Stop** (repeatable):
```rust
core.start().await?;   // Start auto_start components
// ... running ...
core.stop().await?;    // Stop all running
core.start().await?;   // Can restart
```

Start steps:
1. Start sources (auto_start=true)
2. Start queries (auto_start=true, subscribes to sources)
3. Start reactions (auto_start=true, subscribes to queries)
4. Mark as running

Stop steps:
1. Stop reactions (unsubscribes from queries)
2. Stop queries (unsubscribes from sources)
3. Stop sources
4. Mark as stopped

### Component Status States

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
- Stopped → Starting → Running (normal start)
- Running → Stopping → Stopped (normal stop)
- Any → Error (on failure)

### Dynamic Component Management

Runtime operations:

```rust
// Add components (can auto-start)
core.create_source(source_config).await?;
core.create_query(query_config).await?;
core.create_reaction(reaction_config).await?;

// Remove components (stops if running)
core.remove_source("source-id").await?;
core.remove_query("query-id").await?;
core.remove_reaction("reaction-id").await?;

// Individual start/stop
core.start_source("source-id").await?;
core.stop_query("query-id").await?;
core.start_reaction("reaction-id").await?;
```

---

## File Structure

### Key Directories

```
src/
├── channels/
│   ├── dispatcher.rs          # ChangeDispatcher/ChangeReceiver traits & impls
│   ├── events.rs              # Event type definitions
│   └── priority_queue.rs      # Generic priority queue
├── sources/
│   ├── base.rs                # SourceBase abstraction
│   ├── manager.rs             # SourceManager
│   ├── postgres/              # PostgreSQL replication source
│   ├── http/                  # HTTP source
│   ├── grpc/                  # gRPC source
│   ├── mock/                  # Mock test source
│   ├── platform/              # Platform (Redis Streams) source
│   └── application/           # Application API source
├── queries/
│   ├── base.rs                # QueryBase abstraction
│   └── manager.rs             # QueryManager & DrasiQuery
├── reactions/
│   ├── manager.rs             # ReactionManager
│   ├── application/           # Application API reaction
│   ├── log/                   # Log reaction
│   ├── http/                  # HTTP reaction
│   ├── grpc/                  # gRPC reaction
│   ├── sse/                   # Server-Sent Events reaction
│   ├── platform/              # Platform reaction
│   └── profiler/              # Profiler reaction
├── bootstrap/
│   ├── mod.rs                 # BootstrapProvider trait, factory
│   └── providers/
│       ├── postgres.rs        # PostgreSQL bootstrap
│       ├── script_file.rs     # JSONL file bootstrap
│       ├── application.rs     # Application data bootstrap
│       ├── platform.rs        # Platform query API bootstrap
│       └── noop.rs            # No-op bootstrap
├── config/
│   ├── schema.rs              # Configuration schema
│   └── runtime.rs             # Runtime config processing
├── api/
│   ├── builder.rs             # DrasiServerCore builder
│   ├── source.rs              # Source builders
│   └── query.rs               # Query builders
└── server_core.rs             # Main orchestration
```

---

## Design Rationale

### Why Channel Mode is Default

**Channel Mode Advantages**:
1. **Better Isolation**: Each subscriber has dedicated channel, failures don't affect others
2. **No Lag Concerns**: MPSC channels never lag, guaranteed delivery order
3. **Lazy Creation**: Channels created only when needed, minimal overhead
4. **Simpler Reasoning**: 1-to-1 mapping easier to debug
5. **Production Ready**: Preferred for production deployments

**Broadcast Mode Use Cases**:
- Many subscribers (100+ reactions) to same source/query
- Memory efficiency more important than isolation
- Can tolerate lag (non-critical monitoring)

### Zero-Copy Message Sharing

**Pattern**:
```rust
// Wrap once at dispatch point
let event = Arc::new(SourceEventWrapper { ... });

// Share Arc across all dispatchers (zero-copy)
for dispatcher in dispatchers.iter() {
    dispatcher.dispatch_change(event.clone()).await?;
}
```

**Benefits**:
- Single allocation per event
- Cheap Arc clones (atomic reference count increment only)
- No serialization/deserialization
- Shared ownership across subscribers

**Types using Arc**:
- `Arc<SourceEventWrapper>`
- `Arc<QueryResult>`
- `Arc<BootstrapEvent>` (wrapped in event types)

### Lazy Dispatcher Creation (Channel Mode)

**Implementation**:
```rust
// Initialization (Channel mode)
if dispatch_mode == DispatchMode::Broadcast {
    // Create dispatcher upfront
    dispatchers.push(BroadcastChangeDispatcher::new(capacity));
}
// For channel mode: dispatchers vector starts empty

// Subscription (Channel mode)
DispatchMode::Channel => {
    // Create dispatcher on-demand
    let dispatcher = ChannelChangeDispatcher::new(capacity);
    let receiver = dispatcher.create_receiver()?;
    dispatchers.push(dispatcher);  // Add to list
    receiver
}
```

**Advantages**:
- No overhead if component never has subscribers
- Each subscription gets fresh dedicated channel
- Clear ownership: one dispatcher per subscriber

### Separation of Bootstrap vs Streaming

**Design Philosophy**:
- Bootstrap = initial data load (complete snapshot)
- Streaming = ongoing change events
- Never mix in same channel

**Implementation**:
- Bootstrap: Dedicated `mpsc::Sender<BootstrapEvent>` per query subscription
- Streaming: Dispatcher-based `ChangeReceiver<SourceEventWrapper>`
- Query receives both, processes bootstrap first

**Benefits**:
- Clear semantics: bootstrap complete before streaming
- Prevents out-of-order processing
- Different buffer sizes (bootstrap_buffer_size vs dispatch_buffer_capacity)
- Bootstrap can be retried without affecting streaming

### Base Classes for Code Reuse

**SourceBase** and **QueryBase** extract 50%+ of common functionality:
- Dispatcher lifecycle management
- Subscription handling
- Event dispatching with profiling
- Component status tracking
- Common start/stop patterns

**Benefits**:
- Reduced code duplication
- Consistent behavior across implementations
- Easier to maintain and test
- Simpler implementation of new sources/queries

### Direct Subscriptions (No Routers)

Previous architecture had centralized routers (SubscriptionRouter, DataRouter, BootstrapRouter). Current architecture uses **direct subscriptions**:

```rust
// Query subscribes directly to Source
let response = source.subscribe(query_id, ...).await?;

// Reaction subscribes directly to Query
let receiver = query.subscribe(reaction_id).await?;
```

**Advantages**:
- Simpler code paths
- Easier to trace event flow
- No centralized bottlenecks
- Better encapsulation (components own their dispatchers)

---

## Summary

The drasi-core server-core architecture provides:

- **Flexible Event Distribution**: Trait-based dispatchers with configurable modes (Channel/Broadcast)
- **Zero-Copy Messaging**: Arc-based sharing for performance
- **Pluggable Bootstrap**: Universal provider system, any source can use any provider
- **Direct Subscriptions**: Simple, traceable event flow
- **Priority Queue Processing**: Timestamp-ordered event processing
- **Three-Level Configuration**: Component → Global → Hardcoded defaults
- **Dynamic Management**: Runtime component add/remove/start/stop
- **Library Design**: Embedded in applications via programmatic APIs

The system is production-ready with sensible defaults (Channel mode, capacity limits) and comprehensive lifecycle management.
