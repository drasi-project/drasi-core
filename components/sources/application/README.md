# Application Source

## Overview

The Application Source is a programmatic data injection plugin for Drasi that enables direct, in-process delivery of graph data changes from Rust applications. Unlike network-based sources (HTTP, gRPC) or database-connected sources (PostgreSQL), the Application Source provides a native Rust API through a clonable handle pattern, allowing any part of your application to send graph events directly into Drasi's continuous query processing pipeline.

### Key Capabilities

- **Handle-based API**: Clone and share handles across threads for concurrent event injection
- **Type-safe Event Construction**: Builder pattern for creating graph nodes and relationships with compile-time type safety
- **Pluggable Bootstrap Support**: Configure any bootstrap provider for initial data delivery
- **Zero Network Overhead**: In-process communication via bounded async channels
- **Flexible Property Types**: Support for strings, integers, floats, booleans, and null values
- **Batch Operations**: Send multiple changes efficiently in a single operation

### Use Cases

**Ideal for:**
- **Embedded Drasi**: Integrate Drasi directly within your Rust application without external infrastructure
- **Testing**: Write precise unit and integration tests with full control over event sequences and timing
- **Synthetic Data Generation**: Create simulation environments, demos, or development data pipelines
- **Hybrid Architectures**: Combine programmatic events with data from external sources
- **Low-latency Scenarios**: Eliminate network serialization overhead for performance-critical applications

**Not suitable for:**
- External system integration (use PostgreSQL, HTTP, or gRPC sources instead)
- Cross-language scenarios (use HTTP or gRPC sources for language-agnostic access)
- Persistent data sources (use PostgreSQL or other database sources)

## Configuration

### Builder Pattern (Recommended)

The Application Source is typically created programmatically using the constructor pattern:

```rust
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};
use std::collections::HashMap;

// Create minimal configuration
let config = ApplicationSourceConfig {
    properties: HashMap::new(),
};

// Create source and handle
let (source, handle) = ApplicationSource::new("my-source", config)?;

// Configure bootstrap provider (optional)
source.set_bootstrap_provider(Box::new(my_bootstrap_provider)).await;
```

### Configuration Struct

```rust
pub struct ApplicationSourceConfig {
    /// Application-specific properties (flexible key-value map)
    pub properties: HashMap<String, serde_json::Value>,
}
```

### Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `properties` | Custom application-specific properties passed through to `Source::properties()` | `HashMap<String, serde_json::Value>` | Any JSON-serializable key-value pairs | `{}` (empty map) |
| `auto_start` | Whether to start automatically when added to DrasiLib | `bool` | `true`, `false` | `true` |

**Note**: The Application Source has minimal configuration requirements since it operates entirely in-process. The `properties` field is primarily for metadata and custom application logic.

**Auto-Start Behavior**: When `auto_start=true` (default), the source starts immediately if added to a running DrasiLib instance. If added before `drasi.start()` is called, it starts when the DrasiLib starts. When `auto_start=false`, the source must be started manually via `drasi.start_source("source-id")`.

## Input Schema

The Application Source accepts graph data changes through the `ApplicationSourceHandle` API. Events follow Drasi's graph data model:

### Graph Elements

**Node Structure:**
```rust
Element::Node {
    metadata: ElementMetadata {
        reference: ElementReference {
            source_id: Arc<str>,      // Automatically set from handle
            element_id: Arc<str>,     // Provided by application
        },
        labels: Arc<[Arc<str>]>,      // Node labels (e.g., ["Person", "Employee"])
        effective_from: u64,          // Timestamp (nanoseconds since epoch, auto-generated)
    },
    properties: ElementPropertyMap,   // Key-value property map
}
```

**Relationship Structure:**
```rust
Element::Relation {
    metadata: ElementMetadata {
        reference: ElementReference {
            source_id: Arc<str>,
            element_id: Arc<str>,
        },
        labels: Arc<[Arc<str>]>,      // Relation types (e.g., ["KNOWS", "WORKS_WITH"])
        effective_from: u64,
    },
    in_node: ElementReference,        // Target/end node
    out_node: ElementReference,       // Source/start node
    properties: ElementPropertyMap,
}
```

### Property Types

Properties are built using `PropertyMapBuilder` and support the following types:

| Type | Rust Type | Builder Method | Example |
|------|-----------|----------------|---------|
| String | `Arc<str>` | `.with_string(key, value)` | `.with_string("name", "Alice")` |
| Integer | `i64` | `.with_integer(key, value)` | `.with_integer("age", 30)` |
| Float | `f64` | `.with_float(key, value)` | `.with_float("score", 95.5)` |
| Boolean | `bool` | `.with_bool(key, value)` | `.with_bool("active", true)` |
| Null | - | `.with_null(key)` | `.with_null("optional_field")` |

### Event Types

The Application Source processes three types of graph changes:

1. **Insert**: Add a new node or relationship
   ```rust
   SourceChange::Insert { element: Element }
   ```

2. **Update**: Modify an existing node or relationship (full replacement)
   ```rust
   SourceChange::Update { element: Element }
   ```

3. **Delete**: Remove a node or relationship
   ```rust
   SourceChange::Delete { metadata: ElementMetadata }
   ```

## Usage Examples

### Basic Setup

```rust
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder
};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create the source
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
    };
    let (source, handle) = ApplicationSource::new("app-source", config)?;

    // Start the source (required before sending events)
    source.start().await?;

    // Use the handle to send events...
    Ok(())
}
```

### Inserting Nodes

```rust
// Insert a person node
let person_props = PropertyMapBuilder::new()
    .with_string("name", "Alice Johnson")
    .with_integer("age", 30)
    .with_string("email", "alice@example.com")
    .with_bool("active", true)
    .build();

handle.send_node_insert(
    "person-1",                    // Element ID
    vec!["Person", "Employee"],    // Labels
    person_props                   // Properties
).await?;

// Insert a company node
let company_props = PropertyMapBuilder::new()
    .with_string("name", "Acme Corp")
    .with_string("industry", "Technology")
    .with_integer("founded", 2010)
    .build();

handle.send_node_insert("company-1", vec!["Company"], company_props).await?;
```

### Inserting Relationships

```rust
// Create an employment relationship
let employment_props = PropertyMapBuilder::new()
    .with_string("role", "Software Engineer")
    .with_string("start_date", "2020-01-15")
    .with_integer("salary", 120000)
    .build();

handle.send_relation_insert(
    "employment-1",      // Relation ID
    vec!["WORKS_FOR"],   // Relation type
    employment_props,    // Properties
    "person-1",          // Start node (source)
    "company-1"          // End node (target)
).await?;

// Create a social relationship
let knows_props = PropertyMapBuilder::new()
    .with_string("since", "2018")
    .with_integer("closeness", 8)
    .build();

handle.send_relation_insert(
    "knows-1",
    vec!["KNOWS"],
    knows_props,
    "person-1",
    "person-2"
).await?;
```

### Updating Nodes

```rust
// Update existing node (full replacement)
let updated_props = PropertyMapBuilder::new()
    .with_string("name", "Alice Johnson-Smith")  // Changed name
    .with_integer("age", 31)                     // Birthday!
    .with_string("email", "alice@example.com")
    .with_bool("active", true)
    .with_string("department", "Engineering")    // New property
    .build();

handle.send_node_update(
    "person-1",
    vec!["Person", "Employee", "Manager"],  // Can update labels
    updated_props
).await?;
```

### Deleting Elements

```rust
// Delete a node
handle.send_delete("person-1", vec!["Person", "Employee"]).await?;

// Delete a relationship
handle.send_delete("knows-1", vec!["KNOWS"]).await?;
```

### Batch Operations

```rust
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use std::sync::Arc;

let mut changes = Vec::new();

// Build multiple changes
for i in 1..=5 {
    let props = PropertyMapBuilder::new()
        .with_string("name", format!("Person {}", i))
        .with_integer("id", i)
        .build();

    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(handle.source_id()),
                element_id: Arc::from(format!("person-{}", i).as_str()),
            },
            labels: Arc::from(vec![Arc::from("Person")]),
            effective_from: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64,
        },
        properties: props,
    };

    changes.push(SourceChange::Insert { element });
}

// Send all changes in sequence
handle.send_batch(changes).await?;
```

### Multi-threaded Usage

```rust
use tokio::task;

// Clone handle for concurrent access
let handle1 = handle.clone();
let handle2 = handle.clone();

let task1 = task::spawn(async move {
    let props = PropertyMapBuilder::new()
        .with_string("name", "Thread 1 Node")
        .build();
    handle1.send_node_insert("node-1", vec!["Test"], props).await
});

let task2 = task::spawn(async move {
    let props = PropertyMapBuilder::new()
        .with_string("name", "Thread 2 Node")
        .build();
    handle2.send_node_insert("node-2", vec!["Test"], props).await
});

// Wait for both tasks
task1.await??;
task2.await??;
```

### Integration with Drasi Server

```rust
use drasi_lib::DrasiLib;
use std::sync::Arc;

// Create Drasi instance
let drasi = DrasiLib::new("my-app").await?;

// Create application source
let config = ApplicationSourceConfig { properties: HashMap::new() };
let (source, handle) = ApplicationSource::new("events", config)?;

// Add source to Drasi
drasi.add_source(Arc::new(source)).await?;

// Define a query that uses the source
let query = drasi.create_query("high-temp-sensors")
    .cypher("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s")
    .from_source("events")
    .build()
    .await?;

drasi.add_query(query).await?;

// Now send events via handle
let props = PropertyMapBuilder::new()
    .with_string("id", "sensor-1")
    .with_integer("temperature", 80)
    .build();

handle.send_node_insert("sensor-1", vec!["Sensor"], props).await?;
```

## Bootstrap Support

The Application Source supports pluggable bootstrap providers via the `BootstrapProvider` trait. Any bootstrap provider implementation can be used with this source.

### Configuring Bootstrap

```rust
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};
// Bootstrap providers are separate crates - add the one you need to your Cargo.toml
// use drasi_bootstrap_application::ApplicationBootstrapProvider;
// use drasi_bootstrap_scriptfile::ScriptFileBootstrapProvider;

// Create source
let config = ApplicationSourceConfig { properties: HashMap::new() };
let (source, handle) = ApplicationSource::new("my-source", config)?;

// Configure bootstrap provider (example with ApplicationBootstrapProvider)
// let bootstrap_provider = ApplicationBootstrapProvider::new();
// source.set_bootstrap_provider(Box::new(bootstrap_provider)).await;
```

### Common Bootstrap Provider Options

Bootstrap providers are independent crates that you can add as dependencies:

- `ApplicationBootstrapProvider` (`drasi-bootstrap-application`) - Replays stored insert events from shared state
- `ScriptFileBootstrapProvider` (`drasi-bootstrap-scriptfile`) - Loads initial data from JSONL files
- `NoopBootstrapProvider` (`drasi-bootstrap-noop`) - Skips bootstrap entirely
- Custom implementations of the `BootstrapProvider` trait from `drasi-lib`

### Bootstrap Behavior

When a query subscribes with `enable_bootstrap: true`:
1. The source delegates to the configured bootstrap provider
2. The provider sends initial data events to the query
3. After bootstrap completes, the query receives streaming events

If no bootstrap provider is configured, the query is informed that bootstrap is not available.

## Thread Safety and Concurrency

### Channel Architecture

The Application Source uses Tokio's bounded mpsc channel for event passing:

```rust
// Internal channel (default capacity: 1000 events)
let (tx, rx) = mpsc::channel(1000);
```

**Backpressure**: If 1000 events are queued without processing, subsequent `.send()` calls will await until capacity is available.

### Handle Cloning

`ApplicationSourceHandle` implements `Clone` and is safe to share across threads:

```rust
let handle2 = handle.clone();
let handle3 = handle.clone();

// All handles send to the same source
tokio::spawn(async move { handle2.send_node_insert(...).await });
tokio::spawn(async move { handle3.send_node_insert(...).await });
```

### Async Operation

All send methods are `async` and must be awaited:

```rust
// Correct
handle.send_node_insert("node-1", vec!["Person"], props).await?;

// Incorrect (won't compile)
handle.send_node_insert("node-1", vec!["Person"], props)?;
```

### Shutdown Behavior

When `ApplicationSource::stop()` is called:
1. Internal processing task is aborted
2. Channel receiver is dropped
3. Subsequent handle sends return error: `"Failed to send event: channel closed"`

**Best Practice**: Handle channel closure errors gracefully in long-running applications.

## Error Handling

### Common Errors

```rust
// Channel closed (source stopped)
let result = handle.send_node_insert("node-1", vec!["Test"], props).await;
match result {
    Ok(_) => println!("Event sent successfully"),
    Err(e) if e.to_string().contains("channel closed") => {
        eprintln!("Source is not running or has been stopped");
    }
    Err(e) => eprintln!("Unexpected error: {}", e),
}
```

### Timestamp Errors

The source automatically generates timestamps using `chrono::Utc::now()`. In rare cases where timestamp generation fails, it falls back to millisecond precision:

```rust
// Automatic fallback (handled internally)
let effective_from = crate::time::get_current_timestamp_nanos()
    .unwrap_or_else(|e| {
        log::warn!("Failed to get timestamp: {}, using fallback", e);
        (chrono::Utc::now().timestamp_millis() as u64) * 1_000_000
    });
```

## Testing Patterns

### Basic Test Setup

```rust
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::collections::HashMap;

#[tokio::test]
async fn test_basic_insert() {
    let config = ApplicationSourceConfig { properties: HashMap::new() };
    let (source, handle) = ApplicationSource::new("test-source", config)
        .expect("Failed to create source");

    // Start the source
    source.start().await.unwrap();

    // Send an event
    let props = PropertyMapBuilder::new()
        .with_string("name", "Test User")
        .build();

    let result = handle.send_node_insert("user-1", vec!["User"], props).await;
    assert!(result.is_ok());
}
```

### Testing Event Sequences

```rust
#[tokio::test]
async fn test_insert_update_delete_sequence() {
    let config = ApplicationSourceConfig { properties: HashMap::new() };
    let (source, handle) = ApplicationSource::new("test-source", config).unwrap();
    source.start().await.unwrap();

    // Insert
    let props1 = PropertyMapBuilder::new()
        .with_string("status", "pending")
        .build();
    handle.send_node_insert("task-1", vec!["Task"], props1).await.unwrap();

    // Update
    let props2 = PropertyMapBuilder::new()
        .with_string("status", "completed")
        .build();
    handle.send_node_update("task-1", vec!["Task"], props2).await.unwrap();

    // Delete
    handle.send_delete("task-1", vec!["Task"]).await.unwrap();
}
```

### Testing Error Conditions

```rust
#[tokio::test]
async fn test_send_after_stop() {
    let config = ApplicationSourceConfig { properties: HashMap::new() };
    let (source, handle) = ApplicationSource::new("test-source", config).unwrap();
    source.start().await.unwrap();

    // Stop the source
    source.stop().await.unwrap();

    // Attempt to send should fail
    let props = PropertyMapBuilder::new().build();
    let result = handle.send_node_insert("node-1", vec!["Test"], props).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("channel closed"));
}
```

## Performance Considerations

### Channel Capacity

Default capacity: 1000 events. For high-throughput scenarios:

```rust
// Currently requires source code modification
// Future enhancement: configurable channel size
let (app_tx, app_rx) = mpsc::channel(10000);  // Larger buffer
```

### Batch vs. Individual Sends

**Individual sends** (typical):
```rust
for event in events {
    handle.send_node_insert(...).await?;
}
```

**Batch sends** (better for large volumes):
```rust
let changes: Vec<SourceChange> = events.iter().map(|e| { /* ... */ }).collect();
handle.send_batch(changes).await?;
```

### Memory Usage

Memory usage depends on the configured bootstrap provider:
- `ApplicationBootstrapProvider` stores events in memory
- `ScriptFileBootstrapProvider` reads from disk on demand
- Consider bootstrap provider memory implications for high-volume scenarios

## Comparison with Other Sources

| Feature | Application | HTTP | gRPC | PostgreSQL |
|---------|-------------|------|------|------------|
| **Access** | Rust API | REST | Protocol Buffers | Database CDC |
| **Performance** | Highest (in-process) | Moderate | Moderate | Database-dependent |
| **Language Support** | Rust only | Any | Any | Any (via database) |
| **Network** | No | Yes | Yes | Yes |
| **Bootstrap** | Pluggable provider | Pluggable provider | Pluggable provider | Pluggable provider |
| **Use Case** | Embedded, testing | Cross-language | Microservices | DB integration |
| **Setup Complexity** | Minimal | Moderate | Moderate | High |

## Known Limitations

1. **No Deduplication**: Source does not prevent duplicate element IDs (query engine handles this)
2. **Fixed Channel Size**: Default 1000-event capacity requires code changes to increase
3. **Rust-Only**: No cross-language support (by design)
4. **No Manual Timestamps**: Timestamps are auto-generated, cannot be manually set
5. **No Reconnection**: Stopped sources cannot be reused, handles become permanently unusable
6. **Bootstrap Requires Provider**: No built-in bootstrap; requires configuring a bootstrap provider

## Advanced Topics

### Profiling Metadata

The source automatically adds profiling metadata to events for performance analysis:

```rust
let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());
```

This metadata flows through the query pipeline and enables end-to-end latency tracking.

### Component Lifecycle Events

The source emits lifecycle events via the component event channel:

- **Starting**: Source initialization began
- **Running**: Event processor started successfully
- **Stopping**: Shutdown initiated
- **Stopped**: Shutdown completed

Monitor these events for observability in production systems.

### Retrieving Additional Handles

After source creation, get additional handles:

```rust
let (source, handle1) = ApplicationSource::new("my-source", config)?;
let handle2 = source.get_handle();
let handle3 = source.get_handle();
```

All handles (including the original) connect to the same source instance.

## API Reference Summary

### ApplicationSource

| Method | Description |
|--------|-------------|
| `new(id, config)` | Create source and handle |
| `get_handle()` | Get an additional handle |
| `set_bootstrap_provider(provider)` | Configure bootstrap provider |
| `start()` | Start event processing |
| `stop()` | Stop event processing |

### ApplicationSourceHandle

| Method | Description |
|--------|-------------|
| `send(change)` | Send raw `SourceChange` event |
| `send_node_insert(id, labels, props)` | Insert a node |
| `send_node_update(id, labels, props)` | Update a node |
| `send_delete(id, labels)` | Delete a node or relation |
| `send_relation_insert(id, labels, props, start, end)` | Insert a relationship |
| `send_batch(changes)` | Send multiple changes |
| `source_id()` | Get source identifier |

### PropertyMapBuilder

| Method | Description |
|--------|-------------|
| `new()` | Create new builder |
| `with_string(key, value)` | Add string property |
| `with_integer(key, value)` | Add integer property (i64) |
| `with_float(key, value)` | Add float property (f64) |
| `with_bool(key, value)` | Add boolean property |
| `with_null(key)` | Add null property |
| `build()` | Build final property map |

## Further Reading

- **Source Implementation**: `/Users/allenjones/dev/agentofreality/drasi/drasi-server/drasi-core/components/sources/application/src/lib.rs`
- **Test Examples**: `/Users/allenjones/dev/agentofreality/drasi/drasi-server/drasi-core/components/sources/application/src/tests.rs`
- **Drasi Core Models**: See `drasi-core/models/` for graph data type definitions
- **Bootstrap Providers**: See `drasi-lib/bootstrap/` for bootstrap provider architecture
