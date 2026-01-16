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
| `drasi-source-application` | Programmatic/in-memory sources for embedded use | `application/` |
| `drasi-source-grpc` | gRPC streaming data sources | `grpc/` |
| `drasi-source-http` | HTTP endpoint polling with adaptive batching | `http/` |
| `drasi-source-mock` | Test data generator for development | `mock/` |
| `drasi-source-platform` | Redis Streams consumer for platform integration | `platform/` |
| `drasi-source-postgres` | PostgreSQL WAL-based replication | `postgres/` |

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

All sources must implement the `Source` trait from `drasi_lib`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::*;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::Source;
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

    /// Whether this source should auto-start when DrasiLib starts.
    /// Default is `true`. Override to return `false` if this source
    /// should only be started manually via `start_source()`.
    fn auto_start(&self) -> bool {
        true
    }

    /// Start the source - begins data ingestion and event generation
    async fn start(&self) -> Result<()>;

    /// Stop the source - stops data ingestion and cleans up resources
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the source
    async fn status(&self) -> ComponentStatus;

    /// Subscribe to this source for change events.
    /// Called by queries to receive data changes from this source.
    ///
    /// The `settings` parameter contains:
    /// - `source_id`: The source being subscribed to
    /// - `query_id`: ID of the subscribing query
    /// - `enable_bootstrap`: Whether to request initial data
    /// - `nodes`: Set of node labels the query is interested in
    /// - `relations`: Set of relation labels the query is interested in
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse>;

    /// Downcast helper for testing - allows access to concrete types
    fn as_any(&self) -> &dyn std::any::Any;

    /// Initialize the source with runtime context.
    /// Called automatically by DrasiLib when the source is added.
    /// The context provides access to DrasiLib services like status_tx and state_store.
    async fn initialize(&self, context: SourceRuntimeContext);

    /// Set the bootstrap provider for this source.
    /// This method allows setting a bootstrap provider after source construction.
    /// Sources without a bootstrap provider will report that bootstrap is not available.
    async fn set_bootstrap_provider(
        &self,
        _provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        // Default implementation does nothing
    }
}
```

### Key Design Points

- **Context-based initialization**: Sources receive a `SourceRuntimeContext` via `initialize()` when added to DrasiLib. The context provides access to the status channel, state store, and other DrasiLib services.
- **Async lifecycle**: All lifecycle methods (`start`, `stop`, `subscribe`) are async.
- **Generic properties**: The `properties()` method returns a HashMap for API inspection, while the actual typed configuration is owned by the plugin.
- **Query context in subscribe**: The `subscribe()` method receives `SourceSubscriptionSettings` which includes the specific node and relation labels the query needs, enabling sources and bootstrap providers to filter data at the source level.
- **Auto-start control**: Sources can override `auto_start()` to control whether they start automatically when DrasiLib starts.

## SourceSubscriptionSettings

When queries subscribe to sources, they pass `SourceSubscriptionSettings` which contains query context information:

```rust
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub struct SourceSubscriptionSettings {
    /// ID of the source being subscribed to
    pub source_id: String,
    /// Whether to request initial data (bootstrap)
    pub enable_bootstrap: bool,
    /// ID of the subscribing query
    pub query_id: String,
    /// Set of node labels the query is interested in from this source
    pub nodes: HashSet<String>,
    /// Set of relation labels the query is interested in from this source
    pub relations: HashSet<String>,
}
```

This enables:
- **Label filtering at source**: Sources can pre-filter data to only send relevant labels
- **Bootstrap optimization**: Bootstrap providers can fetch only the data types needed
- **Efficient joins**: Multi-source queries receive label allocations for each source

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
    pub dispatch_mode: Option<DispatchMode>,                              // Default: Channel
    pub dispatch_buffer_capacity: Option<usize>,                          // Default: 1000
    pub bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>, // Default: None
    pub auto_start: bool,                                                 // Default: true
}

impl SourceBaseParams {
    pub fn new(id: impl Into<String>) -> Self;
    pub fn with_dispatch_mode(self, mode: DispatchMode) -> Self;
    pub fn with_dispatch_buffer_capacity(self, capacity: usize) -> Self;
    pub fn with_bootstrap_provider(self, provider: impl BootstrapProvider + 'static) -> Self;
    pub fn with_auto_start(self, auto_start: bool) -> Self;
}
```

### Key SourceBase Methods

```rust
impl SourceBase {
    // Status management
    pub async fn get_status(&self) -> ComponentStatus;
    pub async fn set_status(&self, status: ComponentStatus);
    pub async fn set_status_with_event(&self, status: ComponentStatus, message: Option<String>) -> Result<()>;

    // Component lifecycle events
    pub async fn send_component_event(&self, status: ComponentStatus, message: Option<String>) -> Result<()>;

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

    // Subscription handling - uses SourceSubscriptionSettings for query context
    pub async fn subscribe_with_bootstrap(
        &self,
        settings: &SourceSubscriptionSettings,
        source_type: &str,
    ) -> Result<SubscriptionResponse>;

    pub async fn create_streaming_receiver(&self) -> Result<Box<dyn ChangeReceiver<SourceEventWrapper>>>;

    // Bootstrap provider - takes ownership via impl trait
    pub async fn set_bootstrap_provider(&self, provider: impl BootstrapProvider + 'static);

    // Task management
    pub async fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>);
    pub async fn set_shutdown_tx(&self, tx: tokio::sync::oneshot::Sender<()>);
    pub async fn stop_common(&self) -> Result<()>;

    // Context initialization (called automatically by DrasiLib)
    pub async fn initialize(&self, context: SourceRuntimeContext);
    pub fn status_tx(&self) -> Arc<RwLock<Option<ComponentEventSender>>>;
    pub async fn state_store(&self) -> Option<Arc<dyn StateStoreProvider>>;

    // Auto-start configuration
    pub fn get_auto_start(&self) -> bool;

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
    /// Perform bootstrap operation for the given request.
    /// Sends bootstrap events to the provided channel.
    /// Returns the number of elements sent.
    ///
    /// # Arguments
    /// * `request` - Bootstrap request with query ID and labels
    /// * `context` - Bootstrap context with source information
    /// * `event_tx` - Channel to send bootstrap events
    /// * `settings` - Optional subscription settings with additional query context
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        settings: Option<&SourceSubscriptionSettings>,
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
    properties: Arc<HashMap<String, serde_json::Value>>,
}

impl BootstrapContext {
    /// Create a minimal bootstrap context with just server and source IDs
    pub fn new_minimal(server_id: String, source_id: String) -> Self;

    /// Create a bootstrap context with properties
    pub fn with_properties(
        server_id: String,
        source_id: String,
        properties: HashMap<String, serde_json::Value>,
    ) -> Self;

    /// Get the next sequence number for bootstrap events
    pub fn next_sequence(&self) -> u64;
}
```

### Available Bootstrap Provider Types

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

Any source can use any bootstrap provider. For example, you can create an HTTP source that bootstraps from PostgreSQL:

```rust
// Create the source
let http_source = Arc::new(HttpSource::new("my-http-source", http_config)?);

// Create a PostgreSQL bootstrap provider
let bootstrap_provider = Arc::new(PostgresBootstrapProvider::new(postgres_config)?);

// Set the bootstrap provider on the source
http_source.set_bootstrap_provider(bootstrap_provider).await;

// Add to DrasiLib
let core = DrasiLib::builder()
    .with_source(http_source)
    .build()
    .await?;
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
my-source/
├── Cargo.toml
└── src/
    ├── lib.rs      # Source implementation
    └── config.rs   # Configuration struct
```

### Step 2: Cargo.toml

```toml
[package]
name = "drasi-source-my-source"
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
use drasi_lib::Source;
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

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        // Delegate to SourceBase for standard handling
        self.base
            .subscribe_with_bootstrap(&settings, "MySource")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
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

### Programmatic Configuration (Required)

Sources are always configured programmatically and passed to DrasiLib. The `with_source()` method takes ownership and wraps internally in Arc:

```rust
use drasi_lib::DrasiLib;
use my_source_plugin::{MySource, MySourceConfig};

let config = MySourceConfig {
    host: "localhost".to_string(),
    port: 8080,
    interval_ms: 500,
    ..Default::default()
};

let source = MySource::new("my-source-1", config)?;

let core = DrasiLib::builder()
    .with_source(source)  // Ownership transferred, wrapped in Arc internally
    .build()
    .await?;
```

### With Bootstrap Provider

Bootstrap providers can be set in two ways:

**Method 1: Using the Builder Pattern (Recommended)**

```rust
use drasi_lib::DrasiLib;
use my_source_plugin::MySource;
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

let source = MySource::builder("my-source-1")
    .with_host("localhost")
    .with_port(8080)
    .with_interval_ms(500)
    .with_bootstrap_provider(ScriptFileBootstrapProvider::new("/data/initial.jsonl"))
    .build()?;

let core = DrasiLib::builder()
    .with_source(source)
    .build()
    .await?;
```

**Method 2: Using the Source Trait Method**

```rust
use drasi_lib::DrasiLib;
use drasi_lib::Source;  // Required for set_bootstrap_provider
use my_source_plugin::{MySource, MySourceConfig};
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

let config = MySourceConfig {
    host: "localhost".to_string(),
    port: 8080,
    interval_ms: 500,
    ..Default::default()
};

let source = MySource::new("my-source-1", config)?;

// Set bootstrap provider via the Source trait method
source.set_bootstrap_provider(Box::new(
    ScriptFileBootstrapProvider::new("/data/initial.jsonl")
)).await;

let core = DrasiLib::builder()
    .with_source(source)
    .build()
    .await?;
```

### With Custom Dispatch Settings

```rust
let source = MySource::with_dispatch(
    "my-source-1",
    config,
    Some(DispatchMode::Broadcast),  // Use broadcast for high fanout
    Some(10000),                     // Large buffer
)?;

// with_source takes ownership
let core = DrasiLib::builder()
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
    async fn test_subscribe_with_settings() {
        use drasi_lib::config::SourceSubscriptionSettings;
        use std::collections::HashSet;

        let config = MySourceConfig::default();
        let source = MySource::new("test-source", config).unwrap();

        source.start().await.unwrap();

        // Create subscription settings with query context
        let settings = SourceSubscriptionSettings {
            source_id: "test-source".to_string(),
            query_id: "test-query".to_string(),
            enable_bootstrap: false,
            nodes: ["Item"].iter().map(|s| s.to_string()).collect(),
            relations: HashSet::new(),
        };

        let response = source.subscribe(settings).await.unwrap();

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

#[tokio::test]
async fn test_source_with_drasi() {
    let config = MySourceConfig::default();
    let source = MySource::new("test-source", config).unwrap();

    let core = DrasiLib::builder()
        .with_source(source)  // Ownership transferred
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
| `lib/src/sources/traits.rs` | `Source` trait definition |
| `lib/src/sources/base.rs` | `SourceBase` implementation |
| `lib/src/config/schema.rs` | `SourceSubscriptionSettings` and configuration types |
| `lib/src/channels/events.rs` | Event types (`SourceChange`, `SourceEvent`, etc.) |
| `lib/src/channels/dispatcher.rs` | Dispatcher traits and implementations |
| `lib/src/bootstrap/mod.rs` | Bootstrap provider traits and types |
| `lib/src/profiling/mod.rs` | Profiling metadata types |

## Implementation Checklist

When creating a new source plugin:

- [ ] Create project structure with `Cargo.toml`
- [ ] Define configuration struct in `config.rs`
- [ ] Implement `Source` trait methods:
  - [ ] `id()` - return source identifier
  - [ ] `type_name()` - return source type string
  - [ ] `properties()` - return config as HashMap
  - [ ] `auto_start()` - delegate to SourceBase (optional, defaults to true)
  - [ ] `start()` - initialize and spawn processing task
  - [ ] `stop()` - gracefully shutdown
  - [ ] `status()` - return current status
  - [ ] `subscribe(settings)` - handle query subscriptions with SourceSubscriptionSettings
  - [ ] `as_any()` - return self for downcasting
  - [ ] `initialize(context)` - delegate to SourceBase
  - [ ] `set_bootstrap_provider()` - delegate to SourceBase (optional)
- [ ] Use `SourceBase` for common functionality
- [ ] Handle status transitions properly
- [ ] Dispatch events with profiling metadata
- [ ] Optional: Implement bootstrap provider
- [ ] Optional: Implement builder pattern for ergonomic construction
- [ ] Add unit tests
- [ ] Add integration tests
- [ ] Document configuration options

## Best Practices

### Builder Pattern

Consider implementing a builder pattern for ergonomic source construction. This is the approach used by all built-in source plugins:

```rust
pub struct MySourceBuilder {
    id: String,
    // Configuration fields with defaults
    host: String,
    port: u16,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl MySourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            host: "localhost".to_string(),
            port: 8080,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_bootstrap_provider(
        mut self,
        provider: impl BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn build(self) -> Result<MySource> {
        let config = MySourceConfig {
            host: self.host,
            port: self.port,
            ..Default::default()
        };

        let mut params = SourceBaseParams::new(&self.id)
            .with_auto_start(self.auto_start);

        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        Ok(MySource {
            base: SourceBase::new(params)?,
            config,
        })
    }
}

impl MySource {
    pub fn builder(id: impl Into<String>) -> MySourceBuilder {
        MySourceBuilder::new(id)
    }
}
```

This pattern enables clean, fluent construction:

```rust
let source = MySource::builder("my-source")
    .with_host("db.example.com")
    .with_port(5432)
    .with_dispatch_mode(DispatchMode::Broadcast)
    .with_bootstrap_provider(ScriptFileBootstrapProvider::new("/data/init.jsonl"))
    .with_auto_start(false)
    .build()?;
```

### Error Handling in Spawned Tasks

When dispatching events from spawned tasks, use `SourceBase::dispatch_from_task()`:

```rust
// Clone what we need before spawning
let dispatchers = self.base.dispatchers.clone();
let source_id = self.base.id.clone();

tokio::spawn(async move {
    // Create and dispatch events
    let wrapper = SourceEventWrapper::new(/* ... */);

    if let Err(e) = SourceBase::dispatch_from_task(
        dispatchers.clone(),
        wrapper,
        &source_id,
    ).await {
        debug!("Failed to dispatch: {e}");
    }
});
```

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
    self.base.set_status(ComponentStatus::Starting).await;
    self.base.send_component_event(
        ComponentStatus::Starting,
        Some("Connecting to database".to_string()),
    ).await?;

    // ... initialization logic ...

    self.base.set_status(ComponentStatus::Running).await;
    self.base.send_component_event(
        ComponentStatus::Running,
        Some("Connected successfully".to_string()),
    ).await?;

    Ok(())
}
```

## Additional Resources

- See individual plugin directories for implementation examples
- Check `lib/CLAUDE.md` for library architecture details
- Review the parent `drasi-core/CLAUDE.md` for query engine information
