# Drasi Server-Core Architecture

This document provides a comprehensive overview of the drasi-core server-core architecture, including the relationships between components, channel communication patterns, lifecycle management, and bootstrap processes.

**Last Updated:** 2025-10-23
**Version:** 4.0 - Router-free architecture with configurable channel and queue capacities

---

## Table of Contents

1. [Overview](#overview)
2. [Component Relationships](#component-relationships)
3. [Channel Architecture](#channel-architecture)
4. [Configuration System](#configuration-system)
5. [Component Lifecycle](#component-lifecycle)
6. [Subscription Process](#subscription-process)
7. [Bootstrap Process](#bootstrap-process)
8. [Event Flow](#event-flow)
9. [Code References](#code-references)
10. [Areas of Concern](#areas-of-concern)
11. [Completed Migrations](#completed-migrations)

---

## Overview

The server-core implements a reactive, event-driven architecture for continuous query processing over streaming data. The system consists of three primary component types (**Sources**, **Queries**, **Reactions**), three managers (**SourceManager**, **QueryManager**, **ReactionManager**), and a broadcast channel infrastructure for zero-copy event distribution.

### Key Design Principles

- **Direct Subscriptions**:
  - Queries subscribe directly to Sources via per-source broadcast channels
  - Reactions subscribe directly to Queries via per-query broadcast channels
  - No centralized routers (SubscriptionRouter, DataRouter, BootstrapRouter all removed)
- **Zero-Copy Distribution**: All event distribution uses broadcast channels with Arc-wrapped data
- **Priority Queue Processing**:
  - Each Query maintains a priority queue for ordered source event processing (configurable capacity)
  - Each Reaction maintains a priority queue for ordered query result processing (configurable capacity)
- **Pluggable Bootstrap**: Bootstrap providers execute during source subscription
- **Dual Channel Architecture per Source**: Broadcast channels for live events, dedicated mpsc for bootstrap
- **Silent Bootstrap**: Bootstrap results are processed but not sent to reactions (only live changes trigger reactions)
- **Three-Level Configuration Hierarchy**: Component-specific → Global defaults → Hardcoded fallbacks for all capacities

---

## Component Relationships

```
┌─────────────────────────────────────────────────────────────────┐
│                     DrasiServerCore                              │
│                                                                   │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐             │
│  │   Source    │  │    Query    │  │  Reaction   │             │
│  │   Manager   │  │   Manager   │  │   Manager   │             │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘             │
│         │                 │                 │                     │
│         │manages          │manages          │manages              │
│         │                 │                 │                     │
│  ┌──────▼──────┐   ┌─────▼──────┐  ┌──────▼──────┐             │
│  │  Sources    │   │  Queries   │  │ Reactions   │             │
│  │  (HashMap)  │   │ (HashMap)  │  │ (HashMap)   │             │
│  │             │   │            │  │             │             │
│  │ Each Source │   │ Each Query │  │ Each Reaction│             │
│  │  contains:  │   │ contains:  │  │  contains:  │             │
│  │             │   │            │  │             │             │
│  │ - Broadcast │   │ - Broadcast│  │ - Priority  │             │
│  │   TX (live) │   │   TX (res.)│  │   Queue     │             │
│  │ - Bootstrap │   │ - Priority │  │ - Subscr.   │             │
│  │   Provider  │   │   Queue    │  │   Tasks     │             │
│  │             │   │ - Subscr.  │  │             │             │
│  └──────┬──────┘   │   Tasks    │  └──────▲──────┘             │
│         │          │ - Bootstrap│         │                     │
│         │          │   State    │         │                     │
│         │          └──────┬─────┘         │                     │
│         │                 │               │                     │
│         │ broadcast       │ broadcast     │ direct              │
│         │ subscribe()     │ subscribe()   │ subscribe()         │
│         └────────────────►│               │                     │
│                           └───────────────┘                     │
│                                                                   │
│  ┌───────────────────────────────────────────────────┐         │
│  │  System Event Channels (EventChannels)            │         │
│  │  - component_event_tx (lifecycle events)          │         │
│  │  - control_signal_tx (coordination signals)       │         │
│  │  - _control_tx (deprecated)                       │         │
│  └───────────────────────────────────────────────────┘         │
└─────────────────────────────────────────────────────────────────┘
```

**File References**:
- `server-core/src/server_core.rs:32` - DrasiServerCore struct
- `server-core/src/sources/manager.rs:68` - SourceManager
- `server-core/src/queries/manager.rs:45` - QueryManager
- `server-core/src/reactions/manager.rs` - ReactionManager

---

## Channel Architecture

The system uses a combination of **system-wide channels** (mpsc) and **dedicated per-component broadcast channels** for zero-copy event distribution.

### System-Wide Channels (EventChannels)

Created once by `EventChannels::new()` and distributed to all components:

```rust
pub struct EventChannels {
    pub component_event_tx: ComponentEventSender,  // mpsc
    pub _control_tx: ControlMessageSender,         // mpsc (deprecated)
    pub control_signal_tx: ControlSignalSender,    // mpsc
}
```

**File**: `server-core/src/channels/events.rs:308-312`

#### System Channel Purposes

| Channel | Type | Direction | Purpose | Capacity | Status |
|---------|------|-----------|---------|----------|--------|
| `component_event_tx/rx` | mpsc | All Components → DrasiServerCore | Component lifecycle events (Starting, Running, Stopped, Error) | 1000 | ✅ Active |
| `_control_tx/rx` | mpsc | (unused) | Legacy control messages | 100 | ⚠️ Deprecated |
| `control_signal_tx/rx` | mpsc | Components → ? | Control signals (Running, Stopped, Deleted) | 100 | ⚠️ No active consumers |

### Per-Source Broadcast Channels

Each source maintains its own broadcast channel for distributing events to multiple subscribers:

```rust
// Created in source constructor with configurable capacity
let capacity = config.broadcast_channel_capacity.unwrap_or(1000);
let (broadcast_tx, _) = tokio::sync::broadcast::channel(capacity);
```

- **Type**: `tokio::sync::broadcast::Sender<Arc<SourceEventWrapper>>`
- **Capacity**: Configurable via three-level hierarchy (default: 1000)
  - Component level: `source_config.broadcast_channel_capacity`
  - Global level: `server_core.broadcast_channel_capacity`
  - Hardcoded fallback: 1000
- **Purpose**: Zero-copy distribution of SourceChange events to multiple queries
- **Event Format**: `Arc<SourceEventWrapper>` containing:
  - `source_id`: String
  - `event`: SourceEvent (Change/Control)
  - `timestamp`: DateTime<Utc>
  - `profiling`: Optional profiling metadata

**File Examples**:
- `server-core/src/sources/mock/mod.rs:45` - MockSource broadcast channel
- `server-core/src/sources/platform/mod.rs:96` - PlatformSource broadcast channel
- `server-core/src/sources/http/adaptive.rs:104` - HttpSource broadcast channel

### Per-Query Broadcast Channels

Each query maintains its own broadcast channel for distributing results to multiple reactions:

```rust
// Created in query constructor with configurable capacity
let capacity = config.broadcast_channel_capacity.unwrap_or(1000);
let (broadcast_tx, _) = tokio::sync::broadcast::channel(capacity);
```

- **Type**: `tokio::sync::broadcast::Sender<Arc<QueryResult>>`
- **Capacity**: Configurable via three-level hierarchy (default: 1000)
  - Component level: `query_config.broadcast_channel_capacity`
  - Global level: `server_core.broadcast_channel_capacity`
  - Hardcoded fallback: 1000
- **Purpose**: Zero-copy distribution of QueryResult to multiple reactions
- **Event Format**: `Arc<QueryResult>` containing:
  - `query_id`: String
  - `timestamp`: DateTime<Utc>
  - `results`: QueryResult (added/updated/removed records)
  - `sequence`: u64
  - `profiling`: Optional profiling metadata

**Subscription Method**: Reactions call `query.subscribe(reaction_id)` which returns a `QuerySubscriptionResponse` with `broadcast_receiver`

**File Examples**:
- `server-core/src/queries/manager.rs:837-851` - Query.subscribe() method
- `server-core/src/reactions/log/mod.rs:98-150` - Reaction subscribing to queries

### Per-Bootstrap Dedicated Channels

Created dynamically during query subscription when bootstrap is enabled:

```rust
// Created in source.subscribe() when enable_bootstrap=true
let (bootstrap_tx, bootstrap_rx) = mpsc::channel(1000);
```

- **Type**: `tokio::sync::mpsc` (unidirectional)
- **Capacity**: 1000 (hardcoded)
- **Purpose**: Dedicated channel for bootstrap data delivery
- **Event Format**: `BootstrapEvent`:
  ```rust
  pub struct BootstrapEvent {
      pub source_id: String,
      pub change: SourceChange,
      pub timestamp: DateTime<Utc>,
      pub sequence: u64,
  }
  ```
- **Lifecycle**: Created during subscribe(), closed when bootstrap completes

**File**: `server-core/src/channels/events.rs:126`

---

## Configuration System

The server-core implements a **three-level configuration hierarchy** for channel and queue capacities, providing flexible control over memory usage and performance characteristics.

### Configuration Hierarchy

All capacity configurations follow this precedence order:

1. **Component-Specific Configuration** (highest priority)
   - Set at the individual source, query, or reaction level
   - Overrides all other settings for that specific component

2. **Global Server Configuration** (medium priority)
   - Set in `server_core` section of configuration
   - Applies to all components unless overridden at component level

3. **Hardcoded Defaults** (lowest priority)
   - Built-in fallback values when no configuration is provided
   - Ensures system always has valid capacities

### Capacity Configuration Options

#### Priority Queue Capacity

Controls the size of priority queues used for event ordering:

- **Default**: 10,000 events
- **Configuration Levels**:
  - Component: `priority_queue_capacity` in QueryConfig or ReactionConfig
  - Global: `server_core.priority_queue_capacity` in DrasiServerCoreSettings
  - Fallback: 10,000 (hardcoded)
- **Applies To**:
  - Queries: Orders source events by timestamp
  - Reactions: Orders query results by timestamp

#### Broadcast Channel Capacity

Controls the buffer size for broadcast channels:

- **Default**: 1,000 events
- **Configuration Levels**:
  - Component: `broadcast_channel_capacity` in SourceConfig or QueryConfig
  - Global: `server_core.broadcast_channel_capacity` in DrasiServerCoreSettings
  - Fallback: 1,000 (hardcoded)
- **Applies To**:
  - Sources: Broadcasting events to multiple queries
  - Queries: Broadcasting results to multiple reactions

### Configuration Examples

#### Component-Specific Override

```yaml
server_core:
  # Global defaults
  priority_queue_capacity: 5000
  broadcast_channel_capacity: 500

sources:
  - id: high_volume_source
    source_type: postgres
    # Override for this specific source
    broadcast_channel_capacity: 10000  # Higher capacity for high-volume source
    properties:
      # ... source properties

queries:
  - id: complex_query
    # Override for this specific query
    priority_queue_capacity: 20000     # Larger queue for complex processing
    broadcast_channel_capacity: 2000   # More buffer for results
    # ... query definition

reactions:
  - id: critical_reaction
    # Override for this specific reaction
    priority_queue_capacity: 15000     # Ensure no drops for critical reaction
    # ... reaction definition
```

#### Global Configuration Only

```yaml
server_core:
  # These apply to all components unless overridden
  priority_queue_capacity: 8000
  broadcast_channel_capacity: 1500

# Components will use global settings
sources:
  - id: source1
    # Uses global broadcast_channel_capacity: 1500

queries:
  - id: query1
    # Uses global priority_queue_capacity: 8000
    # Uses global broadcast_channel_capacity: 1500

reactions:
  - id: reaction1
    # Uses global priority_queue_capacity: 8000
```

### Configuration Resolution Process

The configuration resolution happens in two phases:

1. **Schema to Runtime Conversion** (`config/runtime.rs`)
   - DrasiServerCoreConfig (schema) → RuntimeConfig conversion
   - Global defaults are applied to components without specific overrides
   - Ensures consistent configuration before component creation

2. **Component Instantiation**
   - Components read from RuntimeConfig
   - Apply `unwrap_or()` with hardcoded fallback as final safety net
   - Guarantees valid capacity even with missing configuration

### Memory Implications

#### Priority Queue Memory Usage

Approximate memory per queue:
- 10,000 capacity ≈ 2-5 MB (depending on event size)
- 50,000 capacity ≈ 10-25 MB
- 100,000 capacity ≈ 20-50 MB

#### Broadcast Channel Memory Usage

Approximate memory per channel:
- 1,000 capacity ≈ 200 KB - 1 MB (depending on event size)
- 5,000 capacity ≈ 1-5 MB
- 10,000 capacity ≈ 2-10 MB

### Performance Considerations

#### When to Increase Capacities

**Priority Queue Capacity**:
- Source produces events faster than query processes
- Query produces results faster than reaction processes
- Bursty workloads with temporary spikes
- Complex queries requiring more buffering

**Broadcast Channel Capacity**:
- Multiple slow subscribers
- Network latency between components
- Large event batches
- High-frequency event streams

#### When to Decrease Capacities

- Memory-constrained environments
- Predictable, steady workloads
- Fast processing with minimal buffering needs
- Preference for backpressure over buffering

### Testing Configuration

Comprehensive test coverage ensures configuration hierarchy works correctly:

- **Priority Queue Tests**: `tests/priority_queue_hierarchy_test.rs`
- **Broadcast Channel Tests**: `tests/broadcast_capacity_hierarchy_test.rs`

These tests validate:
- Component overrides take precedence
- Global defaults apply when no override exists
- Hardcoded fallbacks work when all configuration is missing
- Mixed scenarios with partial configuration

---

## Component Lifecycle

All components (Sources, Queries, Reactions) follow a consistent lifecycle managed by their respective managers.

### Lifecycle States

```rust
pub enum ComponentStatus {
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
}
```

**File**: `server-core/src/channels/events.rs:62`

### State Transitions

```
      create()        start()         stop()         delete()
Stopped ──────> Stopped ──────> Running ──────> Stopped ──────> [Removed]
                   │                │                │
                   │                │                │
                   └──> Starting ───┘                └──> Stopping
```

### Component Managers

Each manager maintains a `HashMap<String, Arc<ComponentType>>` for component storage:

- **SourceManager**: `HashMap<String, Arc<dyn Source>>`
- **QueryManager**: `HashMap<String, Arc<Query>>`
- **ReactionManager**: `HashMap<String, Arc<dyn Reaction>>`

**Configuration Resolution**: During component creation, managers use `RuntimeConfig` which has already resolved the three-level configuration hierarchy (component → global → hardcoded) for all capacity settings.

**File References**:
- `server-core/src/sources/manager.rs:68`
- `server-core/src/queries/manager.rs:45`
- `server-core/src/reactions/manager.rs`
- `server-core/src/config/runtime.rs:116-179` (configuration resolution)

---

## Subscription Process

Queries subscribe directly to sources to receive events. This process establishes both live event streaming and optional bootstrap data delivery.

### Subscription Flow

```
┌──────────────────────────────────────────────────────────────────┐
│ 1. Query.start() initiates subscription                          │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 2. For each required source, call:                               │
│    source.subscribe(query_id, enable_bootstrap, labels, labels)  │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 3. Source creates broadcast receiver for live events             │
│    let broadcast_receiver = broadcast_tx.subscribe();            │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 4. If bootstrap enabled and provider exists:                     │
│    a) Create dedicated bootstrap channel                         │
│    b) Spawn task to execute bootstrap provider                   │
│    c) Return Some(bootstrap_rx)                                  │
│    Otherwise: return None                                        │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 5. Source returns SubscriptionResponse {                         │
│      query_id, source_id,                                        │
│      broadcast_receiver,                                         │
│      bootstrap_receiver: Option<BootstrapEventReceiver>          │
│    }                                                             │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 6. Query spawns broadcast forwarder task:                        │
│    - Receives from broadcast_receiver                            │
│    - Forwards Arc<SourceEventWrapper> to priority queue          │
│    - Handles lagging (logs warning, continues)                   │
│    - Runs until broadcast channel closes                         │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 7. If bootstrap_receiver present, query spawns bootstrap task:   │
│    - Receives BootstrapEvents from bootstrap_rx                  │
│    - Processes through ContinuousQuery                           │
│    - Results are DISCARDED (silent bootstrap)                    │
│    - Emits bootstrapCompleted when channel closes                │
└──────────────────────────────────────────────────────────────────┘
```

**Code Reference**: `server-core/src/queries/manager.rs:282-487`

### Source.subscribe() Method Signature

```rust
async fn subscribe(
    &self,
    query_id: String,
    enable_bootstrap: bool,
    node_labels: Vec<String>,
    relation_labels: Vec<String>,
) -> Result<SubscriptionResponse>;
```

**File**: `server-core/src/sources/manager.rs:44`

---

## Bootstrap Process

Bootstrap provides initial data to queries before they process live changes. The implementation uses **subscription-based bootstrap** where bootstrap providers are executed as part of the source subscription flow.

### Bootstrap Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                    Bootstrap Architecture                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                   │
│  ┌────────────────┐                                              │
│  │ Bootstrap      │                                              │
│  │ Provider       │ (PostgreSQL, Platform, ScriptFile,          │
│  │ (Pluggable)    │  Application, Noop)                         │
│  └────────┬───────┘                                              │
│           │ bootstrap(request, context, event_tx)                │
│           │                                                       │
│  ┌────────▼───────┐           ┌──────────────┐                  │
│  │  Source        │           │  Bootstrap   │                  │
│  │                │  spawns   │  Task        │                  │
│  │  subscribe()   │───────────▶              │                  │
│  │  method        │           │  executes    │                  │
│  │                │           │  provider    │                  │
│  └────────────────┘           └──────┬───────┘                  │
│                                      │                           │
│                                      │ sends BootstrapEvents     │
│                                      ▼                           │
│                               ┌──────────────┐                   │
│                               │ Bootstrap    │                   │
│                               │ Channel      │                   │
│                               │ (mpsc)       │                   │
│                               └──────┬───────┘                   │
│                                      │                           │
│                                      │ receives events           │
│                                      ▼                           │
│                               ┌──────────────┐                   │
│                               │  Query       │                   │
│                               │  Bootstrap   │                   │
│                               │  Task        │                   │
│                               └──────────────┘                   │
└─────────────────────────────────────────────────────────────────┘
```

### Bootstrap Provider Trait

**Current Signature**:

```rust
#[async_trait]
pub trait BootstrapProvider: Send + Sync {
    /// Perform bootstrap operation for the given request
    /// Sends bootstrap events to the provided channel
    /// Returns the number of elements sent
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize>;
}
```

**File**: `server-core/src/bootstrap/mod.rs:99`

**Key Change**: Bootstrap providers now receive `event_tx` as a parameter instead of accessing it through context. This makes the data flow explicit and allows bootstrap to happen independently of source lifecycle.

### Bootstrap Context

Provides configuration and metadata to bootstrap providers:

```rust
pub struct BootstrapContext {
    pub server_id: String,
    pub source_config: Arc<SourceConfig>,
    pub source_id: String,
    sequence_counter: Arc<AtomicU64>,
}
```

**File**: `server-core/src/bootstrap/mod.rs:59`

### Bootstrap Provider Types

| Provider | Description | Configuration | File |
|----------|-------------|---------------|------|
| **Postgres** | Snapshots from PostgreSQL tables using snapshot isolation | `type: postgres` | `bootstrap/providers/postgres.rs` |
| **Application** | Replays stored insert events from memory | `type: application` | `bootstrap/providers/application.rs` |
| **ScriptFile** | Reads JSONL script files | `type: scriptfile, file_paths: [...]` | `bootstrap/providers/script_file.rs` |
| **Platform** | Fetches from remote Query API via HTTP streaming | `type: platform, query_api_url: ...` | `bootstrap/providers/platform.rs` |
| **Noop** | Returns no data (default) | `type: noop` | `bootstrap/providers/noop.rs` |

### Bootstrap Flow (Detailed)

```
┌──────────────────────────────────────────────────────────────────┐
│ 1. Query.start() determines bootstrap needed (via label          │
│    extraction or explicit configuration)                          │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 2. Query calls source.subscribe(enable_bootstrap=true)           │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 3. Source checks if bootstrap_provider exists                    │
│    If None: return SubscriptionResponse with bootstrap_receiver  │
│             = None                                               │
│    If Some: continue to step 4                                   │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 4. Source creates dedicated bootstrap channel:                   │
│    let (bootstrap_tx, bootstrap_rx) = mpsc::channel(1000);       │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 5. Source spawns async task to execute bootstrap:                │
│    tokio::spawn(async move {                                     │
│      let request = BootstrapRequest {                            │
│        query_id, node_labels, relation_labels, ...               │
│      };                                                           │
│      let context = BootstrapContext::new(...);                   │
│      provider.bootstrap(request, &context, bootstrap_tx).await   │
│    });                                                            │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 6. Bootstrap provider executes (in background task):             │
│    - Fetches/generates initial data based on labels              │
│    - For each element:                                           │
│      a) Create SourceChange::Insert                              │
│      b) Get sequence number from context                         │
│      c) Create BootstrapEvent                                    │
│      d) Send to bootstrap_tx channel                             │
│    - Returns total count when complete                           │
│    - Channel closes automatically when task ends                 │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 7. Source returns SubscriptionResponse immediately (doesn't      │
│    wait for bootstrap to complete):                              │
│    SubscriptionResponse {                                        │
│      broadcast_receiver: ...,                                    │
│      bootstrap_receiver: Some(bootstrap_rx)                      │
│    }                                                             │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 8. Query receives SubscriptionResponse and spawns bootstrap      │
│    processing task:                                              │
│    while let Some(bootstrap_event) = bootstrap_rx.recv() {       │
│      let results = continuous_query                              │
│        .process_source_change(bootstrap_event.change).await;     │
│      // Results are DISCARDED (silent bootstrap)                 │
│    }                                                             │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 9. When bootstrap channel closes (provider done):                │
│    - Query marks bootstrap phase Complete for that source        │
│    - Emits bootstrapCompleted control signal                     │
│    - Query begins processing live events from priority queue     │
└──────────────────────────────────────────────────────────────────┘
```

**Code Reference**: `server-core/src/queries/manager.rs:389-502`

### Bootstrap State Machine

Each query tracks bootstrap state per source:

```rust
pub enum BootstrapPhase {
    NotStarted,
    InProgress,
    Complete,
}
```

**File**: `server-core/src/queries/manager.rs:25`

This state is tracked in: `HashMap<String, BootstrapPhase>` stored in Query struct.

### Silent Bootstrap

Bootstrap results are **processed but not emitted** to reactions:
- Bootstrap events are processed through `ContinuousQuery.process_source_change()`
- Results are generated but then **discarded** (line 458-464 in manager.rs)
- Only `bootstrapCompleted` control signal is sent to reactions
- **Rationale**: Bootstrap builds initial query state without triggering reactions

If reactions need initial results, they must either:
1. Query the results explicitly after bootstrap completes
2. Be designed to handle incremental updates only

---

## Event Flow

### Source to Query Event Flow (Live Events After Bootstrap)

```
┌──────────┐
│ Source   │ Generates SourceChange (Insert/Update/Delete)
└─────┬────┘
      │
      │ Wraps in SourceEventWrapper
      │ Wraps in Arc<SourceEventWrapper>
      │
      ▼
┌──────────────────┐
│ Source Broadcast │ broadcast_tx.send(Arc<SourceEventWrapper>)
│ Channel          │ (zero-copy distribution)
└─────┬────────────┘
      │
      │ Multiple Query Subscribers
      ├──────────┬──────────┬───────────┐
      ▼          ▼          ▼           ▼
  ┌────────┐ ┌────────┐ ┌────────┐
  │Query 1 │ │Query 2 │ │Query N │
  │        │ │        │ │        │
  │Forward │ │Forward │ │Forward │
  │  Task  │ │  Task  │ │  Task  │
  └───┬────┘ └───┬────┘ └───┬────┘
      │          │          │
      │ Receives Arc<SourceEventWrapper>
      │ Enqueues into Priority Queue
      │
      ▼
┌──────────────────┐
│ Query Priority   │ Orders events by timestamp
│ Queue            │ (Configurable, default: 10000)
└─────┬────────────┘
      │
      │ Processing Task dequeues events
      │
      ▼
┌──────────────────┐
│ ContinuousQuery  │ process_source_change()
│ (drasi-core)     │
└─────┬────────────┘
      │
      │ Produces QueryResults
      │ Wraps in Arc<QueryResult>
      │
      ▼
┌──────────────────┐
│ Query Broadcast  │ broadcast_tx.send(Arc<QueryResult>)
│ Channel          │ (zero-copy distribution)
└─────┬────────────┘
      │
      │ Multiple Reaction Subscribers
      ├──────────┬──────────┬───────────┐
      ▼          ▼          ▼           ▼
  ┌────────┐ ┌────────┐ ┌────────┐
  │React 1 │ │React 2 │ │React N │
  │        │ │        │ │        │
  │Forward │ │Forward │ │Forward │
  │  Task  │ │  Task  │ │  Task  │
  └───┬────┘ └───┬────┘ └───┬────┘
      │          │          │
      │ Receives Arc<QueryResult>
      │ Enqueues into Priority Queue
      │
      ▼
┌──────────────────┐
│ Reaction Priority│ Orders results by timestamp
│ Queue            │ (Configurable, default: 10000)
└─────┬────────────┘
      │
      │ Processing Task dequeues results
      │
      ▼
┌──────────────────┐
│ Reaction Handler │ HTTP POST / gRPC call / Log / etc.
│                  │
└──────────────────┘
```

### Bootstrap Event Flow (During Bootstrap)

```
┌──────────┐
│Bootstrap │ Executes provider.bootstrap()
│ Provider │
└─────┬────┘
      │
      │ Generates BootstrapEvents
      │ (sequence tracked per-source)
      │
      ▼
┌──────────────────┐
│ Bootstrap        │ mpsc::channel (dedicated)
│ Channel          │
└─────┬────────────┘
      │
      │ Single Query Subscriber
      │
      ▼
┌──────────────────┐
│ Query Bootstrap  │ Receives BootstrapEvents
│ Processing Task  │
└─────┬────────────┘
      │
      │ while let Some(event) = rx.recv()
      │
      ▼
┌──────────────────┐
│ ContinuousQuery  │ process_source_change(event.change)
│ (drasi-core)     │
└─────┬────────────┘
      │
      │ Produces QueryResults
      │
      ▼
┌──────────────────┐
│ Results          │ ❌ DISCARDED
│ Discarded        │ (silent bootstrap)
└──────────────────┘

      When channel closes:
      ▼
┌──────────────────┐
│ Query Result     │ Emits bootstrapCompleted control signal
│ Channel          │
└──────────────────┘
```

---

## Code References

### Key Files and Line Numbers

#### Core Architecture
- **DrasiServerCore**: `server-core/src/server_core.rs` (main server struct)
- **EventChannels**: `server-core/src/channels/events.rs:308` (system-wide channels)
- **SubscriptionResponse**: `server-core/src/channels/events.rs:151` (source subscription response)
- **QuerySubscriptionResponse**: `server-core/src/channels/events.rs` (query subscription response)
- **BootstrapEvent**: `server-core/src/channels/events.rs:137`

#### Managers
- **SourceManager**: `server-core/src/sources/manager.rs`
- **QueryManager**: `server-core/src/queries/manager.rs`
- **Query (DrasiQuery)**: `server-core/src/queries/manager.rs:121`
- **ReactionManager**: `server-core/src/reactions/manager.rs`
- **Reaction trait**: `server-core/src/reactions/manager.rs:50`

#### Sources
- **Source trait**: `server-core/src/sources/manager.rs:34`
- **Source.subscribe()**: `server-core/src/sources/manager.rs:44`
- **MockSource**: `server-core/src/sources/mock/mod.rs:29`
- **PlatformSource**: `server-core/src/sources/platform/mod.rs:82`
- **HttpSource**: `server-core/src/sources/http/adaptive.rs:39`
- **PostgresSource**: `server-core/src/sources/postgres/mod.rs:68`

#### Bootstrap
- **BootstrapProvider trait**: `server-core/src/bootstrap/mod.rs:99`
- **BootstrapContext**: `server-core/src/bootstrap/mod.rs:59`
- **BootstrapRequest**: `server-core/src/bootstrap/mod.rs:17`
- **PostgresBootstrapProvider**: `server-core/src/bootstrap/providers/postgres.rs`
- **PlatformBootstrapProvider**: `server-core/src/bootstrap/providers/platform.rs`
- **ScriptFileBootstrapProvider**: `server-core/src/bootstrap/providers/script_file.rs`

#### Queries
- **Query trait**: `server-core/src/queries/manager.rs:103-118` (trait definition)
- **Query.subscribe() for reactions**: `server-core/src/queries/manager.rs:837-851`
- **Query.start() subscription logic**: `server-core/src/queries/manager.rs:275-544` (subscribing to sources)
- **Broadcast forwarder task**: `server-core/src/queries/manager.rs:342-388` (forwards source events to priority queue)
- **Bootstrap processing task**: `server-core/src/queries/manager.rs:397-538` (processes bootstrap events)
- **Priority queue**: `server-core/src/channels/priority_queue.rs` (generic priority queue implementation)
- **Query-specific priority queue**: `server-core/src/queries/priority_queue.rs` (type alias for SourceEventWrapper)

#### Reactions
- **Reaction trait**: `server-core/src/reactions/manager.rs:50-73` (defines start with DrasiServerCore parameter)
- **LogReaction example**: `server-core/src/reactions/log/mod.rs:32-94`
  - **LogReaction.start()**: `server-core/src/reactions/log/mod.rs:98-150` (subscribes to queries)
  - **Forwarder task**: `server-core/src/reactions/log/mod.rs:139-170` (forwards query results to priority queue)
  - **Processing task**: `server-core/src/reactions/log/mod.rs:172-240` (dequeues and processes results)
- **Reaction priority queue**: Uses generic `PriorityQueue<QueryResult>` from `server-core/src/channels/priority_queue.rs`

#### Configuration
- **DrasiServerCoreSettings**: `server-core/src/config/schema.rs:45-54` (global configuration)
- **SourceConfig**: `server-core/src/config/schema.rs:57-74` (source-level configuration)
- **QueryConfig**: `server-core/src/config/schema.rs:77-111` (query-level configuration)
- **ReactionConfig**: `server-core/src/config/schema.rs:130-146` (reaction-level configuration)
- **RuntimeConfig**: `server-core/src/config/runtime.rs:116-179` (configuration resolution)
- **Configuration Hierarchy Tests**:
  - **Priority Queue Tests**: `server-core/tests/priority_queue_hierarchy_test.rs`
  - **Broadcast Capacity Tests**: `server-core/tests/broadcast_capacity_hierarchy_test.rs`

---

## Areas of Concern

This section identifies incomplete implementations, potential issues, and future work items.

### 🔴 Critical Concerns

#### 1. **Control Signals Not Consumed**
- **Issue**: ControlSignal channel exists but no component actively consumes the signals
- **Current State**:
  - ✅ Control signals defined (Running, Stopped, Deleted)
  - ❌ No active consumers of control_signal_rx
  - ❌ Channel created in EventChannels but no receiver actively processes it
- **Files**: `server-core/src/channels/events.rs:308-312`, `server-core/src/server_core.rs`
- **Impact**: Wasted channel infrastructure, potential confusion
- **Action**: Either implement signal consumers or remove the control_signal channel entirely

#### 2. **Legacy Control Channel Deprecated**
- **Issue**: `_control_tx` channel exists with underscore prefix indicating it's unused
- **Current State**: Channel is created but marked deprecated via underscore naming
- **Files**: `server-core/src/channels/events.rs:310`
- **Impact**: Unused channel consuming 100 slots of capacity
- **Action**: Remove deprecated channel entirely from EventChannels struct


### 🟡 Medium Concerns

#### 3. **Subscription Task Cleanup**
- **Issue**: Both queries and reactions store subscription_tasks but may not explicitly abort them on stop
- **Files**:
  - `server-core/src/queries/manager.rs:132` - Query subscription tasks
  - `server-core/src/reactions/log/mod.rs:37` - Reaction subscription tasks
- **Current Behavior**: Tasks continue running until broadcast channel closes
- **Impact**: Tasks may continue briefly after component stop, consuming resources
- **Action**: Abort subscription tasks explicitly in `Query::stop()` and `Reaction::stop()`

#### 4. **Broadcast Receiver Lagging**
- **Issue**: If a query's priority queue is full or processing is slow, broadcast receivers may lag
- **Current Handling**: Logs warning about skipped events but continues
- **File**: `server-core/src/queries/manager.rs:358-362`
- **Impact**: Data loss under high load without clear visibility
- **Action**:
  - Add metrics/monitoring for lag events
  - Consider backpressure mechanism or circuit breaker
  - Make lag handling configurable (fail-fast vs. continue)


#### 5. **Label Extraction Best Effort**
- **Issue**: Label extraction from queries is "best effort" and may fail for complex queries
- **File**: `server-core/src/queries/manager.rs:267`
- **Current Behavior**: Falls back to empty label lists on failure
- **Impact**: Bootstrap may fetch more data than necessary (over-fetching)
- **Action**: Improve label extraction or make it explicitly configurable per query

### 🟢 Minor Concerns

#### 6. **Bootstrap Results Silent**
- **Issue**: Bootstrap processing results are discarded (not sent to reactions)
- **File**: `server-core/src/queries/manager.rs:458-464`
- **Current Behavior**: Documented as intentional (silent bootstrap)
- **Impact**: Reactions don't receive initial query results from bootstrap
- **Question**: Is this the desired behavior? Should there be an option to send bootstrap results?
- **Workaround**: Reactions can query for initial state after bootstrapCompleted

#### 7. **MockSource Bootstrap Not Implemented**
- **Issue**: MockSource.subscribe() returns None for bootstrap_receiver
- **File**: `server-core/src/sources/mock/mod.rs:363-370`
- **Current State**: Comment says "Bootstrap not yet implemented for MockSource"
- **Impact**: Can't easily test bootstrap flow with MockSource in unit tests
- **Action**: Implement simple mock bootstrap for testing (e.g., generate N test elements)

#### 8. **Test Helper Method Naming**
- **Issue**: `test_subscribe()` method added to sources for testing (MockSource, PlatformSource)
- **Files**:
  - `server-core/src/sources/mock/mod.rs:60`
  - `server-core/src/sources/platform/mod.rs:111`
- **Concern**: Public API exposure for test-only functionality
- **Current**: Documented as "for testing" in doc comments
- **Consideration**: Use `#[cfg(test)]` attribute to limit compilation to test builds

#### 9. **Bootstrap Event Sequence Tracking**
- **Issue**: Each bootstrap provider tracks its own sequence numbers independently
- **Current**: BootstrapContext provides sequence_counter (AtomicU64)
- **Behavior**: Sequences start from 0 for each bootstrap session
- **Impact**: Minimal - sequences are only used for ordering within bootstrap phase
- **Note**: Live events use separate sequencing from sources

---

## Completed Migrations

This section documents successfully completed architectural improvements and migrations.

### ✅ Capacity Configuration System (2025-10-22 to 2025-10-23)

#### **Priority Queue Capacity Configuration**
- **Status**: ✅ Complete (Commit a8c4300, 2025-10-22)
- **Implementation**:
  - Added `priority_queue_capacity` field to QueryConfig and ReactionConfig
  - Added global default in DrasiServerCoreSettings
  - Three-level hierarchy: Component → Global → Hardcoded (10,000)
- **Files Modified**: 27 files updated
- **Testing**: `tests/priority_queue_hierarchy_test.rs` (262 lines)
- **Default Behavior**: 10,000 if not specified

#### **Broadcast Channel Capacity Configuration**
- **Status**: ✅ Complete (Commit e012574, 2025-10-23)
- **Implementation**:
  - Added `broadcast_channel_capacity` to SourceConfig and QueryConfig
  - Added global default in DrasiServerCoreSettings
  - Three-level hierarchy: Component → Global → Hardcoded (1,000)
- **Files Modified**: 28 files updated
- **Testing**: `tests/broadcast_capacity_hierarchy_test.rs` (421 lines)
- **Default Behavior**: 1,000 if not specified

#### **Configuration Resolution Process**
- **Status**: ✅ Complete (Commit d67ff24, 2025-10-22)
- **Implementation**:
  - RuntimeConfig::from() applies three-level hierarchy
  - Global defaults propagate to components without overrides
  - Hardcoded fallbacks ensure safe operation
- **Location**: `config/runtime.rs:116-179`
- **Documentation**: Comprehensive README.md sections added

### ✅ All Routers Removed (2025-10-21 and earlier)

#### **All Routers Removed - Complete**
- **Status**: ✅ Migration complete (2025-10-21 and earlier)
- **Changes**:

1. **BootstrapRouter Removal** (2025-10-21):
   - BootstrapRouter code completely removed
   - Bootstrap request/response channels removed from EventChannels
   - BootstrapRequest moved from channels module to bootstrap module
   - BootstrapProvider trait updated: added `event_tx` parameter
   - All 5 bootstrap providers updated (postgres, platform, script_file, application, noop)
   - Bootstrap happens entirely within source.subscribe() flow
   - Bootstrap data flows through dedicated per-subscription mpsc channels

2. **DataRouter Removal** (prior to 2025-10-21):
   - DataRouter code completely removed
   - Direct query-to-source subscription pattern implemented
   - Each source has its own broadcast channel for zero-copy event distribution
   - Queries spawn forwarder tasks to receive from source broadcast channels

3. **SubscriptionRouter Removal** (2025-10-21):
   - SubscriptionRouter code completely removed
   - Direct reaction-to-query subscription pattern implemented
   - Each query has its own broadcast channel for zero-copy result distribution
   - Reactions spawn forwarder tasks to receive from query broadcast channels
   - Reactions access QueryManager via DrasiServerCore reference

**Evidence**:
- `/server-core/src/routers/mod.rs` contains only a comment: "SubscriptionRouter has been removed - reactions now subscribe directly to queries"
- No router references in codebase (all removed)
- EventChannels no longer contains query_result_tx/rx
- Clean separation: per-component broadcast for data, system-wide mpsc for control/lifecycle
- Reactions receive `Arc<DrasiServerCore>` in start() method for accessing QueryManager

---

## Summary of Current State

### What Works Well ✅
- **Direct Subscriptions Throughout**:
  - Queries subscribe directly to sources via per-source broadcast channels
  - Reactions subscribe directly to queries via per-query broadcast channels
  - No centralized routers - all components removed
- **Zero-Copy Distribution**: Arc-wrapped events for efficient multi-subscriber access
- **Configurable Capacity System**: ✨ NEW
  - Three-level hierarchy for all capacities (component → global → hardcoded)
  - Priority queue capacities fully configurable
  - Broadcast channel capacities fully configurable
  - Comprehensive test coverage for configuration hierarchy
- **Priority Queue Processing**:
  - Each query has a priority queue for timestamp-ordered source event processing
  - Each reaction has a priority queue for timestamp-ordered query result processing
  - Both with configurable capacities for workload tuning
- **Bootstrap via Subscription**: Bootstrap integrated into source.subscribe() flow with dedicated mpsc channels
- **All 5 Bootstrap Provider Types**: postgres, platform, script_file, application, noop - all working
- **Component Lifecycle Management**: Consistent start/stop/delete across all components
- **Clean Channel Separation**:
  - Per-component broadcast channels for data (zero-copy)
  - System-wide mpsc channels for control and lifecycle events
  - Dedicated mpsc channels for bootstrap data
- **Parallel Bootstrap**: Multiple sources can bootstrap simultaneously
- **Silent Bootstrap**: State building without triggering reactions

### What Needs Attention ⚠️
- Control signal infrastructure exists but is unused (remove or implement?)
- Legacy _control_tx channel deprecated but still exists (remove entirely?)
- No active bootstrap in MockSource (implement for testing)
- Subscription tasks not explicitly aborted on component stop (both queries and reactions)
- Lagging subscriber handling needs metrics and policy options
- Label extraction is best-effort (may over-fetch bootstrap data)
- Test helper methods (`test_subscribe`) exposed in public API

### Architecture Decisions ✔️
- **No Routers**: All routers removed - direct subscriptions throughout
  - BootstrapRouter removed - bootstrap via source.subscribe()
  - DataRouter removed - queries subscribe directly to sources
  - SubscriptionRouter removed - reactions subscribe directly to queries
- **Event Distribution**: Broadcast channels with Arc for zero-copy distribution
- **Event Ordering**: Priority queues per query (for sources) and per reaction (for queries)
- **Bootstrap Data**: Dedicated mpsc channels per subscription
- **Bootstrap Results**: Silent (results discarded during bootstrap phase)
- **Bootstrap Provider Parameters**: Event sender passed explicitly as parameter
- **Channel Types**:
  - System-wide: mpsc for component lifecycle and control signals (EventChannels)
  - Per-source: broadcast for live event distribution to queries
  - Per-query: broadcast for result distribution to reactions
  - Per-bootstrap: mpsc for dedicated bootstrap data delivery
- **Reaction Initialization**: Reactions receive `Arc<DrasiServerCore>` to access QueryManager

### Performance Characteristics
- **Zero-copy**: Arc-wrapped events allow multiple subscribers without cloning data
- **Parallel Processing**:
  - Each query processes events independently
  - Each reaction processes results independently
- **Ordered Processing**:
  - Priority queue ensures correct event ordering per query
  - Priority queue ensures correct result ordering per reaction
- **Non-blocking Bootstrap**: Bootstrap runs in background task, doesn't block live events
- **Bounded Queues**: All channels and priority queues have capacity limits (prevents unbounded memory growth)
- **Direct Communication**: No router overhead - components communicate point-to-point

### Testing Status
- ✅ 451 unit tests passing
- ✅ All compilation errors fixed
- ⚠️ 2 test failures (pre-existing, unrelated to bootstrap changes):
  - `test_bootstrap_with_joins` - channel closed issue
  - `test_load_config_from_file` - assertion mismatch
- ✅ Platform source integration tests compile (Arc pattern matching fixed)

---

## Future Considerations

### Potential Enhancements
1. **Configurable Capacities**: Make all channel capacities configurable via RuntimeConfig
2. **Backpressure Strategy**: Implement configurable strategies for handling slow consumers
3. **Bootstrap Metrics**: Add detailed metrics for bootstrap progress and performance
4. **Optional Bootstrap Results**: Add option to emit bootstrap results to reactions
5. **Improved Label Extraction**: More robust label extraction from complex queries
6. **Subscription Lifecycle**: Explicit cleanup and cancellation of subscription tasks
7. **Mock Bootstrap**: Implement bootstrap in MockSource for easier testing

### Architectural Questions
1. Should bootstrap results be optionally sent to reactions?
2. Should we remove unused control_signal channel infrastructure?
3. Should we make lagging behavior configurable (fail vs. warn)?
4. Should test helper methods be `#[cfg(test)]` only?

---

**Document Version**: 4.0
**Last Updated**: 2025-10-23
**Changes**: Added comprehensive Configuration System section, documented three-level capacity configuration hierarchy, moved resolved concerns to Completed Migrations, updated all diagrams to show configurable capacities
**Previous Version**: 3.0 - Complete router removal and direct subscriptions
**Reviewers**: Claude Code
**Next Review**: When significant architectural changes occur
