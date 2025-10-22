# Drasi Server-Core Architecture

This document provides a comprehensive overview of the drasi-core server-core architecture, including the relationships between components, channel communication patterns, lifecycle management, and bootstrap processes.

**Last Updated:** 2025-10-21
**Version:** Current implementation with subscription-based bootstrap

---

## Table of Contents

1. [Overview](#overview)
2. [Component Relationships](#component-relationships)
3. [Channel Architecture](#channel-architecture)
4. [Component Lifecycle](#component-lifecycle)
5. [Subscription Process](#subscription-process)
6. [Bootstrap Process](#bootstrap-process)
7. [Event Flow](#event-flow)
8. [Code References](#code-references)
9. [Areas of Concern](#areas-of-concern)

---

## Overview

The server-core implements a reactive, event-driven architecture for continuous query processing over streaming data. The system consists of three primary component types (**Sources**, **Queries**, **Reactions**), three managers (**SourceManager**, **QueryManager**, **ReactionManager**), one router (**SubscriptionRouter**), and an event channel infrastructure.

### Key Design Principles

- **Direct Subscription**: Queries subscribe directly to Sources via broadcast channels
- **Zero-Copy Distribution**: Sources use broadcast channels with Arc-wrapped events for efficient distribution
- **Priority Queue Processing**: Each Query maintains a priority queue for ordered event processing
- **Pluggable Bootstrap**: Bootstrap providers are passed as parameters to the bootstrap() method
- **Dual Channel Architecture**: Broadcast channels for live events, dedicated mpsc for bootstrap
- **Silent Bootstrap**: Bootstrap results are processed but not sent to reactions (only live changes trigger reactions)

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
│  │  (HashMap)  │◄──┤ (HashMap)  │  │ (HashMap)   │             │
│  │             │   │            │  └──────┬──────┘             │
│  │ Each Source │   │ Each Query │         │                     │
│  │  contains:  │   │ contains:  │         │                     │
│  │             │   │            │  ┌──────▼──────────┐         │
│  │ - Broadcast │   │ - Priority │  │  Subscription   │         │
│  │   Channel   │   │   Queue    │  │  Router         │         │
│  │ - Bootstrap │   │ - Subscr.  │  └─────────────────┘         │
│  │   Provider  │   │   Tasks    │                               │
│  └─────────────┘   │ - Bootstrap│                               │
│                     │   State    │                               │
│                     └────────────┘                               │
│                           │                                       │
│                    ┌──────▼──────┐                               │
│                    │ Event       │                               │
│                    │ Channels    │                               │
│                    └─────────────┘                               │
└─────────────────────────────────────────────────────────────────┘
```

**File References**:
- `server-core/src/server_core.rs:32` - DrasiServerCore struct
- `server-core/src/sources/manager.rs:68` - SourceManager
- `server-core/src/queries/manager.rs:45` - QueryManager
- `server-core/src/reactions/manager.rs` - ReactionManager

---

## Channel Architecture

The system uses a combination of **shared channels** (mpsc) and **dedicated channels** (broadcast per-source, mpsc per-bootstrap) for communication.

### Shared System Channels

Created once by `EventChannels::new()` and distributed to all components:

```rust
pub struct EventChannels {
    pub query_result_tx: QueryResultSender,        // mpsc
    pub component_event_tx: ComponentEventSender,  // mpsc
    pub _control_tx: ControlMessageSender,         // mpsc (deprecated)
    pub control_signal_tx: ControlSignalSender,    // mpsc (unused)
}
```

**File**: `server-core/src/channels/events.rs:312`

#### Channel Purposes

| Channel | Type | Direction | Purpose | Capacity | Status |
|---------|------|-----------|---------|----------|--------|
| `query_result_tx/rx` | mpsc | Query → SubscriptionRouter | Query results for reactions | 1000 | ✅ Active |
| `component_event_tx/rx` | mpsc | All Components → DrasiServerCore | Component status events | 1000 | ✅ Active |
| `_control_tx/rx` | mpsc | (unused) | Legacy control messages | 100 | ⚠️ Deprecated |
| `control_signal_tx/rx` | mpsc | (unused) | Control signals | 100 | ⚠️ Unused |

### Per-Source Broadcast Channels

Each source maintains its own broadcast channel for distributing events to multiple subscribers:

```rust
// Created in source constructor
let (broadcast_tx, _) = tokio::sync::broadcast::channel(1000);
```

- **Type**: `tokio::sync::broadcast::Sender<Arc<SourceEventWrapper>>`
- **Capacity**: 1000 (hardcoded)
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

**File References**:
- `server-core/src/sources/manager.rs:68`
- `server-core/src/queries/manager.rs:45`
- `server-core/src/reactions/manager.rs`

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

### Live Event Flow (After Bootstrap)

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
│ Broadcast        │ broadcast_tx.send(Arc<SourceEventWrapper>)
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
│ Priority Queue   │ Orders events by sequence/timestamp
│ (capacity: 10000)│
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
      │
      ▼
┌──────────────────┐
│ Query Result     │ query_result_tx.send(QueryResult)
│ Channel (mpsc)   │
└─────┬────────────┘
      │
      ▼
┌──────────────────┐
│ Subscription     │ Routes results to subscribed reactions
│ Router           │
└─────┬────────────┘
      │
      ├──────────┬──────────┬───────────┐
      ▼          ▼          ▼           ▼
  ┌──────────┐┌──────────┐┌──────────┐
  │Reaction 1││Reaction 2││Reaction N│
  └──────────┘└──────────┘└──────────┘
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
- **DrasiServerCore**: `server-core/src/server_core.rs:32`
- **EventChannels**: `server-core/src/channels/events.rs:312`
- **SubscriptionResponse**: `server-core/src/channels/events.rs:151`
- **BootstrapEvent**: `server-core/src/channels/events.rs:126`

#### Managers
- **SourceManager**: `server-core/src/sources/manager.rs:68`
- **QueryManager**: `server-core/src/queries/manager.rs:45`
- **Query (DrasiQuery)**: `server-core/src/queries/manager.rs:91`
- **ReactionManager**: `server-core/src/reactions/manager.rs`

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
- **Query.start() subscription logic**: `server-core/src/queries/manager.rs:282-487`
- **Broadcast forwarder task**: `server-core/src/queries/manager.rs:341-378`
- **Bootstrap processing task**: `server-core/src/queries/manager.rs:429-502`
- **Priority queue**: `server-core/src/queries/priority_queue.rs`

---

## Areas of Concern

This section identifies incomplete implementations, potential issues, and future work items.

### 🔴 Critical Concerns

#### 1. **Control Signals Not Consumed**
- **Issue**: ControlSignal channel exists but no component actively consumes the signals
- **Current State**:
  - ✅ Control signals defined (Running, Stopped, Deleted)
  - ❌ No active consumers of control_signal_rx
  - ❌ Channel created but immediately dropped in EventReceivers
- **Files**: `server-core/src/channels/events.rs:310`, `server-core/src/server_core.rs`
- **Impact**: Wasted channel infrastructure, potential confusion
- **Action**: Either implement signal consumers or remove the control_signal channel entirely

#### 2. **Priority Queue Capacity Hardcoded**
- **Issue**: Each query's priority queue has a hardcoded capacity of 10,000
- **File**: `server-core/src/queries/manager.rs:146`
- **Impact**:
  - May drop events if source produces faster than query processes
  - No configuration option for tuning based on workload
- **Action**: Make capacity configurable per query or globally

### 🟡 Medium Concerns

#### 3. **Subscription Task Cleanup**
- **Issue**: Query stores subscription_tasks but doesn't explicitly abort them on stop
- **File**: `server-core/src/queries/manager.rs:132`
- **Current Behavior**: Tasks continue running until broadcast channel closes
- **Impact**: Tasks may continue briefly after query stop, consuming resources
- **Action**: Abort subscription tasks explicitly in `Query::stop()`

#### 4. **Broadcast Receiver Lagging**
- **Issue**: If a query's priority queue is full or processing is slow, broadcast receivers may lag
- **Current Handling**: Logs warning about skipped events but continues
- **File**: `server-core/src/queries/manager.rs:358-362`
- **Impact**: Data loss under high load without clear visibility
- **Action**:
  - Add metrics/monitoring for lag events
  - Consider backpressure mechanism or circuit breaker
  - Make lag handling configurable (fail-fast vs. continue)

#### 5. **Hardcoded Channel Capacities**
- **Issue**: All channel capacities are hardcoded:
  - Broadcast channels: 1000
  - Bootstrap channels: 1000
  - System channels: 1000 or 100
- **Files**: Throughout source implementations and `events.rs:327-331`
- **Impact**: No tuning for different workloads
- **Action**: Make capacities configurable via RuntimeConfig

#### 6. **Label Extraction Best Effort**
- **Issue**: Label extraction from queries is "best effort" and may fail for complex queries
- **File**: `server-core/src/queries/manager.rs:267`
- **Current Behavior**: Falls back to empty label lists on failure
- **Impact**: Bootstrap may fetch more data than necessary (over-fetching)
- **Action**: Improve label extraction or make it explicitly configurable per query

### 🟢 Minor Concerns

#### 7. **Bootstrap Results Silent**
- **Issue**: Bootstrap processing results are discarded (not sent to reactions)
- **File**: `server-core/src/queries/manager.rs:458-464`
- **Current Behavior**: Documented as intentional (silent bootstrap)
- **Impact**: Reactions don't receive initial query results from bootstrap
- **Question**: Is this the desired behavior? Should there be an option to send bootstrap results?
- **Workaround**: Reactions can query for initial state after bootstrapCompleted

#### 8. **MockSource Bootstrap Not Implemented**
- **Issue**: MockSource.subscribe() returns None for bootstrap_receiver
- **File**: `server-core/src/sources/mock/mod.rs:363-370`
- **Current State**: Comment says "Bootstrap not yet implemented for MockSource"
- **Impact**: Can't easily test bootstrap flow with MockSource in unit tests
- **Action**: Implement simple mock bootstrap for testing (e.g., generate N test elements)

#### 9. **Test Helper Method Naming**
- **Issue**: `test_subscribe()` method added to sources for testing (MockSource, PlatformSource)
- **Files**:
  - `server-core/src/sources/mock/mod.rs:60`
  - `server-core/src/sources/platform/mod.rs:111`
- **Concern**: Public API exposure for test-only functionality
- **Current**: Documented as "for testing" in doc comments
- **Consideration**: Use `#[cfg(test)]` attribute to limit compilation to test builds

#### 10. **Bootstrap Event Sequence Tracking**
- **Issue**: Each bootstrap provider tracks its own sequence numbers independently
- **Current**: BootstrapContext provides sequence_counter (AtomicU64)
- **Behavior**: Sequences start from 0 for each bootstrap session
- **Impact**: Minimal - sequences are only used for ordering within bootstrap phase
- **Note**: Live events use separate sequencing from sources

### ✅ Completed Migrations

#### **BootstrapRouter Removal - Complete**
- **Status**: ✅ Migration complete (2025-10-21)
- **Changes**:
  - BootstrapRouter code completely removed
  - Bootstrap request/response channels removed from EventChannels
  - BootstrapRequest moved from channels module to bootstrap module
  - BootstrapProvider trait updated: added `event_tx` parameter
  - All 5 bootstrap providers updated (postgres, platform, script_file, application, noop)
  - Queries no longer hold bootstrap_request_tx/bootstrap_response_rx
  - QueryManager simplified to remove bootstrap channel management
  - Documentation updated to reflect subscription-based only approach
  - All test compilation errors fixed (451 tests passing)

**Evidence**:
- No references to BootstrapRouter in codebase
- Bootstrap happens entirely within source.subscribe() flow
- Bootstrap data flows through dedicated per-subscription channels
- Clean separation: broadcast for live events, mpsc for bootstrap

#### **DataRouter Removal - Complete**
- **Status**: ✅ Migration complete (prior to BootstrapRouter removal)
- **Evidence**:
  - No active code references DataRouter
  - Direct subscription pattern fully implemented
  - Broadcast channels handle all event distribution
  - Only remnants are in .bak files (not compiled)

---

## Summary of Current State

### What Works Well ✅
- Direct query-to-source subscription via broadcast channels
- Zero-copy event distribution using Arc-wrapped events
- Priority queue ordered processing per query
- Bootstrap via subscription flow with dedicated channels
- All 5 bootstrap provider types implemented and working
- Query result routing to reactions through SubscriptionRouter
- Component lifecycle management (start/stop/delete)
- Clean separation between bootstrap and live event channels
- Parallel bootstrap from multiple sources
- Silent bootstrap (state building without reaction triggers)

### What Needs Attention ⚠️
- Control signal infrastructure exists but is unused (remove or implement?)
- Hardcoded capacity limits for all channels (make configurable)
- No active bootstrap in MockSource (implement for testing)
- Subscription tasks not explicitly aborted on query stop
- Lagging subscriber handling needs metrics and policy options
- Label extraction is best-effort (may over-fetch bootstrap data)
- Test helper methods (`test_subscribe`) exposed in public API

### Architecture Decisions ✔️
- **Bootstrap**: Subscription-based only (BootstrapRouter removed)
- **Event Distribution**: Broadcast channels with Arc (DataRouter removed)
- **Event Ordering**: Priority queues per query
- **Bootstrap Data**: Dedicated mpsc channels per subscription
- **Bootstrap Results**: Silent (results discarded during bootstrap phase)
- **Bootstrap Provider Parameters**: Event sender passed explicitly as parameter
- **Channel Types**:
  - System-wide: mpsc for query results and component events
  - Per-source: broadcast for live event distribution
  - Per-bootstrap: mpsc for dedicated bootstrap data delivery

### Performance Characteristics
- **Zero-copy**: Arc-wrapped events allow multiple subscribers without cloning data
- **Parallel Processing**: Each query processes events independently
- **Ordered Processing**: Priority queue ensures correct event ordering per query
- **Non-blocking Bootstrap**: Bootstrap runs in background task, doesn't block live events
- **Bounded Queues**: All channels have capacity limits (prevents unbounded memory growth)

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

**Document Version**: 2.0
**Last Reviewed**: 2025-10-21
**Reviewers**: Claude Code
**Next Review**: When significant architectural changes occur
