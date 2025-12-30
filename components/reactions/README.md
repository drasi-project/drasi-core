# Reaction Developer Guide

This guide explains how to create custom reaction plugins for Drasi. Reactions are the output components that receive query results and deliver them to external systems or users.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [The Reaction Trait](#the-reaction-trait)
- [Using ReactionBase](#using-reactionbase)
- [Event Types](#event-types)
- [Priority Queue](#priority-queue)
- [Component Lifecycle](#component-lifecycle)
- [Creating a Reaction Plugin](#creating-a-reaction-plugin)
- [Configuration Patterns](#configuration-patterns)
- [Testing Reactions](#testing-reactions)
- [Key Files Reference](#key-files-reference)
- [Implementation Checklist](#implementation-checklist)
- [Best Practices](#best-practices)
- [Additional Resources](#additional-resources)

## Overview

Reactions are responsible for:

1. **Query Subscription**: Subscribing to one or more queries to receive result changes
2. **Result Processing**: Processing query results in timestamp order via priority queue
3. **Output Delivery**: Delivering results to external systems (HTTP, gRPC, logs, etc.)
4. **Lifecycle Management**: Starting, stopping, and reporting status

### Available Reaction Plugins

| Plugin | Description | Directory |
|--------|-------------|-----------|
| `drasi-reaction-log` | Console logging with template support | `log/` |
| `drasi-reaction-http` | HTTP POST to external endpoints | `http/` |
| `drasi-reaction-http-adaptive` | HTTP with adaptive batching | `http-adaptive/` |
| `drasi-reaction-grpc` | gRPC streaming delivery | `grpc/` |
| `drasi-reaction-grpc-adaptive` | gRPC with adaptive batching | `grpc-adaptive/` |
| `drasi-reaction-sse` | Server-Sent Events streaming | `sse/` |
| `drasi-reaction-application` | Programmatic/in-memory for embedded use | `application/` |
| `drasi-reaction-platform` | Redis Streams publisher for platform integration | `platform/` |
| `drasi-reaction-profiler` | Performance profiling and metrics | `profiler/` |

## Architecture

### Data Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           Reaction                                       │
│  ┌──────────────┐    ┌─────────────────┐    ┌────────────────────────┐  │
│  │ Query 1      │───▶│ Priority Queue  │───▶│ Reaction Plugin        │──┼──▶ HTTP
│  │              │    │ (timestamp      │    │ (your code)            │──┼──▶ gRPC
│  │ Query 2      │───▶│  ordered)       │    │                        │──┼──▶ Console
│  │              │    │                 │    └────────────────────────┘  │
│  │ Query N      │───▶│                 │                                │
│  └──────────────┘    └─────────────────┘                                │
└─────────────────────────────────────────────────────────────────────────┘
```

### Plugin Architecture

Each reaction plugin:
1. Defines its own typed configuration struct
2. Creates a `ReactionBase` instance using `ReactionBaseParams`
3. Implements the `Reaction` trait
4. Is passed to `DrasiLib` via `add_reaction()` which takes ownership

DrasiLib has no knowledge of which plugins exist - it only knows about the `Reaction` trait.

### Context-based Initialization

Reactions receive dependencies through a single `initialize()` method that provides a `ReactionRuntimeContext`:
- `status_tx` - Channel for reporting component status events (Starting, Running, Stopped, Error)
- `query_provider` - Query provider for accessing queries
- `state_store` - Optional persistent state storage (if configured)
- `reaction_id` - The reaction's unique identifier

The context is automatically provided by DrasiLib when the reaction is added via `add_reaction()`.

## The Reaction Trait

All reactions must implement the `Reaction` trait from `drasi_lib`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::{Reaction, QueryProvider};
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
pub trait Reaction: Send + Sync {
    /// Get the reaction's unique identifier
    fn id(&self) -> &str;

    /// Get the reaction type name (e.g., "http", "log", "sse")
    fn type_name(&self) -> &str;

    /// Get the reaction's configuration properties for inspection
    fn properties(&self) -> HashMap<String, serde_json::Value>;

    /// Get the list of query IDs this reaction subscribes to
    fn query_ids(&self) -> Vec<String>;

    /// Whether this reaction should auto-start when DrasiLib starts.
    /// Default is `true`. Override to return `false` if this reaction
    /// should only be started manually via `start_reaction()`.
    fn auto_start(&self) -> bool {
        true
    }

    /// Initialize the reaction with runtime context.
    /// Called automatically by DrasiLib when the reaction is added.
    /// The context provides access to status_tx, query_provider, and state_store.
    async fn initialize(&self, context: ReactionRuntimeContext);

    /// Start the reaction.
    /// The reaction should:
    /// 1. Subscribe to all configured queries (using injected QueryProvider)
    /// 2. Start its processing loop
    /// 3. Update its status to Running
    async fn start(&self) -> Result<()>;

    /// Stop the reaction, cleaning up all subscriptions and tasks
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the reaction
    async fn status(&self) -> ComponentStatus;
}
```

### Key Design Points

- **Context-based initialization**: Reactions receive a `ReactionRuntimeContext` via `initialize()` when added to DrasiLib. The context provides access to status_tx, query_provider, state_store, and other DrasiLib services.
- **Async lifecycle**: All lifecycle methods (`start`, `stop`) are async.
- **Multi-query subscription**: Reactions can subscribe to multiple queries via `query_ids()`.
- **Priority queue processing**: Results from all queries are merged in timestamp order.
- **Auto-start control**: Reactions can override `auto_start()` to control whether they start automatically.

### QueryProvider Trait

The `QueryProvider` trait provides decoupled access to queries:

```rust
#[async_trait]
pub trait QueryProvider: Send + Sync {
    /// Get a query instance by ID
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn Query>>;
}
```

## Using ReactionBase

`ReactionBase` encapsulates common functionality used across all reaction implementations:

- Query subscription management
- Priority queue handling
- Task lifecycle management
- Component status tracking
- Event reporting

### Creating a ReactionBase

```rust
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};

pub struct MyReaction {
    base: ReactionBase,
    config: MyReactionConfig,
}

impl MyReaction {
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: MyReactionConfig) -> Self {
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(10000);  // Optional, 10000 is default

        Self {
            base: ReactionBase::new(params),
            config,
        }
    }
}
```

### ReactionBaseParams Builder

```rust
pub struct ReactionBaseParams {
    pub id: String,
    pub queries: Vec<String>,
    pub priority_queue_capacity: Option<usize>,  // Default: 10000
    pub auto_start: bool,                        // Default: true
}

impl ReactionBaseParams {
    pub fn new(id: impl Into<String>, queries: Vec<String>) -> Self;
    pub fn with_priority_queue_capacity(self, capacity: usize) -> Self;
    pub fn with_auto_start(self, auto_start: bool) -> Self;
}
```

### Key ReactionBase Methods

```rust
impl ReactionBase {
    // Status management
    pub async fn get_status(&self) -> ComponentStatus;
    pub async fn set_status_with_event(&self, status: ComponentStatus, message: Option<String>) -> Result<()>;

    // Component lifecycle events
    pub async fn send_component_event(&self, status: ComponentStatus, message: Option<String>) -> Result<()>;

    // Context initialization (called automatically by DrasiLib)
    pub async fn initialize(&self, context: ReactionRuntimeContext);
    pub async fn query_provider(&self) -> Option<Arc<dyn QueryProvider>>;
    pub async fn state_store(&self) -> Option<Arc<dyn StateStoreProvider>>;

    // Query subscription - subscribes to all configured queries
    pub async fn subscribe_to_queries(&self) -> Result<()>;

    // Shutdown management
    pub async fn create_shutdown_channel(&self) -> tokio::sync::oneshot::Receiver<()>;
    pub async fn stop_common(&self) -> Result<()>;

    // Task management
    pub async fn set_processing_task(&self, task: tokio::task::JoinHandle<()>);

    // Auto-start configuration
    pub fn get_auto_start(&self) -> bool;

    // Getters
    pub fn get_id(&self) -> &str;
    pub fn get_queries(&self) -> &[String];

    // Cloning for spawned tasks
    pub fn clone_shared(&self) -> Self;
}
```

## Event Types

### QueryResult

The result type received from queries:

```rust
pub struct QueryResult {
    pub query_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<serde_json::Value>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub profiling: Option<ProfilingMetadata>,
}
```

Each result in `results` typically contains:
- `type`: One of `"add"`, `"update"`, `"remove"`/`"delete"`
- `data`: The result data (for add/delete)
- `before`/`after`: Previous and current values (for update)

### QuerySubscriptionResponse

What reactions receive when subscribing to a query:

```rust
pub struct QuerySubscriptionResponse {
    pub query_id: String,
    pub receiver: Box<dyn ChangeReceiver<QueryResult>>,
}
```

### ComponentStatus

```rust
pub enum ComponentStatus {
    Starting,  // Initializing
    Running,   // Active processing
    Stopping,  // Graceful shutdown
    Stopped,   // Not running
    Error,     // Fatal error
}
```

## Priority Queue

The priority queue provides timestamp-ordered processing of query results with backpressure support.

### Queue Operations

```rust
pub struct PriorityQueue<T> {
    // Enqueue operations
    pub async fn enqueue(&self, event: Arc<T>) -> bool;      // Non-blocking, returns false if full
    pub async fn enqueue_wait(&self, event: Arc<T>);         // Blocking, waits for space

    // Dequeue operations
    pub async fn dequeue(&self) -> Arc<T>;                   // Blocking, waits for event
    pub fn try_dequeue(&self) -> Option<Arc<T>>;             // Non-blocking

    // Queue management
    pub async fn drain(&self) -> Vec<Arc<T>>;
    pub fn depth(&self) -> usize;
    pub fn is_empty(&self) -> bool;

    // Metrics
    pub fn metrics(&self) -> PriorityQueueMetrics;
}
```

### Enqueue Strategies Based on Query Dispatch Mode

ReactionBase automatically chooses the appropriate enqueue strategy:

**Channel Mode (Default)** - `enqueue_wait()`:
- Blocks until space available
- Never drops events
- Provides backpressure from reaction to query
- Safe because each subscriber has isolated channel

**Broadcast Mode** - `enqueue()`:
- Non-blocking to prevent deadlock
- May drop events when queue is full
- Logs warning on drops
- Required because broadcast uses shared channel

### Queue Metrics

```rust
pub struct PriorityQueueMetrics {
    pub total_enqueued: u64,        // Total events enqueued
    pub total_dequeued: u64,        // Total events dequeued
    pub current_depth: usize,       // Current queue size
    pub max_depth_seen: usize,      // Peak queue utilization
    pub drops_due_to_capacity: u64, // Non-blocking rejects
    pub blocked_enqueue_count: u64, // Blocking wait count
}
```

## Component Lifecycle

Reactions follow a consistent state machine:

```
Stopped → Starting → Running → Stopping → Stopped
              ↓
            Error
```

### Status Transitions

Use `ReactionBase` methods for status management:

```rust
// Set status and send lifecycle event
self.base.set_status_with_event(
    ComponentStatus::Running,
    Some("Connected to API".to_string()),
).await?;
```

## Creating a Reaction Plugin

### Step 1: Project Structure

```
my-reaction/
├── Cargo.toml
└── src/
    ├── lib.rs      # Reaction implementation and builder
    └── config.rs   # Configuration struct
```

### Step 2: Cargo.toml

```toml
[package]
name = "drasi-reaction-my-reaction"
version = "0.1.0"
edition = "2021"

[dependencies]
drasi-lib = { path = "../../lib" }
anyhow = "1.0"
async-trait = "0.1"
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
log = "0.4"
chrono = { version = "0.4", features = ["serde"] }
```

### Step 3: Configuration

```rust
// src/config.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyReactionConfig {
    pub endpoint: String,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    5000
}

impl Default for MyReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:8080".to_string(),
            timeout_ms: default_timeout(),
        }
    }
}
```

### Step 4: Reaction Implementation

```rust
// src/lib.rs
mod config;

pub use config::MyReactionConfig;

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::{QueryProvider, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use log::{debug, info};
use std::collections::HashMap;
use std::sync::Arc;

pub struct MyReaction {
    base: ReactionBase,
    config: MyReactionConfig,
}

impl MyReaction {
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: MyReactionConfig,
    ) -> Self {
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create a builder for MyReaction
    pub fn builder(id: impl Into<String>) -> MyReactionBuilder {
        MyReactionBuilder::new(id)
    }
}

#[async_trait]
impl Reaction for MyReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "my-reaction"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert("endpoint".to_string(), serde_json::json!(self.config.endpoint));
        props.insert("timeout_ms".to_string(), serde_json::json!(self.config.timeout_ms));
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        info!("Starting MyReaction '{}'", self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QueryProvider was injected via inject_query_provider() when reaction was added
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Clone what we need for the spawned task
        let priority_queue = self.base.priority_queue.clone();
        let reaction_id = self.base.id.clone();
        let config = self.config.clone();

        // Spawn processing task to dequeue and process results
        let processing_task = tokio::spawn(async move {
            loop {
                // Use select to wait for either a result OR shutdown signal
                let query_result = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                // Process the result
                info!(
                    "[{}] Received {} results from query '{}'",
                    reaction_id,
                    query_result.results.len(),
                    query_result.query_id
                );

                // Your custom processing logic here
                for result in &query_result.results {
                    // Send to endpoint, transform data, etc.
                    debug!("[{reaction_id}] Processing: {}", result);
                }
            }

            info!("[{reaction_id}] Processing task completed");
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping MyReaction '{}'", self.base.id);

        // Use ReactionBase common stop functionality
        // This sends shutdown signal, aborts forwarder tasks, and drains queue
        self.base.stop_common().await?;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

/// Builder for MyReaction
pub struct MyReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: MyReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl MyReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: MyReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Connect this reaction to receive results from a query (alias for with_query)
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> MyReaction {
        let mut params = ReactionBaseParams::new(self.id, self.queries)
            .with_auto_start(self.auto_start);

        if let Some(capacity) = self.priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        MyReaction {
            base: ReactionBase::new(params),
            config: self.config,
        }
    }
}
```

## Configuration Patterns

### Programmatic Configuration (Required)

Reactions are always configured programmatically and passed to DrasiLib:

```rust
use drasi_lib::DrasiLib;
use my_reaction_plugin::{MyReaction, MyReactionConfig};

let config = MyReactionConfig {
    endpoint: "http://api.example.com/webhook".to_string(),
    timeout_ms: 10000,
};

let reaction = MyReaction::new(
    "my-reaction-1",
    vec!["query1".to_string(), "query2".to_string()],
    config,
);

let core = DrasiLib::builder()
    .with_reaction(reaction)  // Ownership transferred
    .build()
    .await?;
```

### Using the Builder Pattern (Recommended)

```rust
use drasi_lib::DrasiLib;
use my_reaction_plugin::MyReaction;

let reaction = MyReaction::builder("my-reaction-1")
    .with_query("query1")
    .with_query("query2")
    .with_endpoint("http://api.example.com/webhook")
    .with_timeout_ms(10000)
    .with_priority_queue_capacity(5000)
    .with_auto_start(true)
    .build();

let core = DrasiLib::builder()
    .with_reaction(reaction)
    .build()
    .await?;
```

### Multiple Reactions

```rust
let log_reaction = LogReaction::builder("logger")
    .with_query("query1")
    .build()?;

let http_reaction = HttpReaction::builder("webhook")
    .with_query("query1")
    .with_base_url("http://api.example.com")
    .build()?;

let core = DrasiLib::builder()
    .with_reaction(log_reaction)
    .with_reaction(http_reaction)
    .build()
    .await?;
```

## Testing Reactions

### Unit Test Pattern

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_reaction_creation() {
        let config = MyReactionConfig::default();
        let reaction = MyReaction::new(
            "test-reaction",
            vec!["query1".to_string()],
            config,
        );

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_reaction_builder() {
        let reaction = MyReaction::builder("test-reaction")
            .with_query("query1")
            .with_query("query2")
            .with_endpoint("http://test.example.com")
            .with_timeout_ms(3000)
            .build();

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1", "query2"]);

        let props = reaction.properties();
        assert_eq!(
            props.get("endpoint"),
            Some(&serde_json::json!("http://test.example.com"))
        );
    }

    #[tokio::test]
    async fn test_status_reporting() {
        use drasi_lib::context::ReactionRuntimeContext;

        let (status_tx, mut event_rx) = mpsc::channel(100);
        let reaction = MyReaction::builder("test-reaction")
            .with_query("query1")
            .build();

        // Create a mock query subscriber for the context
        struct MockQueryProvider;
        #[async_trait::async_trait]
        impl drasi_lib::QueryProvider for MockQueryProvider {
            async fn get_query_instance(&self, _id: &str) -> anyhow::Result<std::sync::Arc<dyn drasi_lib::Query>> {
                Err(anyhow::anyhow!("MockQueryProvider: query not found"))
            }
        }

        // Initialize with context
        let context = ReactionRuntimeContext::new(
            "test-reaction",
            status_tx,
            None,
            std::sync::Arc::new(MockQueryProvider),
        );
        reaction.base.initialize(context).await;

        // Manually trigger a status event
        reaction.base.set_status_with_event(
            ComponentStatus::Starting,
            Some("Test".to_string()),
        ).await.unwrap();

        // Verify event was sent
        let event = event_rx.try_recv().unwrap();
        assert_eq!(event.status, ComponentStatus::Starting);
    }
}
```

### Integration Testing

```rust
use drasi_lib::DrasiLib;
use my_reaction_plugin::MyReaction;

#[tokio::test]
async fn test_reaction_with_drasi() {
    let reaction = MyReaction::builder("test-reaction")
        .with_query("test-query")
        .build();

    let core = DrasiLib::builder()
        .with_reaction(reaction)
        .build()
        .await
        .unwrap();

    core.initialize().await.unwrap();
    core.start().await.unwrap();

    // Verify reaction is running
    let status = core.get_reaction_status("test-reaction").await.unwrap();
    assert_eq!(status, ComponentStatus::Running);

    core.stop().await.unwrap();
}
```

## Key Files Reference

| File | Description |
|------|-------------|
| `lib/src/reactions/traits.rs` | `Reaction` trait definition |
| `lib/src/reactions/common/base.rs` | `ReactionBase` implementation |
| `lib/src/reactions/manager.rs` | `ReactionManager` lifecycle management |
| `lib/src/channels/events.rs` | Event types (`QueryResult`, `ComponentStatus`, etc.) |
| `lib/src/channels/priority_queue.rs` | Priority queue implementation |
| `lib/src/channels/dispatcher.rs` | Dispatcher traits and dispatch modes |
| `lib/src/profiling/mod.rs` | Profiling metadata types |

## Implementation Checklist

When creating a new reaction plugin:

- [ ] Create project structure with `Cargo.toml`
- [ ] Define configuration struct in `config.rs`
- [ ] Implement `Reaction` trait methods:
  - [ ] `id()` - return reaction identifier
  - [ ] `type_name()` - return reaction type string
  - [ ] `properties()` - return config as HashMap
  - [ ] `query_ids()` - return list of subscribed query IDs
  - [ ] `auto_start()` - delegate to ReactionBase (optional, defaults to true)
  - [ ] `initialize(context)` - delegate to ReactionBase
  - [ ] `start()` - subscribe to queries and spawn processing task
  - [ ] `stop()` - use ReactionBase.stop_common() and update status
  - [ ] `status()` - delegate to ReactionBase
- [ ] Use `ReactionBase` for common functionality
- [ ] Use `subscribe_to_queries()` for query subscription
- [ ] Use `create_shutdown_channel()` for graceful termination
- [ ] Implement `tokio::select!` in processing loop for shutdown handling
- [ ] Handle status transitions properly
- [ ] Implement builder pattern for ergonomic construction
- [ ] Add unit tests
- [ ] Add integration tests
- [ ] Document configuration options

## Best Practices

### Builder Pattern

Implement a builder pattern for ergonomic reaction construction. This is the approach used by all built-in reaction plugins:

```rust
pub struct MyReactionBuilder {
    id: String,
    queries: Vec<String>,
    // Configuration fields with defaults
    endpoint: String,
    timeout_ms: u64,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl MyReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            endpoint: "http://localhost".to_string(),
            timeout_ms: 5000,
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Alias for with_query - makes the data flow clearer
    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    pub fn build(self) -> MyReaction {
        let mut params = ReactionBaseParams::new(self.id, self.queries)
            .with_auto_start(self.auto_start);
        if let Some(capacity) = self.priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        MyReaction {
            base: ReactionBase::new(params),
            config: MyReactionConfig { endpoint: self.endpoint, timeout_ms: self.timeout_ms },
        }
    }
}

impl MyReaction {
    pub fn builder(id: impl Into<String>) -> MyReactionBuilder {
        MyReactionBuilder::new(id)
    }
}
```

### Graceful Shutdown with tokio::select!

Always use `tokio::select!` with a shutdown receiver for graceful termination:

```rust
// In start():
let mut shutdown_rx = self.base.create_shutdown_channel().await;

let processing_task = tokio::spawn(async move {
    loop {
        tokio::select! {
            biased;  // Check shutdown first

            _ = &mut shutdown_rx => {
                debug!("[{reaction_id}] Shutdown signal received");
                break;
            }

            result = priority_queue.dequeue() => {
                // Process result
            }
        }
    }
});

self.base.set_processing_task(processing_task).await;
```

The `biased` keyword ensures the shutdown signal is checked first, enabling rapid shutdown even when the queue has pending events.

### Status Transitions

Follow the standard lifecycle state machine:

```
Stopped → Starting → Running → Stopping → Stopped
              ↓
            Error
```

Always send component events when transitioning states:

```rust
async fn start(&self) -> Result<()> {
    self.base.set_status_with_event(
        ComponentStatus::Starting,
        Some("Initializing connection".to_string()),
    ).await?;

    // ... initialization logic ...

    self.base.set_status_with_event(
        ComponentStatus::Running,
        Some("Connected successfully".to_string()),
    ).await?;

    Ok(())
}
```

### Processing Multiple Queries

When subscribing to multiple queries, `ReactionBase.subscribe_to_queries()` automatically:
1. Gets each query instance via QueryProvider
2. Subscribes to each query
3. Spawns forwarder tasks that enqueue results to the priority queue
4. Uses the appropriate enqueue strategy based on dispatch mode

Results are merged in timestamp order, ensuring consistent processing regardless of which query produced them.

### Error Handling in Processing Loop

Handle errors gracefully in the processing loop:

```rust
let processing_task = tokio::spawn(async move {
    loop {
        let result = tokio::select! {
            biased;
            _ = &mut shutdown_rx => break,
            r = priority_queue.dequeue() => r,
        };

        // Process each result item
        for item in &result.results {
            if let Err(e) = process_item(item).await {
                // Log error but continue processing
                error!("[{reaction_id}] Failed to process item: {e}");
                // Optionally: track error metrics, retry logic, etc.
            }
        }
    }
});
```

### Adaptive Batching (Advanced)

For high-throughput reactions, consider using `AdaptiveBatcher` which automatically adjusts batch size and timing based on traffic:

```rust
use drasi_lib::utils::adaptive_batcher::{AdaptiveBatcher, AdaptiveBatchConfig};

let batcher_config = AdaptiveBatchConfig {
    adaptive_min_batch_size: 1,
    adaptive_max_batch_size: 100,
    adaptive_window_size: 10,
    adaptive_batch_timeout_ms: 1000,
};

let batcher = AdaptiveBatcher::new(batcher_config);
```

See `http-adaptive/` and `grpc-adaptive/` for examples.

## Additional Resources

- See individual plugin directories for implementation examples
- Check `lib/CLAUDE.md` for library architecture details
- Review `components/sources/README.md` for source development patterns
- Review the parent `drasi-core/CLAUDE.md` for query engine information
