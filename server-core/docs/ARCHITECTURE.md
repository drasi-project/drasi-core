# Drasi Server-Core Architecture

This document provides a comprehensive overview of the drasi-core server-core architecture, including the relationships between components, channel communication patterns, lifecycle management, and bootstrap processes.

**Last Updated:** 2025-10-27
**Version:** 5.0 - Dispatcher-based architecture with configurable dispatch modes

---

## Table of Contents

1. [Overview](#overview)
2. [Component Relationships](#component-relationships)
3. [Dispatcher Architecture](#dispatcher-architecture)
4. [Channel Architecture](#channel-architecture)
5. [Configuration System](#configuration-system)
6. [Component Lifecycle](#component-lifecycle)
7. [Subscription Process](#subscription-process)
8. [Bootstrap Process](#bootstrap-process)
9. [Event Flow](#event-flow)
10. [Code References](#code-references)
11. [Areas of Concern](#areas-of-concern)
12. [Completed Migrations](#completed-migrations)

---

## Overview

The server-core implements a reactive, event-driven architecture for continuous query processing over streaming data. The system consists of three primary component types (**Sources**, **Queries**, **Reactions**), three managers (**SourceManager**, **QueryManager**, **ReactionManager**), and a **trait-based dispatcher infrastructure** for configurable event distribution patterns.

### Key Design Principles

- **Trait-Based Dispatching**: ✨ **NEW**
  - All event distribution uses `ChangeDispatcher` and `ChangeReceiver` traits
  - Configurable dispatch modes: Broadcast (1-to-N) or Channel (1-to-1)
  - Zero-copy distribution maintained through Arc wrapping
- **Direct Subscriptions**:
  - Queries subscribe directly to Sources via dispatcher-created receivers
  - Reactions subscribe directly to Queries via dispatcher-created receivers
  - No centralized routers (SubscriptionRouter, DataRouter, BootstrapRouter all removed)
- **Priority Queue Processing**:
  - Each Query maintains a priority queue for ordered source event processing
  - Each Reaction maintains a priority queue for ordered query result processing
- **Pluggable Bootstrap**: Bootstrap providers execute during source subscription
- **Silent Bootstrap**: Bootstrap results are processed but not sent to reactions
- **Three-Level Configuration Hierarchy**: Component-specific → Global defaults → Hardcoded fallbacks

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
│  │ - Vec of    │   │ - Vec of   │  │ - Priority  │             │
│  │   Change    │   │   Change   │  │   Queue     │             │
│  │   Dispatch- │   │   Dispatch-│  │ - Subscr.   │             │
│  │   ers       │   │   ers      │  │   Tasks     │             │
│  │ - Bootstrap │   │ - Priority │  │             │             │
│  │   Provider  │   │   Queue    │  │             │             │
│  │             │   │ - Subscr.  │  └──────▲──────┘             │
│  └──────┬──────┘   │   Tasks    │         │                     │
│         │          │ - Bootstrap│         │                     │
│         │          │   State    │         │                     │
│         │          └──────┬─────┘         │                     │
│         │                 │               │                     │
│         │ dispatcher      │ dispatcher    │ direct              │
│         │ .create_        │ .create_      │ subscribe()         │
│         │ receiver()      │ receiver()    │                     │
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
- `server-core/src/sources/manager.rs:36` - Source trait
- `server-core/src/queries/manager.rs:45` - QueryManager
- `server-core/src/reactions/manager.rs` - ReactionManager

---

## Dispatcher Architecture

✨ **NEW in Version 5.0**: The system now uses a **trait-based dispatcher pattern** that abstracts the underlying channel implementation, allowing configurable dispatch modes and improved code reuse.

### Core Traits

The dispatcher system is built on two fundamental traits:

```rust
// Sends changes to receivers
#[async_trait]
pub trait ChangeDispatcher<T>: Send + Sync {
    async fn dispatch_change(&self, change: Arc<T>) -> Result<()>;
    async fn dispatch_changes(&self, changes: Vec<Arc<T>>) -> Result<()>;
    fn create_receiver(&self) -> Result<Box<dyn ChangeReceiver<T>>>;
}

// Receives changes from dispatchers
#[async_trait]
pub trait ChangeReceiver<T>: Send + Sync {
    async fn recv(&mut self) -> Result<Arc<T>>;
}
```

**File**: `server-core/src/channels/dispatcher.rs:26-53`

### Dispatch Modes

```rust
pub enum DispatchMode {
    /// Single broadcast channel with multiple receivers (1-to-N fanout)
    Broadcast,
    /// Dedicated channel per subscriber (1-to-1 communication)
    Channel,
}
```

**File**: `server-core/src/channels/dispatcher.rs:9-16`

### Dispatcher Implementations

#### BroadcastChangeDispatcher

- **Pattern**: 1-to-N fanout using tokio broadcast channels
- **Use Case**: Multiple subscribers receiving identical data
- **Memory**: Single buffer shared across all receivers
- **Capacity**: Configurable via `broadcast_channel_capacity`
- **Lag Handling**: Logs warning and continues (non-fatal)

**Key Characteristics**:
- Created once during component initialization
- All subscribers share same channel
- Efficient for read-heavy workloads
- Handles receiver lag gracefully

**File**: `server-core/src/channels/dispatcher.rs:56-93`

#### ChannelChangeDispatcher

- **Pattern**: 1-to-1 dedicated communication using tokio mpsc channels
- **Use Case**: Individual subscribers with different processing rates
- **Memory**: Separate buffer per subscriber
- **Capacity**: Configurable via `broadcast_channel_capacity` (despite name)
- **Creation**: Lazy - created on-demand during subscription

**Key Characteristics**:
- Created per subscription
- Each subscriber gets dedicated channel
- Better backpressure control
- No lag between subscribers

**File**: `server-core/src/channels/dispatcher.rs:125-171`

### Integration with Components

#### SourceBase and QueryBase

Both `SourceBase` and `QueryBase` use identical patterns:

```rust
pub struct SourceBase {
    pub config: SourceConfig,
    pub status: Arc<RwLock<ComponentStatus>>,
    // NEW: Collection of dispatchers instead of single broadcast_tx
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<T> + Send + Sync>>>>,
    pub event_tx: ComponentEventSender,
    // ... other fields
}
```

**Initialization Logic**:

```rust
// Broadcast mode: Create single dispatcher upfront
if dispatch_mode == DispatchMode::Broadcast {
    let dispatcher = BroadcastChangeDispatcher::new(capacity);
    dispatchers.push(Box::new(dispatcher));
}
// Channel mode: Dispatchers created lazily during subscription
```

**File References**:
- `server-core/src/sources/base.rs:36-76` - SourceBase with dispatchers
- `server-core/src/queries/base.rs:36-101` - QueryBase with dispatchers

### Zero-Copy Event Distribution

The dispatcher pattern maintains zero-copy semantics through Arc wrapping:

```rust
// Event dispatch pattern used throughout
pub async fn dispatch_event(&self, wrapper: EventWrapper) -> Result<()> {
    // Arc-wrap once for zero-copy sharing
    let arc_wrapper = Arc::new(wrapper);

    // Send to all dispatchers (cheap Arc::clone)
    let dispatchers = self.dispatchers.read().await;
    for dispatcher in dispatchers.iter() {
        dispatcher.dispatch_change(arc_wrapper.clone()).await?;
    }
    Ok(())
}
```

**Benefits**:
- Event data allocated once
- Only Arc pointers (8 bytes) are cloned
- Identical memory layout across all receivers
- No data copying regardless of subscriber count

---

## Channel Architecture

The system uses a combination of **trait-based dispatchers** (for data distribution) and **system-wide channels** (for control and lifecycle events).

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

| Channel | Type | Purpose | Capacity | Status |
|---------|------|---------|----------|--------|
| `component_event_tx/rx` | mpsc | Component lifecycle events | 1000 | ✅ Active |
| `_control_tx/rx` | mpsc | Legacy control messages | 100 | ⚠️ Deprecated |
| `control_signal_tx/rx` | mpsc | Control signals | 100 | ⚠️ No consumers |

### Per-Component Dispatchers

Each source and query maintains its own collection of dispatchers:

#### Source Dispatchers

- **Type**: `Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>`
- **Dispatch Modes**:
  - **Broadcast**: Single `BroadcastChangeDispatcher` created at initialization
  - **Channel**: Multiple `ChannelChangeDispatcher` created on subscription
- **Event Format**: `Arc<SourceEventWrapper>` containing:
  - `source_id`: String
  - `event`: SourceEvent (Change/Control)
  - `timestamp`: DateTime<Utc>
  - `profiling`: Optional profiling metadata

#### Query Dispatchers

- **Type**: `Vec<Box<dyn ChangeDispatcher<QueryResult>>>`
- **Dispatch Modes**: Same as sources
- **Event Format**: `Arc<QueryResult>` containing:
  - `query_id`: String
  - `timestamp`: DateTime<Utc>
  - `results`: QueryResult data
  - `metadata`: HashMap<String, String>
  - `profiling`: Optional profiling metadata

### Subscription Pattern

The new subscription flow through dispatchers:

```rust
// Query subscribing to Source
async fn subscribe(&self, ...) -> Result<SubscriptionResponse> {
    let receiver = match self.config.dispatch_mode {
        DispatchMode::Broadcast => {
            // Get receiver from existing dispatcher
            self.dispatchers.read().await
                .first()
                .ok_or_else(|| anyhow!("No dispatcher"))?
                .create_receiver()?
        }
        DispatchMode::Channel => {
            // Create new dispatcher for this subscription
            let dispatcher = ChannelChangeDispatcher::new(capacity);
            let receiver = dispatcher.create_receiver()?;
            self.dispatchers.write().await.push(Box::new(dispatcher));
            receiver
        }
    };

    // Return receiver (trait object, not concrete type)
    Ok(SubscriptionResponse { receiver, ... })
}
```

---

## Configuration System

The configuration system now includes dispatch mode selection alongside capacity configuration.

### Dispatch Mode Configuration

```yaml
# Global default (optional)
server_core:
  priority_queue_capacity: 10000
  broadcast_channel_capacity: 1000

# Component-specific
sources:
  - id: high_volume_source
    source_type: postgres
    dispatch_mode: broadcast        # NEW: Choose dispatch pattern
    broadcast_channel_capacity: 5000

  - id: dedicated_source
    source_type: http
    dispatch_mode: channel          # NEW: 1-to-1 channels
    broadcast_channel_capacity: 500

queries:
  - id: aggregation_query
    dispatch_mode: broadcast        # Multiple reactions get same data
    broadcast_channel_capacity: 2000
    priority_queue_capacity: 20000
```

### Configuration Hierarchy

All configurations follow this three-level precedence:

1. **Component-Specific** (highest priority)
   - `dispatch_mode`: Broadcast or Channel
   - `broadcast_channel_capacity`: Buffer size
   - `priority_queue_capacity`: Queue size

2. **Global Server Configuration** (medium priority)
   - Applied to components without specific overrides
   - Set in `server_core` section

3. **Hardcoded Defaults** (lowest priority)
   - `dispatch_mode`: Broadcast (default)
   - `broadcast_channel_capacity`: 1000
   - `priority_queue_capacity`: 10000

### Memory Implications by Dispatch Mode

#### Broadcast Mode Memory

- **Single buffer** shared by all subscribers
- Memory usage: O(capacity × event_size)
- Example: 1000 capacity × 1KB events = ~1MB total

#### Channel Mode Memory

- **Separate buffer** per subscriber
- Memory usage: O(subscribers × capacity × event_size)
- Example: 10 subscribers × 500 capacity × 1KB = ~5MB total

### Performance Characteristics

| Aspect | Broadcast Mode | Channel Mode |
|--------|---------------|--------------|
| **Dispatch Cost** | O(1) - single send | O(n) - send to each |
| **Receiver Creation** | O(n) - clone receiver | O(1) - new channel |
| **Memory Usage** | Shared buffer | Per-subscriber buffer |
| **Lag Impact** | Affects all receivers | Isolated per receiver |
| **Backpressure** | Limited control | Per-subscriber control |
| **Best For** | Many identical consumers | Varied processing rates |

---

## Component Lifecycle

All components follow the same lifecycle, now with dispatcher management integrated:

### Lifecycle States

```rust
pub enum ComponentStatus {
    Starting,  // Initializing dispatchers
    Running,   // Dispatchers active
    Stopping,  // Closing dispatchers
    Stopped,   // Dispatchers closed
    Error,
}
```

### Dispatcher Lifecycle

1. **Creation Phase**:
   - Broadcast: Dispatcher created during component initialization
   - Channel: No dispatchers created initially

2. **Subscription Phase**:
   - Broadcast: Receivers created from existing dispatcher
   - Channel: New dispatcher created per subscription

3. **Running Phase**:
   - Events dispatched to all active dispatchers
   - Failed dispatches logged but don't affect others

4. **Shutdown Phase**:
   - Subscription tasks aborted
   - Dispatchers naturally close when dropped
   - Receivers detect closed channels

---

## Subscription Process

The subscription process now uses dispatcher-created receivers:

```
┌──────────────────────────────────────────────────────────────────┐
│ 1. Query.start() initiates subscription                          │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 2. For each source, call:                                        │
│    source.subscribe(query_id, enable_bootstrap, labels)          │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 3. Source determines dispatch mode from config                   │
│    - Broadcast: Use existing dispatcher                          │
│    - Channel: Create new dispatcher                              │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 4. Create receiver through dispatcher:                           │
│    let receiver = dispatcher.create_receiver()?                  │
│    Returns: Box<dyn ChangeReceiver<SourceEventWrapper>>          │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 5. If bootstrap enabled, create dedicated channel                │
│    (unchanged - still uses mpsc directly)                        │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 6. Return SubscriptionResponse with trait-based receiver         │
└──────────────────────────────────────────────────────────────────┘
                          │
                          ▼
┌──────────────────────────────────────────────────────────────────┐
│ 7. Query spawns forwarder task:                                  │
│    - Calls receiver.recv() (trait method)                        │
│    - Forwards Arc<SourceEventWrapper> to priority queue          │
│    - Continues until channel closed                              │
└──────────────────────────────────────────────────────────────────┘
```

---

## Bootstrap Process

The bootstrap process remains unchanged - it still uses dedicated mpsc channels directly, not dispatchers:

- Bootstrap providers execute during subscription
- Dedicated mpsc channel per bootstrap session
- Results processed but not dispatched (silent bootstrap)
- Uses direct channels for simplicity and isolation

---

## Event Flow

### Source to Query Event Flow (with Dispatchers)

```
┌──────────┐
│ Source   │ Generates SourceChange
└─────┬────┘
      │
      │ Wraps in SourceEventWrapper
      │ Wraps in Arc<SourceEventWrapper>
      │
      ▼
┌──────────────────┐
│ Source           │ for dispatcher in dispatchers:
│ Dispatchers      │   dispatcher.dispatch_change(arc_wrapper.clone())
└─────┬────────────┘
      │
      │ Dispatch Mode determines distribution
      ├────────────────┬────────────────┐
      ▼                ▼                ▼
  BROADCAST:      or  CHANNEL:
  Single channel      Dedicated channels
  Multiple receivers  One receiver each
      │                │
      ├──────┬─────────┤
      ▼      ▼         ▼
  ┌────────┐ ┌────────┐ ┌────────┐
  │Query 1 │ │Query 2 │ │Query N │
  │Receiver│ │Receiver│ │Receiver│
  └───┬────┘ └───┬────┘ └───┬────┘
      │          │          │
      │ receiver.recv() (trait method)
      │ Returns Arc<SourceEventWrapper>
      │
      ▼
┌──────────────────┐
│ Query Priority   │ Orders by timestamp
│ Queue            │
└─────┬────────────┘
      │
      │ Process through ContinuousQuery
      │
      ▼
┌──────────────────┐
│ Query            │ Dispatches results
│ Dispatchers      │ (same pattern as sources)
└──────────────────┘
```

---

## Code References

### Dispatcher System (NEW)
- **Trait Definitions**: `server-core/src/channels/dispatcher.rs:26-53`
- **DispatchMode Enum**: `server-core/src/channels/dispatcher.rs:9-16`
- **BroadcastChangeDispatcher**: `server-core/src/channels/dispatcher.rs:56-93`
- **ChannelChangeDispatcher**: `server-core/src/channels/dispatcher.rs:125-171`
- **Dispatcher Tests**: `server-core/src/channels/dispatcher.rs:202-347`

### Base Abstractions (UPDATED)
- **SourceBase**: `server-core/src/sources/base.rs:36-330`
  - Dispatcher initialization: lines 53-76
  - Subscribe with dispatchers: lines 85-141
  - Event dispatch: lines 232-250
- **QueryBase**: `server-core/src/queries/base.rs:36-200`
  - Same pattern as SourceBase
  - Dispatcher management: lines 78-174

### Configuration (UPDATED)
- **DispatchMode in Schema**: `server-core/src/config/schema.rs:72-73, 109-110`
- **Serialization Tests**: `server-core/src/config/schema.rs:350-365`

### Source Implementations (SIMPLIFIED)
All sources now use SourceBase for dispatcher management:
- **MockSource**: `server-core/src/sources/mock/mod.rs` (~150 lines, was 400+)
- **HttpSource**: `server-core/src/sources/http/adaptive.rs` (~120 lines)
- **GrpcSource**: `server-core/src/sources/grpc/mod.rs` (~100 lines)
- **PlatformSource**: `server-core/src/sources/platform/mod.rs` (~200 lines)

---

## Areas of Concern

### 🔴 Critical Concerns

#### 1. **Control Signals Still Not Consumed**
- Control signal channel exists but unused
- Should either implement consumers or remove entirely

#### 2. **Legacy Control Channel Still Present**
- `_control_tx` marked deprecated but not removed
- Consuming resources unnecessarily

### 🟡 Medium Concerns

#### 3. **Channel Mode Dispatcher Cleanup**
- Channel mode creates new dispatchers per subscription
- No explicit cleanup mechanism when subscriptions end
- Dispatchers remain in Vec until component stops
- **Impact**: Memory growth with many subscribe/unsubscribe cycles
- **Solution**: Add unsubscribe mechanism to remove unused dispatchers

#### 4. **Dispatch Mode Runtime Changes**
- Dispatch mode is set at component creation
- Cannot change mode without recreating component
- **Impact**: Requires component restart for mode changes
- **Solution**: Consider if runtime mode switching is needed

### 🟢 Minor Concerns

#### 5. **Naming Inconsistency**
- Config field `broadcast_channel_capacity` used for both modes
- In channel mode, it configures mpsc capacity
- **Impact**: Potential confusion
- **Solution**: Consider renaming to `channel_capacity`

#### 6. **Test Helper Methods**
- `test_subscribe()` methods exposed in public API
- Should be `#[cfg(test)]` only

---

## Completed Migrations

### ✅ Dispatcher Abstraction Implementation (2025-10-27)

#### **Change Dispatcher Pattern**
- **Status**: ✅ Complete
- **Implementation**:
  - Created `ChangeDispatcher` and `ChangeReceiver` traits
  - Implemented `BroadcastChangeDispatcher` and `ChannelChangeDispatcher`
  - Added `DispatchMode` enum with Broadcast/Channel options
  - Migrated all sources from `broadcast_tx` to dispatcher collections
  - Created `SourceBase` and `QueryBase` with shared dispatcher logic
- **Benefits**:
  - 50%+ code reduction in source implementations
  - Configurable dispatch patterns per component
  - Maintained zero-copy semantics
  - Improved testability through trait abstraction
- **Files Modified**: 40+ files
- **Testing**: Unit tests for both dispatcher implementations

### ✅ Capacity Configuration System (2025-10-22 to 2025-10-23)

[Previous completed migrations remain unchanged...]

---

## Summary of Current State

### What Works Well ✅

- **Trait-Based Dispatcher System**: ✨ **NEW**
  - Clean abstraction over channel implementations
  - Configurable dispatch modes (Broadcast vs Channel)
  - Maintained zero-copy through Arc wrapping
  - 50%+ code reduction in sources
- **Direct Subscriptions**: No routers, components connect directly
- **Configurable Capacities**: Three-level hierarchy for all settings
- **Priority Queue Processing**: Timestamp-ordered event processing
- **Bootstrap System**: All 5 providers working with dedicated channels
- **Component Lifecycle**: Consistent across all components

### What Needs Attention ⚠️

- Control signal infrastructure unused
- Legacy _control_tx channel still present
- Channel mode dispatcher cleanup strategy needed
- Config field naming (`broadcast_channel_capacity` for both modes)
- Test helper methods in public API

### Architecture Decisions ✔️

- **Dispatcher Pattern**: Trait-based abstraction for flexibility
- **Dispatch Modes**: Broadcast (1-to-N) and Channel (1-to-1)
- **Lazy Creation**: Channel dispatchers created on-demand
- **Zero-Copy**: Enforced through Arc<T> in trait signatures
- **Base Classes**: SourceBase and QueryBase for code reuse

### Performance Characteristics

- **Broadcast Mode**: O(1) dispatch, shared buffer, best for identical consumers
- **Channel Mode**: O(n) dispatch, isolated buffers, best for varied rates
- **Zero-Copy**: Arc wrapping ensures no data duplication
- **Type Safety**: Generic traits allow type-specific dispatchers

---

**Document Version**: 5.0
**Last Updated**: 2025-10-27
**Changes**: Added comprehensive Dispatcher Architecture section documenting the new trait-based dispatcher pattern, updated all diagrams to show dispatcher collections instead of broadcast_tx, documented dispatch modes and their trade-offs, updated code references for new base abstractions
**Previous Version**: 4.0 - Router-free architecture with configurable capacities
**Next Review**: When significant architectural changes occur