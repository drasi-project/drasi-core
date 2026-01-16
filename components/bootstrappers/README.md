# Bootstrap Provider Developer Guide

This guide explains how to create custom bootstrap provider plugins for Drasi. Bootstrap providers handle initial data delivery when queries first subscribe to sources, providing the current state before streaming changes begin.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [The BootstrapProvider Trait](#the-bootstrapprovider-trait)
- [Core Types](#core-types)
- [Label Filtering](#label-filtering)
- [Creating a Bootstrap Provider Plugin](#creating-a-bootstrap-provider-plugin)
- [Configuration Patterns](#configuration-patterns)
- [Testing Bootstrap Providers](#testing-bootstrap-providers)
- [Key Files Reference](#key-files-reference)
- [Implementation Checklist](#implementation-checklist)
- [Best Practices](#best-practices)
- [Additional Resources](#additional-resources)

## Overview

Bootstrap providers are responsible for:

1. **Initial Data Delivery**: Providing current state data when queries first subscribe to sources
2. **Label Filtering**: Sending only nodes and relations that match the query's requirements
3. **Sequence Numbering**: Assigning monotonically increasing sequence numbers to bootstrap events
4. **Async Streaming**: Sending data through channels without blocking source subscriptions

### Key Architectural Principle

**Bootstrap providers are completely independent from sources.** Any source can use any bootstrap provider, enabling powerful mix-and-match scenarios:

- Bootstrap from PostgreSQL, stream changes from HTTP
- Bootstrap from script files, stream changes from gRPC
- Bootstrap from a remote Drasi instance, stream changes from Redis

### Available Bootstrap Provider Plugins

| Plugin | Description | Directory |
|--------|-------------|-----------|
| `drasi-bootstrap-noop` | Returns no data (default) | `noop/` |
| `drasi-bootstrap-scriptfile` | Reads JSONL script files | `scriptfile/` |
| `drasi-bootstrap-postgres` | PostgreSQL snapshot using COPY | `postgres/` |
| `drasi-bootstrap-application` | Replays in-memory insert events | `application/` |
| `drasi-bootstrap-platform` | HTTP streaming from remote Drasi | `platform/` |

## Architecture

### Data Flow

```
┌─────────────────────────────────────────────────────────────────────────┐
│                      Source Subscription                                 │
│                                                                          │
│  ┌─────────────┐    ┌──────────────────────┐    ┌───────────────────┐   │
│  │ Query       │───▶│ Source.subscribe()   │───▶│ Streaming Events  │──▶│
│  │             │    │                      │    │ (ongoing changes) │   │
│  │             │    │                      │    └───────────────────┘   │
│  │             │    │                      │                            │
│  │             │    │  ┌────────────────┐  │    ┌───────────────────┐   │
│  │             │◀───│──│ Bootstrap      │──│───▶│ Bootstrap Events  │──▶│
│  │             │    │  │ Provider       │  │    │ (initial state)   │   │
│  └─────────────┘    │  └────────────────┘  │    └───────────────────┘   │
│                     └──────────────────────┘                            │
└─────────────────────────────────────────────────────────────────────────┘
```

### Bootstrap Flow

1. Query calls `source.subscribe(settings)` with `enable_bootstrap: true`
2. Source creates a dedicated bootstrap channel (capacity: 1000)
3. Source spawns an async task that calls `bootstrap_provider.bootstrap(...)`
4. Bootstrap provider sends `BootstrapEvent` items through the channel
5. Query processes bootstrap events to build initial state
6. Query then processes streaming events for ongoing changes

### Plugin Architecture

Each bootstrap provider plugin:
1. Defines its own typed configuration struct
2. Implements the `BootstrapProvider` trait
3. Is passed to sources via their builder or `set_bootstrap_provider()` method

DrasiLib has no knowledge of which bootstrap providers exist - it only knows about the `BootstrapProvider` trait.

## The BootstrapProvider Trait

All bootstrap providers must implement the `BootstrapProvider` trait from `drasi_lib::bootstrap`:

```rust
use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEventSender;
use drasi_lib::config::SourceSubscriptionSettings;

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
    ) -> Result<usize>;
}
```

### Key Design Points

- **Async execution**: Bootstrap runs in a spawned task, not blocking subscription
- **Channel-based delivery**: Events sent via MPSC channel, not returned directly
- **Label filtering**: Both `request` and `settings` contain labels for filtering
- **Sequence numbering**: Use `context.next_sequence()` for each event
- **Return count**: Return the number of elements sent for logging/metrics

## Core Types

### BootstrapRequest

Contains the query's data requirements:

```rust
#[derive(Debug, Clone)]
pub struct BootstrapRequest {
    /// ID of the query requesting bootstrap
    pub query_id: String,
    /// Node labels the query is interested in (from this source)
    pub node_labels: Vec<String>,
    /// Relation labels the query is interested in (from this source)
    pub relation_labels: Vec<String>,
    /// Unique request identifier (query_id + UUID)
    pub request_id: String,
}
```

### BootstrapContext

Provides execution context for the bootstrap operation:

```rust
#[derive(Clone)]
pub struct BootstrapContext {
    /// Unique server ID for logging and tracing
    pub server_id: String,
    /// Source ID for labeling bootstrap events
    pub source_id: String,
    /// Atomic sequence counter for bootstrap events
    pub sequence_counter: Arc<AtomicU64>,
    /// Optional properties from source configuration
    properties: Arc<HashMap<String, serde_json::Value>>,
}

impl BootstrapContext {
    /// Create a minimal context (preferred)
    pub fn new_minimal(server_id: String, source_id: String) -> Self;

    /// Create context with properties (if needed)
    pub fn with_properties(
        server_id: String,
        source_id: String,
        properties: HashMap<String, serde_json::Value>,
    ) -> Self;

    /// Get next sequence number for bootstrap events
    pub fn next_sequence(&self) -> u64;

    /// Get an optional property
    pub fn get_property(&self, key: &str) -> Option<serde_json::Value>;

    /// Get a typed property
    pub fn get_typed_property<T>(&self, key: &str) -> Result<Option<T>>
    where
        T: for<'de> Deserialize<'de>;
}
```

### BootstrapEvent

The event type sent through the bootstrap channel:

```rust
pub struct BootstrapEvent {
    /// Source ID this event belongs to
    pub source_id: String,
    /// The actual data change (always Insert for bootstrap)
    pub change: SourceChange,
    /// Timestamp when the event was created
    pub timestamp: chrono::DateTime<chrono::Utc>,
    /// Sequence number from BootstrapContext
    pub sequence: u64,
}

pub type BootstrapEventSender = mpsc::Sender<BootstrapEvent>;
pub type BootstrapEventReceiver = mpsc::Receiver<BootstrapEvent>;
```

### SourceSubscriptionSettings

Query context passed to bootstrap (recent addition - commit e824b2d):

```rust
#[derive(Debug, Clone)]
pub struct SourceSubscriptionSettings {
    /// ID of the source being subscribed to
    pub source_id: String,
    /// Whether bootstrap is enabled
    pub enable_bootstrap: bool,
    /// ID of the subscribing query
    pub query_id: String,
    /// Set of node labels the query needs from this source
    pub nodes: HashSet<String>,
    /// Set of relation labels the query needs from this source
    pub relations: HashSet<String>,
}
```

## Label Filtering

Bootstrap providers should filter data to only send elements that match the requested labels. This optimization prevents unnecessary data transfer and processing.

### Filtering Logic

```rust
fn matches_labels(
    element_labels: &[String],
    requested_labels: &[String],
) -> bool {
    // If no labels requested, include all elements
    if requested_labels.is_empty() {
        return true;
    }

    // Include if any element label matches any requested label
    element_labels.iter().any(|label| requested_labels.contains(label))
}
```

### Using SourceSubscriptionSettings

The `settings` parameter provides the same label information as `request`, but as `HashSet` instead of `Vec`. Use either:

```rust
// From request (Vec<String>)
let node_labels = &request.node_labels;

// From settings (HashSet<String>) - may have additional context
if let Some(settings) = settings {
    let nodes = &settings.nodes;  // HashSet<String>
}
```

## Creating a Bootstrap Provider Plugin

### Step 1: Project Structure

```
my-bootstrap/
├── Cargo.toml
└── src/
    └── lib.rs      # Provider implementation
```

### Step 2: Cargo.toml

```toml
[package]
name = "drasi-bootstrap-my-bootstrap"
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

### Step 3: Provider Implementation

```rust
// src/lib.rs
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender};
use drasi_lib::config::SourceSubscriptionSettings;
use log::info;
use std::sync::Arc;

/// Configuration for MyBootstrapProvider
#[derive(Debug, Clone)]
pub struct MyBootstrapConfig {
    pub data_source: String,
    pub timeout_ms: u64,
}

impl Default for MyBootstrapConfig {
    fn default() -> Self {
        Self {
            data_source: "default".to_string(),
            timeout_ms: 30000,
        }
    }
}

/// My custom bootstrap provider
pub struct MyBootstrapProvider {
    config: MyBootstrapConfig,
}

impl MyBootstrapProvider {
    /// Create a new provider from configuration
    pub fn new(config: MyBootstrapConfig) -> Self {
        Self { config }
    }

    /// Create a builder for MyBootstrapProvider
    pub fn builder() -> MyBootstrapProviderBuilder {
        MyBootstrapProviderBuilder::new()
    }

    /// Check if an element matches the requested labels
    fn matches_labels(element_labels: &[String], requested_labels: &[String]) -> bool {
        if requested_labels.is_empty() {
            return true;
        }
        element_labels.iter().any(|l| requested_labels.contains(l))
    }

    /// Create a BootstrapEvent from a SourceChange
    fn create_event(
        source_id: &str,
        change: SourceChange,
        sequence: u64,
    ) -> BootstrapEvent {
        BootstrapEvent {
            source_id: source_id.to_string(),
            change,
            timestamp: chrono::Utc::now(),
            sequence,
        }
    }
}

impl Default for MyBootstrapProvider {
    fn default() -> Self {
        Self::new(MyBootstrapConfig::default())
    }
}

#[async_trait]
impl BootstrapProvider for MyBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<usize> {
        info!(
            "Starting bootstrap for query {} from source {}",
            request.query_id, context.source_id
        );

        let mut count = 0;

        // Fetch your data (example: hardcoded for demonstration)
        let nodes = vec![
            ("node1", vec!["Person"], vec![("name", "Alice")]),
            ("node2", vec!["Person"], vec![("name", "Bob")]),
        ];

        for (id, labels, props) in nodes {
            // Filter by requested labels
            let label_strings: Vec<String> = labels.iter().map(|s| s.to_string()).collect();
            if !Self::matches_labels(&label_strings, &request.node_labels) {
                continue;
            }

            // Create Element
            let mut properties = drasi_core::models::ElementPropertyMap::new();
            for (key, value) in props {
                properties.insert(
                    key,
                    drasi_core::models::ElementValue::String(Arc::from(value)),
                );
            }

            let element = Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(&context.source_id, id),
                    labels: labels.iter().map(|l| Arc::from(*l)).collect::<Vec<_>>().into(),
                    effective_from: 0,
                },
                properties,
            };

            // Create and send bootstrap event
            let change = SourceChange::Insert { element };
            let sequence = context.next_sequence();
            let event = Self::create_event(&context.source_id, change, sequence);

            event_tx
                .send(event)
                .await
                .map_err(|e| anyhow!("Failed to send bootstrap event: {e}"))?;

            count += 1;
        }

        info!(
            "Completed bootstrap for query {}: sent {} elements",
            request.query_id, count
        );

        Ok(count)
    }
}

/// Builder for MyBootstrapProvider
pub struct MyBootstrapProviderBuilder {
    config: MyBootstrapConfig,
}

impl MyBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self {
            config: MyBootstrapConfig::default(),
        }
    }

    pub fn with_data_source(mut self, data_source: impl Into<String>) -> Self {
        self.config.data_source = data_source.into();
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }

    pub fn build(self) -> MyBootstrapProvider {
        MyBootstrapProvider::new(self.config)
    }
}

impl Default for MyBootstrapProviderBuilder {
    fn default() -> Self {
        Self::new()
    }
}
```

## Configuration Patterns

### Using with Source Builder (Recommended)

```rust
use drasi_source_mock::MockSource;
use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

let source = MockSource::builder("my-source")
    .with_data_type("sensor")
    .with_bootstrap_provider(
        ScriptFileBootstrapProvider::builder()
            .with_file("/data/initial_state.jsonl")
            .build()
    )
    .build()?;
```

### Using Source Trait Method

```rust
use drasi_lib::Source;
use drasi_source_http::HttpSource;
use drasi_bootstrap_postgres::PostgresBootstrapProvider;

let source = HttpSource::builder("my-http-source")
    .with_host("localhost")
    .with_port(9000)
    .build()?;

// Set bootstrap provider after construction
source.set_bootstrap_provider(Box::new(
    PostgresBootstrapProvider::builder()
        .with_host("localhost")
        .with_database("mydb")
        .build()?
)).await;
```

### Mix-and-Match Scenarios

```rust
// HTTP source with PostgreSQL bootstrap
let http_with_postgres = HttpSource::builder("http-source")
    .with_bootstrap_provider(
        PostgresBootstrapProvider::builder()
            .with_host("db.example.com")
            .build()?
    )
    .build()?;

// gRPC source with script file bootstrap
let grpc_with_scriptfile = GrpcSource::builder("grpc-source")
    .with_bootstrap_provider(
        ScriptFileBootstrapProvider::builder()
            .with_file("/test/data.jsonl")
            .build()
    )
    .build()?;

// Platform source with remote Drasi bootstrap
let platform_with_remote = PlatformSource::builder("platform-source")
    .with_bootstrap_provider(
        PlatformBootstrapProvider::builder()
            .with_query_api_url("http://remote-drasi:8080")
            .with_timeout_seconds(600)
            .build()?
    )
    .build()?;
```

## Testing Bootstrap Providers

### Unit Test Pattern

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_bootstrap_sends_events() {
        let provider = MyBootstrapProvider::builder()
            .with_data_source("test")
            .build();

        // Create channel to receive events
        let (tx, mut rx) = mpsc::channel(100);

        // Create context
        let context = BootstrapContext::new_minimal(
            "test-server".to_string(),
            "test-source".to_string(),
        );

        // Create request
        let request = BootstrapRequest {
            query_id: "test-query".to_string(),
            node_labels: vec!["Person".to_string()],
            relation_labels: vec![],
            request_id: "test-request-1".to_string(),
        };

        // Run bootstrap
        let count = provider
            .bootstrap(request, &context, tx, None)
            .await
            .unwrap();

        // Verify events were sent
        assert!(count > 0);

        // Check received events
        let mut received = 0;
        while let Ok(event) = rx.try_recv() {
            assert_eq!(event.source_id, "test-source");
            assert!(matches!(event.change, SourceChange::Insert { .. }));
            received += 1;
        }

        assert_eq!(received, count);
    }

    #[tokio::test]
    async fn test_label_filtering() {
        let provider = MyBootstrapProvider::default();
        let (tx, mut rx) = mpsc::channel(100);

        let context = BootstrapContext::new_minimal(
            "server".to_string(),
            "source".to_string(),
        );

        // Request only "Company" labels (not in our test data)
        let request = BootstrapRequest {
            query_id: "query".to_string(),
            node_labels: vec!["Company".to_string()],
            relation_labels: vec![],
            request_id: "req".to_string(),
        };

        let count = provider
            .bootstrap(request, &context, tx, None)
            .await
            .unwrap();

        // Should send 0 events (no "Company" nodes in test data)
        assert_eq!(count, 0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_sequence_numbers_are_monotonic() {
        let provider = MyBootstrapProvider::default();
        let (tx, mut rx) = mpsc::channel(100);

        let context = BootstrapContext::new_minimal(
            "server".to_string(),
            "source".to_string(),
        );

        let request = BootstrapRequest {
            query_id: "query".to_string(),
            node_labels: vec![],  // Accept all
            relation_labels: vec![],
            request_id: "req".to_string(),
        };

        provider
            .bootstrap(request, &context, tx, None)
            .await
            .unwrap();

        // Verify sequence numbers are monotonically increasing
        let mut last_seq = None;
        while let Ok(event) = rx.try_recv() {
            if let Some(prev) = last_seq {
                assert!(event.sequence > prev, "Sequence numbers must increase");
            }
            last_seq = Some(event.sequence);
        }
    }
}
```

### Integration Testing with Source

```rust
use drasi_lib::DrasiLib;
use drasi_source_mock::MockSource;
use my_bootstrap::MyBootstrapProvider;

#[tokio::test]
async fn test_bootstrap_with_source() {
    let source = MockSource::builder("test-source")
        .with_bootstrap_provider(MyBootstrapProvider::default())
        .build()
        .unwrap();

    let core = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .unwrap();

    core.initialize().await.unwrap();
    core.start().await.unwrap();

    // Verify source is running with bootstrap provider
    let status = core.get_source_status("test-source").await.unwrap();
    assert_eq!(status, ComponentStatus::Running);

    core.stop().await.unwrap();
}
```

## Key Files Reference

| File | Description |
|------|-------------|
| `lib/src/bootstrap/mod.rs` | `BootstrapProvider` trait, config types, `BootstrapContext` |
| `lib/src/config/schema.rs` | `SourceSubscriptionSettings` definition |
| `lib/src/channels/events.rs` | `BootstrapEvent`, channel types |
| `lib/src/sources/base.rs` | `SourceBase` bootstrap subscription handling |
| `lib/src/queries/label_extractor.rs` | Query label extraction |
| `lib/src/queries/subscription_builder.rs` | Label allocation to sources |

## Implementation Checklist

When creating a new bootstrap provider plugin:

- [ ] Create project structure with `Cargo.toml`
- [ ] Define configuration struct (if needed)
- [ ] Implement `BootstrapProvider` trait:
  - [ ] `bootstrap()` - main bootstrap logic
- [ ] Implement label filtering for nodes and relations
- [ ] Use `context.next_sequence()` for each event
- [ ] Send events via `event_tx.send(event).await`
- [ ] Return count of events sent
- [ ] Implement builder pattern for ergonomic construction
- [ ] Implement `Default` trait if sensible defaults exist
- [ ] Add unit tests for:
  - [ ] Event sending
  - [ ] Label filtering
  - [ ] Sequence number monotonicity
  - [ ] Empty request handling
- [ ] Add integration tests with sources
- [ ] Document configuration options

## Best Practices

### Builder Pattern

Implement a builder pattern for ergonomic construction:

```rust
pub struct MyBootstrapProviderBuilder {
    config: MyBootstrapConfig,
}

impl MyBootstrapProviderBuilder {
    pub fn new() -> Self {
        Self { config: MyBootstrapConfig::default() }
    }

    pub fn with_option(mut self, value: String) -> Self {
        self.config.option = value;
        self
    }

    pub fn build(self) -> MyBootstrapProvider {
        MyBootstrapProvider::new(self.config)
    }
}

impl MyBootstrapProvider {
    pub fn builder() -> MyBootstrapProviderBuilder {
        MyBootstrapProviderBuilder::new()
    }
}
```

### Efficient Label Filtering

Filter early to avoid unnecessary data processing:

```rust
async fn bootstrap(&self, request: BootstrapRequest, ...) -> Result<usize> {
    let mut count = 0;

    for item in self.fetch_data().await? {
        // Filter BEFORE creating Element (saves allocation)
        if !Self::matches_labels(&item.labels, &request.node_labels) {
            continue;
        }

        // Now create Element and send event
        let element = self.convert_to_element(&item)?;
        // ...
        count += 1;
    }

    Ok(count)
}
```

### Error Handling

Handle channel send errors gracefully:

```rust
event_tx
    .send(event)
    .await
    .map_err(|e| anyhow!("Failed to send bootstrap event: {e}"))?;
```

### Logging

Provide informative logging for debugging:

```rust
info!(
    "Starting bootstrap for query {} from source {} (nodes: {:?}, relations: {:?})",
    request.query_id,
    context.source_id,
    request.node_labels,
    request.relation_labels
);

// ... bootstrap logic ...

info!(
    "Completed bootstrap for query {}: sent {} elements",
    request.query_id, count
);
```

### Using Settings for Additional Context

The `settings` parameter provides query context that may be useful:

```rust
async fn bootstrap(
    &self,
    request: BootstrapRequest,
    context: &BootstrapContext,
    event_tx: BootstrapEventSender,
    settings: Option<&SourceSubscriptionSettings>,
) -> Result<usize> {
    // Log additional context if available
    if let Some(s) = settings {
        info!(
            "Bootstrap for query {} (enable_bootstrap: {}, nodes: {:?}, relations: {:?})",
            s.query_id, s.enable_bootstrap, s.nodes, s.relations
        );
    }

    // Use settings.nodes/relations as HashSet for O(1) lookup
    let node_set: HashSet<&str> = settings
        .map(|s| s.nodes.iter().map(|n| n.as_str()).collect())
        .unwrap_or_default();

    // ... rest of bootstrap logic
}
```

### Converting JSON to Element Properties

Use the helper function from drasi-lib:

```rust
use drasi_lib::sources::manager::convert_json_to_element_properties;

let properties = if let serde_json::Value::Object(obj) = &json_props {
    convert_json_to_element_properties(obj)?
} else {
    Default::default()
};
```

### Creating Elements

Follow the standard Element creation pattern:

```rust
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use std::sync::Arc;

// For Node elements
let node = Element::Node {
    metadata: ElementMetadata {
        reference: ElementReference::new(&context.source_id, &node_id),
        labels: labels.iter().map(|l| Arc::from(l.as_str())).collect::<Vec<_>>().into(),
        effective_from: 0,  // Bootstrap uses 0
    },
    properties,
};

// For Relation elements
let relation = Element::Relation {
    metadata: ElementMetadata {
        reference: ElementReference::new(&context.source_id, &relation_id),
        labels: labels.iter().map(|l| Arc::from(l.as_str())).collect::<Vec<_>>().into(),
        effective_from: 0,
    },
    properties,
    in_node: ElementReference::new(&context.source_id, &end_node_id),
    out_node: ElementReference::new(&context.source_id, &start_node_id),
};

// Always use Insert for bootstrap
let change = SourceChange::Insert { element: node };
```

## JSONL Script File Format

For `ScriptFileBootstrapProvider`, files use JSONL (JSON Lines) format with these record types:

```jsonl
{"type":"Header","start_time":"2024-01-01T00:00:00Z","description":"Initial data"}
{"type":"Node","id":"n1","labels":["Person"],"properties":{"name":"Alice","age":30}}
{"type":"Node","id":"n2","labels":["Person"],"properties":{"name":"Bob","age":25}}
{"type":"Relation","id":"r1","labels":["KNOWS"],"start_id":"n1","end_id":"n2","properties":{"since":2020}}
{"type":"Comment","content":"This is a comment (will be skipped)"}
{"type":"Label","label":"checkpoint-1"}
{"type":"Finish","description":"End of initial data"}
```

| Record Type | Required | Description |
|-------------|----------|-------------|
| `Header` | Yes (first) | Script metadata with start_time and description |
| `Node` | No | Node data with id, labels, and properties |
| `Relation` | No | Relation with id, labels, start_id, end_id, properties |
| `Comment` | No | Comments (filtered out during processing) |
| `Label` | No | Checkpoint markers |
| `Finish` | No | End marker (stops processing) |

## Additional Resources

- See individual plugin directories for implementation examples
- Check `lib/CLAUDE.md` for library architecture details
- Review `components/sources/README.md` for source development patterns
- Review `components/reactions/README.md` for reaction development patterns
- Review the parent `drasi-core/CLAUDE.md` for query engine information
