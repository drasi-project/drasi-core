# Application Source

## Overview

The Application Source is a programmatic API for injecting graph data changes directly into Drasi from Rust applications. Unlike HTTP or gRPC sources that receive data over network protocols, the Application Source provides a native Rust handle-based API for embedded use cases, testing, and scenarios where your application needs to directly control data flow into the Drasi query engine.

This source is ideal for:
- Embedding Drasi within your Rust application
- Writing tests that require precise control over data changes
- Scenarios where network-based sources add unnecessary overhead
- Programmatic data generation or transformation pipelines

## When to Use This Source

**Use Application Source when:**
- You're embedding Drasi within a Rust application and want direct API access
- Writing unit or integration tests that need to send specific sequences of changes
- Building data generators or synthetic data pipelines
- You need low-latency, in-process data injection without network overhead

**Use other sources when:**
- Data comes from external systems (use Platform, PostgreSQL, HTTP, or gRPC sources)
- You need language-agnostic integration (use HTTP or gRPC sources)
- Data originates from change data capture (use PostgreSQL or Platform sources)

## Configuration

The Application Source requires minimal configuration. It's typically created programmatically rather than via YAML configuration:

```yaml
sources:
  - id: my_app_source
    source_type: application
    auto_start: true
    properties: {}  # No properties required
```

In most cases, you'll create the source directly in code using `ApplicationSource::new()`.

## Architecture

The Application Source uses a channel-based architecture:

1. **ApplicationSource**: The source component that implements the `Source` trait and integrates with the Drasi server core
2. **ApplicationSourceHandle**: A cloneable handle that applications use to send events
3. **Internal Channel**: A bounded mpsc channel (default capacity: 1000) for event passing

The handle can be cloned and shared across threads, making it easy to send events from multiple parts of your application.

## API Reference

### ApplicationSourceHandle

The main interface for sending events to the source.

#### Methods

##### `send`
```rust
pub async fn send(&self, change: SourceChange) -> Result<()>
```

Send a raw `SourceChange` event. This is the low-level method; most users will prefer the convenience methods below.

**Parameters:**
- `change`: A `SourceChange` enum variant (Insert, Update, or Delete)

**Returns:**
- `Result<()>`: Ok on success, error if the channel is closed

**Example:**
```rust
let element = Element::Node {
    metadata: ElementMetadata { /* ... */ },
    properties: ElementPropertyMap::new(),
};
handle.send(SourceChange::Insert { element }).await?;
```

---

##### `send_node_insert`
```rust
pub async fn send_node_insert(
    &self,
    element_id: impl Into<Arc<str>>,
    labels: Vec<impl Into<Arc<str>>>,
    properties: ElementPropertyMap,
) -> Result<()>
```

Send an insert event for a new node.

**Parameters:**
- `element_id`: Unique identifier for the node (within this source)
- `labels`: Vector of labels to apply to the node (e.g., `["Person", "Employee"]`)
- `properties`: Property map created with `PropertyMapBuilder` or `ElementPropertyMap::new()`

**Returns:**
- `Result<()>`: Ok on success, error if the channel is closed

**Behavior:**
- Automatically sets the source_id from the handle
- Sets effective_from timestamp to current UTC time (nanoseconds)
- Stores the insert event for bootstrap purposes

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_string("name", "Alice")
    .with_integer("age", 30)
    .build();

handle.send_node_insert("person-1", vec!["Person"], props).await?;
```

---

##### `send_node_update`
```rust
pub async fn send_node_update(
    &self,
    element_id: impl Into<Arc<str>>,
    labels: Vec<impl Into<Arc<str>>>,
    properties: ElementPropertyMap,
) -> Result<()>
```

Send an update event for an existing node.

**Parameters:**
- `element_id`: Identifier of the node to update
- `labels`: Updated labels for the node
- `properties`: Updated property map (replaces all properties)

**Returns:**
- `Result<()>`: Ok on success, error if the channel is closed

**Behavior:**
- Updates are treated as full replacements, not patches
- Sets effective_from timestamp to current UTC time
- Does NOT store update events for bootstrap (only inserts are stored)

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_string("name", "Alice Smith")
    .with_integer("age", 31)
    .build();

handle.send_node_update("person-1", vec!["Person", "Manager"], props).await?;
```

---

##### `send_delete`
```rust
pub async fn send_delete(
    &self,
    element_id: impl Into<Arc<str>>,
    labels: Vec<impl Into<Arc<str>>>,
) -> Result<()>
```

Send a delete event for a node or relation.

**Parameters:**
- `element_id`: Identifier of the element to delete
- `labels`: Labels of the element being deleted

**Returns:**
- `Result<()>`: Ok on success, error if the channel is closed

**Behavior:**
- Only requires metadata (no properties needed for deletion)
- Sets effective_from timestamp to current UTC time
- Does NOT remove the element from bootstrap storage (bootstrap always reflects initial state)

**Example:**
```rust
handle.send_delete("person-1", vec!["Person"]).await?;
```

---

##### `send_relation_insert`
```rust
pub async fn send_relation_insert(
    &self,
    element_id: impl Into<Arc<str>>,
    labels: Vec<impl Into<Arc<str>>>,
    properties: ElementPropertyMap,
    start_node_id: impl Into<Arc<str>>,
    end_node_id: impl Into<Arc<str>>,
) -> Result<()>
```

Send an insert event for a new relation (edge) between two nodes.

**Parameters:**
- `element_id`: Unique identifier for the relation
- `labels`: Vector of labels for the relation (typically one label like `["KNOWS"]`)
- `properties`: Property map for the relation
- `start_node_id`: ID of the source/outgoing node
- `end_node_id`: ID of the target/incoming node

**Returns:**
- `Result<()>`: Ok on success, error if the channel is closed

**Behavior:**
- Creates a directed edge from `start_node_id` to `end_node_id`
- Both referenced nodes must have been inserted previously (or exist in bootstrap data)
- Stores the insert event for bootstrap purposes

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_string("since", "2020")
    .with_integer("strength", 5)
    .build();

handle.send_relation_insert(
    "rel-1",
    vec!["KNOWS"],
    props,
    "person-1",
    "person-2"
).await?;
```

---

##### `send_batch`
```rust
pub async fn send_batch(&self, changes: Vec<SourceChange>) -> Result<()>
```

Send multiple source changes in sequence.

**Parameters:**
- `changes`: Vector of `SourceChange` events to send

**Returns:**
- `Result<()>`: Ok if all changes sent successfully, error on first failure

**Behavior:**
- Sends changes sequentially in order
- Stops and returns error on first failure
- Each change is sent through the internal channel individually

**Example:**
```rust
let changes = vec![
    SourceChange::Insert { element: node1 },
    SourceChange::Insert { element: node2 },
    SourceChange::Insert { element: relation },
];

handle.send_batch(changes).await?;
```

---

##### `source_id`
```rust
pub fn source_id(&self) -> &str
```

Get the source ID associated with this handle.

**Returns:**
- `&str`: The source identifier

**Example:**
```rust
let id = handle.source_id();
println!("Source ID: {}", id);
```

---

### PropertyMapBuilder

A builder utility for constructing `ElementPropertyMap` instances with a fluent API.

#### Methods

##### `new`
```rust
pub fn new() -> Self
```

Create a new empty property map builder.

**Example:**
```rust
let builder = PropertyMapBuilder::new();
```

---

##### `with_string`
```rust
pub fn with_string(self, key: impl AsRef<str>, value: impl Into<String>) -> Self
```

Add a string property.

**Parameters:**
- `key`: Property name
- `value`: String value

**Returns:**
- `Self`: Builder for method chaining

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_string("name", "Alice")
    .with_string("email", "alice@example.com")
    .build();
```

---

##### `with_integer`
```rust
pub fn with_integer(self, key: impl AsRef<str>, value: i64) -> Self
```

Add an integer property.

**Parameters:**
- `key`: Property name
- `value`: i64 integer value

**Returns:**
- `Self`: Builder for method chaining

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_integer("age", 30)
    .with_integer("employee_id", 12345)
    .build();
```

---

##### `with_float`
```rust
pub fn with_float(self, key: impl AsRef<str>, value: f64) -> Self
```

Add a floating-point property.

**Parameters:**
- `key`: Property name
- `value`: f64 floating-point value

**Returns:**
- `Self`: Builder for method chaining

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_float("price", 19.99)
    .with_float("rating", 4.5)
    .build();
```

---

##### `with_bool`
```rust
pub fn with_bool(self, key: impl AsRef<str>, value: bool) -> Self
```

Add a boolean property.

**Parameters:**
- `key`: Property name
- `value`: Boolean value

**Returns:**
- `Self`: Builder for method chaining

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_bool("active", true)
    .with_bool("verified", false)
    .build();
```

---

##### `with_null`
```rust
pub fn with_null(self, key: impl AsRef<str>) -> Self
```

Add a null property.

**Parameters:**
- `key`: Property name

**Returns:**
- `Self`: Builder for method chaining

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_string("name", "Alice")
    .with_null("middle_name")
    .build();
```

---

##### `build`
```rust
pub fn build(self) -> ElementPropertyMap
```

Build and return the final `ElementPropertyMap`.

**Returns:**
- `ElementPropertyMap`: The constructed property map

**Example:**
```rust
let props = PropertyMapBuilder::new()
    .with_string("name", "Alice")
    .with_integer("age", 30)
    .build();
```

---

## Usage Examples

### Creating an Application Source

```rust
use drasi_server_core::sources::application::ApplicationSource;
use drasi_server_core::config::SourceConfig;
use tokio::sync::mpsc;

// Create channels for integration with server core
let (source_event_tx, source_event_rx) = mpsc::channel(100);
let (event_tx, event_rx) = mpsc::channel(100);

// Create source configuration
let config = SourceConfig {
    id: "my_source".to_string(),
    source_type: "application".to_string(),
    auto_start: true,
    properties: HashMap::new(),
    bootstrap_provider: None,
};

// Create the source and handle
let (source, handle) = ApplicationSource::new(config, source_event_tx, event_tx);

// Start the source
source.start().await?;

// Now use the handle to send events
```

### Inserting Nodes

```rust
use drasi_server_core::sources::application::PropertyMapBuilder;

// Insert a person node
let person_props = PropertyMapBuilder::new()
    .with_string("name", "Alice Johnson")
    .with_integer("age", 30)
    .with_string("email", "alice@example.com")
    .with_bool("active", true)
    .build();

handle.send_node_insert("person-1", vec!["Person", "Employee"], person_props).await?;

// Insert a company node
let company_props = PropertyMapBuilder::new()
    .with_string("name", "Acme Corp")
    .with_string("industry", "Technology")
    .with_integer("founded", 2010)
    .build();

handle.send_node_insert("company-1", vec!["Company"], company_props).await?;
```

### Updating Nodes

```rust
// Update Alice's age and add a new property
let updated_props = PropertyMapBuilder::new()
    .with_string("name", "Alice Johnson")
    .with_integer("age", 31)  // Birthday!
    .with_string("email", "alice@example.com")
    .with_bool("active", true)
    .with_string("department", "Engineering")  // New property
    .build();

handle.send_node_update("person-1", vec!["Person", "Employee"], updated_props).await?;
```

### Deleting Elements

```rust
// Delete a node
handle.send_delete("person-1", vec!["Person", "Employee"]).await?;

// Delete a relation
handle.send_delete("rel-1", vec!["KNOWS"]).await?;
```

### Inserting Relations

```rust
// Create an employment relation
let employment_props = PropertyMapBuilder::new()
    .with_string("role", "Software Engineer")
    .with_string("start_date", "2020-01-15")
    .with_integer("salary", 120000)
    .build();

handle.send_relation_insert(
    "employment-1",
    vec!["WORKS_FOR"],
    employment_props,
    "person-1",      // Alice works for...
    "company-1"      // ...Acme Corp
).await?;

// Create a social relation
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

### Batch Operations

```rust
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use std::sync::Arc;

// Build a batch of changes
let mut changes = Vec::new();

// Add multiple person nodes
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

// Send all at once
handle.send_batch(changes).await?;
```

### Using Multiple Handles (Thread Safety)

```rust
// Clone the handle for use in another task
let handle2 = handle.clone();

tokio::spawn(async move {
    let props = PropertyMapBuilder::new()
        .with_string("name", "Background Task Node")
        .build();

    handle2.send_node_insert("bg-node-1", vec!["Background"], props).await.unwrap();
});
```

### Working with Different Property Types

```rust
let props = PropertyMapBuilder::new()
    .with_string("text", "Hello, World!")
    .with_integer("count", 42)
    .with_float("percentage", 75.5)
    .with_bool("enabled", true)
    .with_null("optional_field")
    .build();

handle.send_node_insert("demo-node", vec!["Demo"], props).await?;
```

## Data Types

### SourceChange

The core event type representing a change to graph data:

```rust
pub enum SourceChange {
    Insert { element: Element },
    Update { element: Element },
    Delete { metadata: ElementMetadata },
    Future { future_ref: FutureElementRef },  // Used internally for temporal queries
}
```

### Element

Represents a graph element (node or relation):

```rust
pub enum Element {
    Node {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
    },
    Relation {
        metadata: ElementMetadata,
        in_node: ElementReference,      // Target node
        out_node: ElementReference,     // Source node
        properties: ElementPropertyMap,
    },
}
```

### ElementMetadata

Metadata associated with every element:

```rust
pub struct ElementMetadata {
    pub reference: ElementReference,
    pub labels: Arc<[Arc<str>]>,
    pub effective_from: ElementTimestamp,  // u64 nanoseconds since epoch
}
```

### ElementReference

Uniquely identifies an element:

```rust
pub struct ElementReference {
    pub source_id: Arc<str>,
    pub element_id: Arc<str>,
}
```

### ElementPropertyMap

A map of property names to values:

```rust
pub struct ElementPropertyMap {
    values: BTreeMap<Arc<str>, ElementValue>,
}

impl ElementPropertyMap {
    pub fn new() -> Self;
    pub fn get(&self, key: &str) -> Option<&ElementValue>;
    pub fn insert(&mut self, key: &str, value: ElementValue);
}
```

### ElementValue

The value types supported for element properties:

```rust
pub enum ElementValue {
    Null,
    Bool(bool),
    Float(OrderedFloat<f64>),
    Integer(i64),
    String(Arc<str>),
    List(Vec<ElementValue>),          // Nested lists supported
    Object(ElementPropertyMap),        // Nested objects supported
}
```

## Bootstrap Behavior

The Application Source implements automatic bootstrap support:

1. **Insert Event Storage**: All `Insert` events (both nodes and relations) are automatically stored in an internal `bootstrap_data` collection
2. **Bootstrap Replay**: When queries request bootstrap data, the stored insert events are replayed
3. **Update/Delete Events**: These are NOT stored for bootstrap, as bootstrap represents the initial state of the graph
4. **Bootstrap Provider**: The Application Source uses the `ApplicationBootstrapProvider` which replays the stored insert events

This means:
- Bootstrap always reflects the "create" operations, not the current state after updates/deletes
- If you insert a node, then update it, bootstrap will replay only the original insert
- If you insert a node, then delete it, bootstrap will still include the original insert

**Note**: This bootstrap behavior is designed for testing and embedded scenarios. For production use cases with persistent state, consider using a different bootstrap strategy or source type.

## Thread Safety and Async

### Channel-Based Communication

The Application Source uses Tokio's bounded mpsc channel for event passing:

```rust
// Default channel capacity: 1000 events
let (tx, rx) = mpsc::channel(1000);
```

**Implications:**
- If you send more than 1000 events without the source processing them, `.send()` will await until space is available
- This provides natural backpressure
- For high-throughput scenarios, monitor channel capacity or increase buffer size

### Handle Cloning

`ApplicationSourceHandle` is `Clone` and can be safely shared across threads:

```rust
let handle2 = handle.clone();
let handle3 = handle.clone();

// All three handles send to the same source
tokio::spawn(async move { handle2.send_node_insert(/* ... */).await });
tokio::spawn(async move { handle3.send_node_insert(/* ... */).await });
```

### Async Methods

All send methods are `async` and must be awaited:

```rust
// Correct
handle.send_node_insert("node-1", vec!["Person"], props).await?;

// Won't compile - must await
handle.send_node_insert("node-1", vec!["Person"], props)?;
```

### Shutdown Behavior

When the `ApplicationSource` is stopped:
1. The internal processing task is aborted
2. The channel receiver is dropped
3. Subsequent `.send()` calls on handles will return an error: `"Failed to send event: channel closed"`

Always handle this error gracefully in long-running applications.

## Testing Patterns

### Basic Test Setup

```rust
use drasi_server_core::test_support::helpers::create_test_application_source;

#[tokio::test]
async fn test_my_scenario() {
    // Create a test source and handle
    let (_source, handle) = create_test_application_source("test-source").await;

    // Send events
    let props = PropertyMapBuilder::new()
        .with_string("name", "Test User")
        .build();

    handle.send_node_insert("user-1", vec!["User"], props).await.unwrap();

    // Assertions...
}
```

### Testing Event Sequences

```rust
#[tokio::test]
async fn test_update_after_insert() {
    let (_source, handle) = create_test_application_source("test-source").await;

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

    // Verify the sequence was processed correctly
}
```

### Testing Error Handling

```rust
#[tokio::test]
async fn test_send_after_source_stopped() {
    let (source, handle) = create_test_application_source("test-source").await;

    // Stop the source
    source.stop().await.unwrap();

    // Attempting to send should fail
    let props = PropertyMapBuilder::new().build();
    let result = handle.send_node_insert("node-1", vec!["Test"], props).await;

    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("channel closed"));
}
```

### Testing with Multiple Labels

```rust
#[tokio::test]
async fn test_multi_label_nodes() {
    let (_source, handle) = create_test_application_source("test-source").await;

    let props = PropertyMapBuilder::new()
        .with_string("name", "Admin User")
        .build();

    handle.send_node_insert(
        "user-1",
        vec!["Person", "User", "Admin"],
        props
    ).await.unwrap();
}
```

## Comparison with Other Sources

### Application Source vs HTTP Source

| Feature | Application Source | HTTP Source |
|---------|-------------------|-------------|
| **Access Method** | Direct Rust API | HTTP REST endpoints |
| **Performance** | Lowest latency (in-process) | Network overhead |
| **Language** | Rust only | Language-agnostic |
| **Use Case** | Embedded, testing | Cross-language integration |
| **Setup Complexity** | Minimal (code-based) | Moderate (requires HTTP server) |
| **Type Safety** | Compile-time | Runtime (JSON parsing) |

### Application Source vs gRPC Source

| Feature | Application Source | gRPC Source |
|---------|-------------------|-------------|
| **Access Method** | Direct Rust API | gRPC protocol |
| **Performance** | Lowest latency | Moderate (network + serialization) |
| **Language** | Rust only | Multi-language (protocol buffers) |
| **Use Case** | Embedded, testing | Microservices architecture |
| **Streaming** | Not applicable | Bidirectional streaming support |

### Application Source vs Platform Source

| Feature | Application Source | Platform Source |
|---------|-------------------|----------------|
| **Data Origin** | Programmatic (in-process) | Redis Streams (external Drasi) |
| **Bootstrap** | Stored insert events | Remote Query API |
| **Use Case** | Embedded apps, testing | Multi-environment integration |
| **Complexity** | Minimal | Higher (requires Redis, Query API) |

### Application Source vs PostgreSQL Source

| Feature | Application Source | PostgreSQL Source |
|---------|-------------------|-------------------|
| **Data Origin** | Programmatic | PostgreSQL WAL/CDC |
| **Real-time Updates** | Manual (you send them) | Automatic (database changes) |
| **Use Case** | Testing, embedded | Production database integration |
| **Bootstrap** | Stored events | Database snapshot |

## Known Limitations

1. **Bootstrap is Insert-Only**: Bootstrap data only includes `Insert` events, not the cumulative effect of updates and deletes. This means bootstrap doesn't represent the "current state" but rather the "creation events".

2. **No Persistence**: The Application Source stores events in memory. If the application restarts, bootstrap data is lost unless you rebuild it.

3. **No Built-in Deduplication**: The source does not prevent you from inserting the same element ID twice. The query engine will handle this, but it may not behave as expected.

4. **Bounded Channel**: The default channel capacity is 1000 events. High-throughput scenarios may experience backpressure. Consider increasing the channel size if needed (requires code modification).

5. **No External Access**: Unlike HTTP/gRPC sources, there's no way for external applications to send data. This source is exclusively for in-process use.

6. **Manual Timestamp Management**: The source automatically sets timestamps using `chrono::Utc::now()`. You cannot manually control timestamps for temporal queries (this is by design for consistency).

7. **Error Handling**: Once the source is stopped, all handles become unusable. There's no automatic reconnection mechanism.

## Advanced Topics

### Custom Source Creation (Not Using Test Helper)

If you need more control than the test helper provides:

```rust
use drasi_server_core::sources::application::ApplicationSource;
use drasi_server_core::config::SourceConfig;
use tokio::sync::mpsc;
use std::collections::HashMap;

let config = SourceConfig {
    id: "custom_source".to_string(),
    source_type: "application".to_string(),
    auto_start: true,
    properties: HashMap::new(),
    bootstrap_provider: None,
};

// Create your own channels with custom capacity
let (source_event_tx, source_event_rx) = mpsc::channel(5000);  // Larger buffer
let (event_tx, event_rx) = mpsc::channel(100);

let (source, handle) = ApplicationSource::new(config, source_event_tx, event_tx);

// You now own source_event_rx and event_rx for custom integration
```

### Profiling Metadata

The Application Source automatically adds profiling metadata to events:

```rust
// Internally, the source adds:
let mut profiling = ProfilingMetadata::new();
profiling.source_send_ns = Some(timestamp_ns());
```

This metadata flows through the system and can be used for performance analysis of your query pipeline.

### Component Lifecycle Events

The source emits component lifecycle events through the `event_tx` channel:

- **Starting**: When `.start()` is called
- **Running**: When the event processor begins
- **Stopping**: When `.stop()` is called
- **Stopped**: When shutdown is complete

These events can be monitored for observability in production systems.

---

## Quick Reference

### Common Imports

```rust
// Creating sources
use drasi_server_core::sources::application::{ApplicationSource, PropertyMapBuilder};
use drasi_server_core::sources::ApplicationSourceHandle;

// Working with data types
use drasi_core::models::{
    Element, ElementMetadata, ElementReference, ElementPropertyMap,
    ElementValue, SourceChange
};

// Testing
use drasi_server_core::test_support::helpers::create_test_application_source;
```

### Quick Start Snippet

```rust
use drasi_server_core::test_support::helpers::create_test_application_source;
use drasi_server_core::sources::application::PropertyMapBuilder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create source and handle
    let (_source, handle) = create_test_application_source("my-source").await;

    // Insert a node
    let props = PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .with_integer("age", 30)
        .build();

    handle.send_node_insert("person-1", vec!["Person"], props).await?;

    // Insert a relation
    let rel_props = PropertyMapBuilder::new()
        .with_string("since", "2020")
        .build();

    handle.send_relation_insert(
        "knows-1",
        vec!["KNOWS"],
        rel_props,
        "person-1",
        "person-2"
    ).await?;

    Ok(())
}
```

---

## Further Reading

- **Drasi Core Models**: See `drasi-core/core/src/models/` for detailed data type definitions
- **Source Trait**: See `drasi-server-core/src/sources/mod.rs` for the `Source` trait interface
- **Bootstrap Providers**: See `drasi-server-core/src/bootstrap/providers/application.rs` for bootstrap implementation details
- **Testing Examples**: See `drasi-server-core/src/sources/application/tests.rs` for comprehensive test examples
