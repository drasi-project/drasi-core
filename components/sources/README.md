# Source Developer Guide

This guide explains how to create custom source plugins for Drasi. Sources are the data ingestion components that feed data changes into the Drasi query engine.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [The Source Trait](#the-source-trait)
- [Using SourceBase](#using-sourcebase)
- [Event Types](#event-types)
- [Dispatch Modes](#dispatch-modes)
- [Bootstrap Providers](#bootstrap-providers)
- [Component Lifecycle](#component-lifecycle)
- [Creating a Source Plugin](#creating-a-source-plugin)
- [Configuration Patterns](#configuration-patterns)
- [Testing Sources](#testing-sources)
- [Key Files Reference](#key-files-reference)

## Overview

Sources are responsible for:

1. **Data Ingestion**: Connecting to external systems and capturing data changes
2. **Event Dispatch**: Distributing change events to subscribing queries
3. **Bootstrap Support**: Providing initial data to new query subscribers
4. **Lifecycle Management**: Starting, stopping, and reporting status

### Available Source Plugins

| Plugin | Description | Directory |
|--------|-------------|-----------|
| `plugin-application` | Programmatic/in-memory sources for embedded use | `plugin-application/` |
| `plugin-grpc` | gRPC streaming data sources | `plugin-grpc/` |
| `plugin-http` | HTTP endpoint polling with adaptive batching | `plugin-http/` |
| `plugin-mock` | Test data generator for development | `plugin-mock/` |
| `plugin-platform` | Redis Streams consumer for platform integration | `plugin-platform/` |
| `plugin-postgres` | PostgreSQL WAL-based replication | `plugin-postgres/` |

## Architecture

### Data Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                          Source                                  │
│  ┌──────────────┐    ┌────────────────┐    ┌─────────────────┐  │
│  │ External     │───▶│ Source Plugin  │───▶│ Dispatchers     │──┼──▶ Query 1
│  │ System       │    │ (your code)    │    │ (SourceBase)    │──┼──▶ Query 2
│  │ (DB, API,    │    │                │    │                 │──┼──▶ Query N
│  │  Stream)     │    └────────────────┘    └─────────────────┘  │
│  └──────────────┘           │                                    │
│                             │                                    │
│                    ┌────────▼────────┐                          │
│                    │ Bootstrap       │                          │
│                    │ Provider        │──▶ Initial Data          │
│                    │ (optional)      │                          │
│                    └─────────────────┘                          │
└─────────────────────────────────────────────────────────────────┘
```

### Plugin Architecture

Each source plugin:
1. Defines its own typed configuration struct
2. Creates a `SourceBase` instance using `SourceBaseParams`
3. Implements the `Source` trait
4. Is passed to `DrasiLib` as `Arc<dyn Source>`

DrasiLib has no knowledge of which plugins exist - it only knows about the `Source` trait.

## The Source Trait

All sources must implement the `Source` trait from `drasi_lib::plugin_core`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::*;
use drasi_lib::plugin_core::Source;
use std::collections::HashMap;

#[async_trait]
pub trait Source: Send + Sync {
    /// Get the source's unique identifier
    fn id(&self) -> &str;

    /// Get the source type name (e.g., "postgres", "http", "mock")
    fn type_name(&self) -> &str;

    /// Get the source's configuration properties for inspection
    fn properties(&self) -> HashMap<String, serde_json::Value>;

    /// Get the dispatch mode for this source (Channel or Broadcast)
    /// Default is Channel mode for backpressure support.
    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    /// Start the source - begins data ingestion and event generation
    async fn start(&self) -> Result<()>;

    /// Stop the source - stops data ingestion and cleans up resources
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the source
    async fn status(&self) -> ComponentStatus;

    /// Subscribe to this source for change events
    /// Called by queries to receive data changes from this source.
    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse>;

    /// Downcast helper for testing - allows access to concrete types
    fn as_any(&self) -> &dyn std::any::Any;

    /// Inject the event channel for component lifecycle events
    /// Called automatically by DrasiLib when the source is added.
    async fn inject_event_tx(&self, tx: ComponentEventSender);
}
```

### Key Design Points

- **No event_tx in constructor**: Sources don't need the event channel during construction. DrasiLib automatically injects it when the source is added via `add_source()`.
- **Async lifecycle**: All lifecycle methods (`start`, `stop`, `subscribe`) are async.
- **Generic properties**: The `properties()` method returns a HashMap for API inspection, while the actual typed configuration is owned by the plugin.

## Using SourceBase

`SourceBase` encapsulates common functionality used across all source implementations:

- Dispatcher setup and management
- Bootstrap subscription handling
- Event dispatching with profiling
- Component lifecycle management

### Creating a SourceBase

```rust
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};

pub struct MySource {
    base: SourceBase,
    config: MySourceConfig,
}

impl MySource {
    pub fn new(id: impl Into<String>, config: MySourceConfig) -> Result<Self> {
        let params = SourceBaseParams::new(id)
            .with_dispatch_mode(DispatchMode::Channel)  // Optional, Channel is default
            .with_dispatch_buffer_capacity(2000);        // Optional, 1000 is default

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }
}
```

### SourceBaseParams Builder

```rust
pub struct SourceBaseParams {
    pub id: String,
    pub dispatch_mode: Option<DispatchMode>,        // Default: Channel
    pub dispatch_buffer_capacity: Option<usize>,    // Default: 1000
}

impl SourceBaseParams {
    pub fn new(id: impl Into<String>) -> Self;
    pub fn with_dispatch_mode(self, mode: DispatchMode) -> Self;
    pub fn with_dispatch_buffer_capacity(self, capacity: usize) -> Self;
}
```

### Key SourceBase Methods

```rust
impl SourceBase {
    // Status management
    pub async fn get_status(&self) -> ComponentStatus;
    pub async fn set_status(&self, status: ComponentStatus);
    pub async fn set_status_with_event(&self, status: ComponentStatus, message: Option<String>) -> Result<()>;

    // Event dispatching
    pub async fn dispatch_source_change(&self, change: SourceChange) -> Result<()>;
    pub async fn dispatch_event(&self, wrapper: SourceEventWrapper) -> Result<()>;
    pub async fn broadcast_control(&self, control: SourceControl) -> Result<()>;

    // For spawned tasks without &self access
    pub async fn dispatch_from_task(
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
        wrapper: SourceEventWrapper,
        source_id: &str,
    ) -> Result<()>;

    // Subscription handling
    pub async fn subscribe_with_bootstrap(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
        source_type: &str,
    ) -> Result<SubscriptionResponse>;

    pub async fn create_streaming_receiver(&self) -> Result<Box<dyn ChangeReceiver<SourceEventWrapper>>>;

    // Bootstrap provider
    pub async fn set_bootstrap_provider(&self, provider: Arc<dyn BootstrapProvider>);

    // Task management
    pub async fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>);
    pub async fn set_shutdown_tx(&self, tx: tokio::sync::oneshot::Sender<()>);
    pub async fn stop_common(&self) -> Result<()>;

    // Event channel (injected by DrasiLib)
    pub async fn inject_event_tx(&self, tx: ComponentEventSender);
    pub fn event_tx(&self) -> Arc<RwLock<Option<ComponentEventSender>>>;

    // Cloning for spawned tasks
    pub fn clone_shared(&self) -> Self;

    // Testing
    pub fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>>;
}
```

## Event Types

### SourceChange

The core data mutation type from `drasi_core::models`:

```rust
pub enum SourceChange {
    Insert { element: Element },
    Update { element: Element },
    Delete { metadata: ElementMetadata },
}

pub enum Element {
    Node {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
    },
    Relation {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
        out_node: ElementReference,  // Source node
        in_node: ElementReference,   // Target node
    },
}

pub struct ElementMetadata {
    pub reference: ElementReference,
    pub labels: Arc<[Arc<str>]>,
    pub effective_from: u64,  // Nanosecond timestamp
}

pub struct ElementReference {
    pub source_id: Arc<str>,
    pub element_id: Arc<str>,
}
```

### SourceEvent

The unified event envelope:

```rust
pub enum SourceEvent {
    Change(SourceChange),              // Data change
    Control(SourceControl),            // Query coordination
    BootstrapStart { query_id: String },
    BootstrapEnd { query_id: String },
}
```

### SourceEventWrapper

The event wrapper with metadata:

```rust
pub struct SourceEventWrapper {
    pub source_id: String,
    pub event: SourceEvent,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub profiling: Option<ProfilingMetadata>,
}

impl SourceEventWrapper {
    pub fn new(source_id: String, event: SourceEvent, timestamp: DateTime<Utc>) -> Self;
    pub fn with_profiling(source_id: String, event: SourceEvent, timestamp: DateTime<Utc>, profiling: ProfilingMetadata) -> Self;
}
```

### SubscriptionResponse

What queries receive when subscribing:

```rust
pub struct SubscriptionResponse {
    pub query_id: String,
    pub source_id: String,
    pub receiver: Box<dyn ChangeReceiver<SourceEventWrapper>>,
    pub bootstrap_receiver: Option<BootstrapEventReceiver>,
}
```

## Dispatch Modes

Sources support two modes for distributing events to subscribers:

### Channel Mode (Default)

Creates a dedicated MPSC channel per subscriber.

```
Source ─┬─▶ [Channel 1] ─▶ Query 1
        ├─▶ [Channel 2] ─▶ Query 2
        └─▶ [Channel 3] ─▶ Query 3
```

**Characteristics:**
- Backpressure support - sources wait if subscribers are slow
- Zero message loss
- Subscribers process independently
- Higher memory usage (buffer × subscribers)

**When to use:**
- Subscribers have different processing speeds
- Message loss is unacceptable
- Few subscribers (1-5)

### Broadcast Mode

Uses a single shared broadcast channel.

```
Source ─▶ [Broadcast Channel] ─┬─▶ Query 1
                               ├─▶ Query 2
                               └─▶ Query 3
```

**Characteristics:**
- No backpressure - fast send, receivers can lag
- Possible message loss when receivers fall behind
- Lower memory usage (single shared buffer)
- All subscribers receive all events

**When to use:**
- High fanout (10+ subscribers)
- All subscribers process at similar speeds
- Can tolerate some message loss
- Memory is constrained

### Configuration

```rust
let params = SourceBaseParams::new("my-source")
    .with_dispatch_mode(DispatchMode::Channel)  // or Broadcast
    .with_dispatch_buffer_capacity(1000);
```

Or in YAML:

```yaml
sources:
  - id: my-source
    source_type: my_type
    dispatch_mode: channel  # or broadcast
    dispatch_buffer_capacity: 2000
```

## Bootstrap Providers

Bootstrap provides initial data to newly subscribing queries. The bootstrap system is completely separated from streaming logic.

### Bootstrap Flow

1. Query subscribes with `enable_bootstrap: true`
2. Source delegates to configured bootstrap provider
3. Provider sends `BootstrapEvent` items through dedicated channel
4. Query processes bootstrap events before streaming events

### BootstrapProvider Trait

```rust
#[async_trait]
pub trait BootstrapProvider: Send + Sync {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize>;  // Returns count of events sent
}

pub struct BootstrapRequest {
    pub query_id: String,
    pub node_labels: Vec<String>,
    pub relation_labels: Vec<String>,
    pub request_id: String,
}

pub struct BootstrapContext {
    pub server_id: String,
    pub source_id: String,
    pub sequence_counter: Arc<AtomicU64>,
    // ... properties
}

pub struct BootstrapEvent {
    pub source_id: String,
    pub change: SourceChange,
    pub timestamp: DateTime<Utc>,
    pub sequence: u64,
}
```

### Available Bootstrap Providers

| Type | Description | Configuration |
|------|-------------|---------------|
| `postgres` | Database snapshot via COPY protocol | Uses source connection config |
| `application` | Replays stored insert events | Uses shared state from source |
| `scriptfile` | Reads from JSONL files | `file_paths: [...]` |
| `platform` | HTTP streaming from remote Drasi | `query_api_url`, `timeout_seconds` |
| `noop` | Returns no data | None |

### Registering a Bootstrap Provider

```rust
impl MySource {
    pub async fn set_bootstrap_provider(&self, provider: Arc<dyn BootstrapProvider>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}
```

### Mix-and-Match

Any source can use any bootstrap provider:

```yaml
sources:
  # HTTP streaming with PostgreSQL bootstrap
  - id: http_with_postgres_bootstrap
    source_type: http
    bootstrap_provider:
      type: postgres
    properties:
      host: localhost
      database: mydb
      # ...

  # Mock source with file bootstrap for testing
  - id: test_source
    source_type: mock
    bootstrap_provider:
      type: scriptfile
      file_paths: ["/data/test.jsonl"]
```

## Component Lifecycle

Sources follow a consistent state machine:

```
Stopped → Starting → Running → Stopping → Stopped
              ↓
            Error
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

### Status Transitions

Use `SourceBase` methods for status management:

```rust
// Set status only
self.base.set_status(ComponentStatus::Running).await;

// Set status and send lifecycle event
self.base.set_status_with_event(
    ComponentStatus::Running,
    Some("Connected to database".to_string()),
).await?;
```

## Creating a Source Plugin

### Step 1: Project Structure

```
plugin-my-source/
├── Cargo.toml
└── src/
    ├── lib.rs      # Source implementation
    └── config.rs   # Configuration struct
```

### Step 2: Cargo.toml

```toml
[package]
name = "drasi-plugin-my-source"
version = "0.1.0"
edition = "2021"

[dependencies]
drasi-lib = { path = "../../lib" }
drasi-core = { path = "../../../core" }
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
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MySourceConfig {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub interval_ms: u64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub properties: HashMap<String, serde_json::Value>,
}

impl Default for MySourceConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 8080,
            interval_ms: 1000,
            properties: HashMap::new(),
        }
    }
}
```

### Step 4: Source Implementation

```rust
// src/lib.rs
mod config;

pub use config::MySourceConfig;

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::channels::*;
use drasi_lib::plugin_core::Source;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use log::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;

pub struct MySource {
    base: SourceBase,
    config: MySourceConfig,
}

impl MySource {
    pub fn new(id: impl Into<String>, config: MySourceConfig) -> Result<Self> {
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    pub fn with_dispatch(
        id: impl Into<String>,
        config: MySourceConfig,
        dispatch_mode: Option<DispatchMode>,
        buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        let mut params = SourceBaseParams::new(id);
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }
}

#[async_trait]
impl Source for MySource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "my-source"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert("host".to_string(), serde_json::json!(self.config.host));
        props.insert("port".to_string(), serde_json::json!(self.config.port));
        props
    }

    async fn start(&self) -> Result<()> {
        info!("Starting MySource '{}'", self.base.id);
        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(ComponentStatus::Starting, Some("Initializing".to_string()))
            .await?;

        // Clone what we need for the spawned task
        let dispatchers = self.base.dispatchers.clone();
        let source_id = self.base.id.clone();
        let status = self.base.status.clone();
        let config = self.config.clone();

        // Spawn the main processing task
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_millis(config.interval_ms)
            );
            let mut seq = 0u64;

            loop {
                interval.tick().await;

                // Check if we should stop
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                seq += 1;

                // Create a source change
                let element_id = format!("item_{}", seq);
                let reference = ElementReference::new(&source_id, &element_id);

                let mut properties = ElementPropertyMap::new();
                properties.insert(
                    "value",
                    drasi_core::models::ElementValue::Integer(seq as i64),
                );
                properties.insert(
                    "timestamp",
                    drasi_core::models::ElementValue::String(
                        chrono::Utc::now().to_rfc3339().into()
                    ),
                );

                let metadata = ElementMetadata {
                    reference,
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as u64,
                };

                let element = Element::Node { metadata, properties };
                let source_change = SourceChange::Insert { element };

                // Create profiling metadata
                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    source_id.clone(),
                    SourceEvent::Change(source_change),
                    chrono::Utc::now(),
                    profiling,
                );

                // Dispatch to subscribers
                if let Err(e) = SourceBase::dispatch_from_task(
                    dispatchers.clone(),
                    wrapper,
                    &source_id,
                ).await {
                    debug!("Failed to dispatch: {}", e);
                }
            }

            info!("MySource task completed");
        });

        *self.base.task_handle.write().await = Some(task);
        self.base.set_status(ComponentStatus::Running).await;
        self.base
            .send_component_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping MySource '{}'", self.base.id);
        self.base.set_status(ComponentStatus::Stopping).await;
        self.base
            .send_component_event(ComponentStatus::Stopping, Some("Stopping".to_string()))
            .await?;

        // Cancel the task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(ComponentStatus::Stopped, Some("Stopped".to_string()))
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        // Delegate to SourceBase for standard handling
        self.base
            .subscribe_with_bootstrap(
                query_id,
                enable_bootstrap,
                node_labels,
                relation_labels,
                "MySource",
            )
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
```

### Step 5: Add Test Helper (Optional)

```rust
impl MySource {
    /// Create a test subscription (for unit tests)
    pub fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
        self.base.test_subscribe()
    }

    /// Inject a test event (for unit tests)
    pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
        self.base.dispatch_source_change(change).await
    }
}
```

## Configuration Patterns

### YAML Configuration

```yaml
sources:
  - id: my-source-1
    source_type: my-source
    auto_start: true
    dispatch_mode: channel
    dispatch_buffer_capacity: 2000
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/data/initial.jsonl"
    properties:
      host: localhost
      port: 8080
      interval_ms: 500
```

### Programmatic Configuration

```rust
use drasi_lib::DrasiLib;
use my_source_plugin::{MySource, MySourceConfig};
use std::sync::Arc;

let config = MySourceConfig {
    host: "localhost".to_string(),
    port: 8080,
    interval_ms: 500,
    ..Default::default()
};

let source = Arc::new(MySource::new("my-source-1", config)?);

let mut core = DrasiLib::builder()
    .with_source(source)
    .build()
    .await?;
```

## Testing Sources

### Unit Test Pattern

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_source_lifecycle() {
        let config = MySourceConfig::default();
        let source = MySource::new("test-source", config).unwrap();

        // Initial status
        assert_eq!(source.status().await, ComponentStatus::Stopped);

        // Start
        source.start().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Running);

        // Stop
        source.stop().await.unwrap();
        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_event_dispatch() {
        let config = MySourceConfig {
            interval_ms: 100,
            ..Default::default()
        };
        let source = MySource::new("test-source", config).unwrap();

        // Create test subscription
        let mut receiver = source.test_subscribe();

        source.start().await.unwrap();

        // Wait for at least one event
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            receiver.recv(),
        )
        .await
        .expect("Timeout waiting for event")
        .expect("Failed to receive event");

        assert_eq!(event.source_id, "test-source");

        source.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_with_bootstrap() {
        let config = MySourceConfig::default();
        let source = MySource::new("test-source", config).unwrap();

        source.start().await.unwrap();

        let response = source
            .subscribe(
                "test-query".to_string(),
                false,  // no bootstrap
                vec!["Item".to_string()],
                vec![],
            )
            .await
            .unwrap();

        assert_eq!(response.source_id, "test-source");
        assert_eq!(response.query_id, "test-query");
        assert!(response.bootstrap_receiver.is_none());

        source.stop().await.unwrap();
    }
}
```

### Integration Testing

```rust
use drasi_lib::DrasiLib;
use my_source_plugin::{MySource, MySourceConfig};
use std::sync::Arc;

#[tokio::test]
async fn test_source_with_drasi() {
    let config = MySourceConfig::default();
    let source = Arc::new(MySource::new("test-source", config).unwrap());

    let mut core = DrasiLib::builder()
        .with_source(source.clone())
        .build()
        .await
        .unwrap();

    core.initialize().await.unwrap();
    core.start().await.unwrap();

    // Verify source is running
    let status = core.get_source_status("test-source").await.unwrap();
    assert_eq!(status, ComponentStatus::Running);

    core.stop().await.unwrap();
}
```

## Key Files Reference

| File | Description |
|------|-------------|
| `drasi-core/lib/src/plugin_core/source.rs` | `Source` trait definition |
| `drasi-core/lib/src/sources/base.rs` | `SourceBase` implementation |
| `drasi-core/lib/src/channels/events.rs` | Event types (`SourceChange`, `SourceEvent`, etc.) |
| `drasi-core/lib/src/channels/dispatcher.rs` | Dispatcher traits and implementations |
| `drasi-core/lib/src/bootstrap/mod.rs` | Bootstrap provider traits and types |
| `drasi-core/lib/src/profiling.rs` | Profiling metadata types |

## Implementation Checklist

When creating a new source plugin:

- [ ] Create project structure with `Cargo.toml`
- [ ] Define configuration struct in `config.rs`
- [ ] Implement `Source` trait methods:
  - [ ] `id()` - return source identifier
  - [ ] `type_name()` - return source type string
  - [ ] `properties()` - return config as HashMap
  - [ ] `start()` - initialize and spawn processing task
  - [ ] `stop()` - gracefully shutdown
  - [ ] `status()` - return current status
  - [ ] `subscribe()` - handle query subscriptions
  - [ ] `as_any()` - return self for downcasting
  - [ ] `inject_event_tx()` - delegate to SourceBase
- [ ] Use `SourceBase` for common functionality
- [ ] Handle status transitions properly
- [ ] Dispatch events with profiling metadata
- [ ] Optional: Implement bootstrap provider
- [ ] Add unit tests
- [ ] Add integration tests
- [ ] Document configuration options

## Additional Resources

- See individual plugin directories for implementation examples
- Check `drasi-core/lib/CLAUDE.md` for library architecture details
- Review `drasi-core/CLAUDE.md` for query engine information
