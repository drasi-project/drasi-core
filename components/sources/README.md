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
- [Packaging as a Dynamic Plugin](#packaging-as-a-dynamic-plugin)

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
| `drasi-source-hyperliquid` | Hyperliquid market data streaming | `hyperliquid/` |

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

    /// Whether this source supports positional replay via `resume_from`.
    /// Sources backed by a persistent log (e.g., Postgres WAL, Kafka) should
    /// override this to return `true`. Default is `false`.
    fn supports_replay(&self) -> bool {
        false
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
    /// - `resume_from`: Optional sequence position to replay from (replay-capable sources)
    /// - `request_position_handle`: Whether the query wants a feedback handle
    ///
    /// Replay-capable sources return `Err(SourceError::PositionUnavailable { .. })`
    /// if they cannot honor `resume_from`.
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse>;

    /// Downcast helper for testing - allows access to concrete types
    fn as_any(&self) -> &dyn std::any::Any;

    /// Initialize the source with runtime context.
    /// Called automatically by DrasiLib when the source is added.
    /// The context provides access to the graph update channel (`update_tx`) and state_store.
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

- **Context-based initialization**: Sources receive a `SourceRuntimeContext` via `initialize()` when added to DrasiLib. The context provides access to `update_tx` (for status updates to the graph), state store, and other DrasiLib services.
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
    /// If set, the subscribing query requests events replayed from this sequence position.
    /// Only meaningful when the source returns `supports_replay() == true`.
    pub resume_from: Option<u64>,
    /// If true, the query requests a shared `Arc<AtomicU64>` position handle in the
    /// `SubscriptionResponse` for reporting its durably-processed position back to the source.
    pub request_position_handle: bool,
}
```

This enables:
- **Label filtering at source**: Sources can pre-filter data to only send relevant labels
- **Bootstrap optimization**: Bootstrap providers can fetch only the data types needed
- **Efficient joins**: Multi-source queries receive label allocations for each source
- **Positional replay**: Replay-capable sources can resume from a requested sequence position
- **Position feedback**: Queries can share their durably-processed position back to the source

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
    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>);

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
    pub fn status_handle(&self) -> ComponentStatusHandle;
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
    /// Shared handle for the query to report its last durably-processed sequence position.
    /// Created by replay-capable sources when `request_position_handle` is true.
    /// Initialized to `u64::MAX` (meaning "no position confirmed yet").
    pub position_handle: Option<Arc<AtomicU64>>,
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
    /// Returns a `BootstrapResult` with the event count plus handover metadata.
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
    ) -> Result<BootstrapResult>;
}

pub struct BootstrapResult {
    /// Number of bootstrap events sent through the channel.
    pub event_count: usize,
    /// The snapshot's position in the source's sequence space (e.g., a
    /// Postgres WAL LSN). `None` for providers without a positional concept.
    pub last_sequence: Option<u64>,
    /// Whether the bootstrap's sequence namespace matches the streaming
    /// source's sequence namespace (typically `true` only for homogeneous
    /// source + bootstrapper pairs, e.g., Postgres / Postgres).
    pub sequences_aligned: bool,
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
// Set status and send lifecycle event
self.base.set_status(
    ComponentStatus::Running,
    Some("Connected to database".to_string()),
).await;
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
        self.base.set_status(ComponentStatus::Starting, Some("Initializing".to_string())).await;

        // Clone what we need for the spawned task
        let dispatchers = self.base.dispatchers.clone();
        let source_id = self.base.id.clone();
        let status_handle = self.base.status_handle();
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
                if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
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
        self.base.set_status(ComponentStatus::Running, Some("Started".to_string())).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping MySource '{}'", self.base.id);
        self.base.set_status(ComponentStatus::Stopping, Some("Stopping".to_string())).await;

        // Cancel the task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        self.base.set_status(ComponentStatus::Stopped, Some("Stopped".to_string())).await;

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
            resume_from: None,
            request_position_handle: false,
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
- [ ] Optional: Package as a dynamic plugin (see [Packaging as a Dynamic Plugin](#packaging-as-a-dynamic-plugin)):
  - [ ] Set `crate-type = ["lib", "cdylib"]` in `Cargo.toml`
  - [ ] Add `drasi-plugin-sdk` and `utoipa` dependencies
  - [ ] Create `descriptor.rs` with configuration DTOs and `SourcePluginDescriptor` impl
  - [ ] Add `export_plugin!` macro invocation in `lib.rs`
  - [ ] Build and verify the `.so`/`.dylib`/`.dll` loads in the server's `plugins/` directory
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
    self.base.set_status(
        ComponentStatus::Starting,
        Some("Connecting to database".to_string()),
    ).await;

    // ... initialization logic ...

    self.base.set_status(
        ComponentStatus::Running,
        Some("Connected successfully".to_string()),
    ).await;

    Ok(())
}
```

## Packaging as a Dynamic Plugin

Sources can be packaged as dynamic plugins (shared libraries) so they are loaded at runtime by the Drasi server. The mock source (`components/sources/mock/`) is the reference implementation for this pattern.

### Cargo.toml Setup

Add the `cdylib` crate type and the required dependencies:

```toml
[lib]
crate-type = ["lib", "cdylib"]

[dependencies]
drasi-plugin-sdk = { workspace = true }
utoipa = { workspace = true }
# ... your other dependencies
```

Keeping `"lib"` alongside `"cdylib"` lets the crate be used as a normal Rust library (e.g. in tests or when linking statically) in addition to producing the shared library.

See `components/sources/mock/Cargo.toml` for a complete example.

### Configuration DTOs

Create a `descriptor.rs` module that defines configuration Data Transfer Objects (DTOs). These DTOs are serialized/deserialized from JSON and also provide an OpenAPI schema for the server's API documentation.

```rust
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

/// Configuration DTO for my source.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[schema(as = source::my_kind::MySourceConfig)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySourceConfigDto {
    pub endpoint: ConfigValue<String>,
    #[serde(default = "default_interval")]
    pub poll_interval_ms: ConfigValue<u64>,
}

fn default_interval() -> ConfigValue<u64> {
    ConfigValue::Static(5000)
}

#[derive(OpenApi)]
#[openapi(components(schemas(MySourceConfigDto)))]
struct MySourceSchemas;
```

Key points:
- **`#[schema(as = source::my_kind::ConfigName)]`** sets the schema path used by the server to locate the schema.
- **`ConfigValue<T>`** wraps values that can be either a literal (`ConfigValue::Static(val)`) or resolved from an environment variable at runtime. Use `DtoMapper::resolve_typed()` to resolve them.
- The `OpenApi` derive struct generates the JSON schema that the server exposes.

### The SourcePluginDescriptor Trait

Implement `SourcePluginDescriptor` to tell the plugin SDK how to create your source:

```rust
use crate::MySourceBuilder;
use drasi_plugin_sdk::prelude::*;
use utoipa::OpenApi;

pub struct MySourceDescriptor;

#[async_trait]
impl SourcePluginDescriptor for MySourceDescriptor {
    fn kind(&self) -> &str {
        "my-kind"
    }

    fn config_version(&self) -> &str {
        "1.0.0"
    }

    fn config_schema_name(&self) -> &str {
        "source.my_kind.MySourceConfig"
    }

    fn config_schema_json(&self) -> String {
        let api = MySourceSchemas::openapi();
        serde_json::to_string(
            &api.components
                .as_ref()
                .expect("OpenAPI components missing")
                .schemas,
        )
        .expect("Failed to serialize config schema")
    }

    async fn create_source(
        &self,
        id: &str,
        config_json: &serde_json::Value,
        auto_start: bool,
    ) -> anyhow::Result<Box<dyn drasi_lib::sources::Source>> {
        let dto: MySourceConfigDto = serde_json::from_value(config_json.clone())?;
        let mapper = DtoMapper::new();

        let source = MySourceBuilder::new(id)
            .with_endpoint(&mapper.resolve_typed::<String>(&dto.endpoint)?)
            .with_poll_interval_ms(mapper.resolve_typed(&dto.poll_interval_ms)?)
            .with_auto_start(auto_start)
            .build()?;

        Ok(Box::new(source))
    }
}
```

The `config_schema_name()` return value must match the `#[schema(as = ...)]` path on your config DTO, using dot-separated segments (e.g. `source.my_kind.MySourceConfig`).

### The export_plugin! Macro

In your `lib.rs`, invoke the `export_plugin!` macro to generate the FFI entry points that the server uses to load the plugin:

```rust
pub mod descriptor;

drasi_plugin_sdk::export_plugin!(
    plugin_id = "my-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::MySourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
```

The mock source gates this behind a `dynamic-plugin` feature flag so the macro is only compiled when building the shared library. This is optional but can be useful if you want to avoid the cdylib overhead in tests.

### Building

Build the plugin with:

```bash
cargo build
```

This produces a shared library in `target/debug/`:
- Linux: `libdrasi_source_my_kind.so`
- macOS: `libdrasi_source_my_kind.dylib`
- Windows: `drasi_source_my_kind.dll`

Copy the shared library into the server's `plugins/` directory (next to the `drasi-server` binary) and the server will load it automatically on startup.

### Bundling Bootstrap Providers

If your source has an associated bootstrap provider, you can export it from the same plugin crate. Implement `BootstrapPluginDescriptor` in your `descriptor.rs` and add it to the `bootstrap_descriptors` list in the `export_plugin!` macro:

```rust
drasi_plugin_sdk::export_plugin!(
    plugin_id = "my-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::MySourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [descriptor::MyBootstrapDescriptor],
);
```

This keeps the source and its bootstrap provider together in a single shared library.

> **Signing:** Published plugins should be signed with cosign: `cargo xtask publish-plugins --sign`

## Additional Resources

- See individual plugin directories for implementation examples
- Check `lib/CLAUDE.md` for library architecture details
- Review the parent `drasi-core/CLAUDE.md` for query engine information

## AI Generated Sources (experimental)

We have experimental AI agents for developing new sources. These agents work as a planner/executor pair where the `source-planner` will generate a comprehensive plan for building a source that the user must then review and refine. Once the user is satisfied with the plan, switch to the `source-plan-executor` agent to implement it.

For local usage with the Copilot CLI:

- Start Copilot CLI
- Select the `source-planner` agent using `/agent`
- Select the `claude-sonnet-4.5` or `claude-opus-4.5` model using `/model` (CLI does not use the model defined in the frontmatter)
- Run a prompt like `Please write a plan for an XXXX source and save it to file in my workspace`
- Once the plan is complete, review it and tweak it in the generated file
- Switch to the `gpt-5.2-codex` model using `/model`
- Switch to the `source-plan-executor` agent using `/agent`
- Run the prompt: `please implement the plan`
