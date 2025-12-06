# Application Bootstrap Provider

## Overview

The Application Bootstrap Provider is a bootstrap plugin for Drasi that replays stored insert events during query subscription. It works in conjunction with the Application Source to provide initial data (bootstrap) to queries when they start or subscribe to a data source.

This provider implements in-memory replay of historical insert events, enabling queries to establish their initial state before processing real-time updates. It's designed primarily for testing scenarios, embedded applications, and situations where you need programmatic control over bootstrap data.

### Key Capabilities

- **In-Memory Event Replay**: Stores and replays insert events that were previously sent through an Application Source
- **Label-Based Filtering**: Filters bootstrap events based on node and relation labels requested by queries
- **Shared Data Storage**: Can share bootstrap data with Application Source instances via `Arc<RwLock<Vec<SourceChange>>>`
- **Isolated Storage Mode**: Can operate independently with its own isolated bootstrap data storage
- **Event Counting**: Tracks and reports the number of bootstrap events replayed to each query

### How It Works

1. **Event Storage**: Insert events sent through an Application Source are stored in a shared vector
2. **Bootstrap Request**: When a query subscribes to a source, it sends a bootstrap request with required labels
3. **Event Filtering**: The provider filters stored events to match the requested node/relation labels
4. **Event Replay**: Matching events are counted and made available to the query
5. **Completion**: The provider reports the count of replayed events

### Use Cases

**Ideal for:**
- Testing query behavior with pre-populated data
- Embedded Drasi applications that manage their own data
- Programmatic data generation scenarios
- Development and debugging environments
- Scenarios requiring precise control over bootstrap data

**Not suitable for:**
- Production systems requiring persistent bootstrap state (use PostgreSQL or Platform sources instead)
- Large datasets (all data is stored in memory)
- Distributed systems where bootstrap data must survive restarts

## Architecture

The Application Bootstrap Provider uses a simple architecture:

```
ApplicationSource
       |
       | Stores Insert Events
       v
   bootstrap_data: Arc<RwLock<Vec<SourceChange>>>
       ^
       | Shared Reference
       |
ApplicationBootstrapProvider
       |
       | Replays Events on Bootstrap Request
       v
   Query Subscription
```

### Components

1. **ApplicationBootstrapProvider**: The main provider struct that implements `BootstrapProvider` trait
2. **bootstrap_data**: Shared vector of `SourceChange::Insert` events
3. **ApplicationBootstrapProviderBuilder**: Fluent builder for creating provider instances

### Data Sharing

The provider can operate in two modes:

1. **Shared Data Mode**: Shares `bootstrap_data` with an Application Source, allowing it to replay actual insert events
2. **Isolated Mode**: Uses its own independent storage (useful for testing the provider itself)

## Configuration

The Application Bootstrap Provider does not use YAML configuration. It is created programmatically and configured through code.

### Builder Configuration

The provider uses a builder pattern for configuration:

```rust
use drasi_bootstrap_application::ApplicationBootstrapProvider;

// Create with isolated storage (for testing)
let provider = ApplicationBootstrapProvider::builder().build();

// Create with shared storage (for production use)
use std::sync::Arc;
use tokio::sync::RwLock;
use drasi_core::models::SourceChange;

let shared_data = Arc::new(RwLock::new(Vec::<SourceChange>::new()));
let provider = ApplicationBootstrapProvider::builder()
    .with_shared_data(shared_data)
    .build();
```

### Constructor Options

The provider offers multiple construction methods:

| Method | Description | Use Case |
|--------|-------------|----------|
| `new()` | Creates provider with isolated storage | Testing provider behavior |
| `with_shared_data(Arc<RwLock<Vec<SourceChange>>>)` | Creates provider sharing storage with Application Source | Normal usage |
| `builder()` | Returns builder for fluent configuration | Flexible configuration |
| `default()` | Same as `new()` | Default construction |

## Usage Examples

### Basic Setup with Application Source

The most common usage is connecting the bootstrap provider to an Application Source:

```rust
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_source_application::ApplicationSource;
use drasi_server_core::config::SourceConfig;
use std::collections::HashMap;
use tokio::sync::mpsc;

// Create Application Source
let config = SourceConfig {
    id: "my-source".to_string(),
    source_type: "application".to_string(),
    auto_start: true,
    properties: HashMap::new(),
    bootstrap_provider: None,
};

let (source_event_tx, _source_event_rx) = mpsc::channel(100);
let (event_tx, _event_rx) = mpsc::channel(100);

let (source, handle) = ApplicationSource::new(config, source_event_tx, event_tx);

// Create Bootstrap Provider sharing the source's bootstrap data
// Note: This requires accessing the source's internal bootstrap_data field
// In practice, ApplicationSource handles this internally
let bootstrap_data = source.get_bootstrap_data();
let provider = ApplicationBootstrapProvider::with_shared_data(bootstrap_data);
```

### Using the Builder Pattern

```rust
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use std::sync::Arc;
use tokio::sync::RwLock;
use drasi_core::models::SourceChange;

// Create shared bootstrap storage
let bootstrap_data = Arc::new(RwLock::new(Vec::<SourceChange>::new()));

// Build provider with shared data
let provider = ApplicationBootstrapProvider::builder()
    .with_shared_data(bootstrap_data.clone())
    .build();

// The bootstrap_data can now be populated by other components
// (typically by ApplicationSource when it receives insert events)
```

### Testing with Isolated Storage

```rust
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_core::models::{Element, ElementMetadata, SourceChange};

#[tokio::test]
async fn test_bootstrap_provider() {
    // Create provider with isolated storage
    let provider = ApplicationBootstrapProvider::new();

    // Manually populate bootstrap data for testing
    let element = Element::Node {
        metadata: create_test_metadata("node-1", vec!["Person"]),
        properties: create_test_properties(),
    };

    provider.store_insert_event(SourceChange::Insert { element }).await;

    // Verify stored events
    let stored = provider.get_stored_events().await;
    assert_eq!(stored.len(), 1);
}
```

### Storing Insert Events

The provider offers methods to manage stored events (typically called by Application Source):

```rust
use drasi_bootstrap_application::ApplicationBootstrapProvider;
use drasi_core::models::{Element, SourceChange};

let provider = ApplicationBootstrapProvider::new();

// Store an insert event
let element = Element::Node { /* ... */ };
provider.store_insert_event(SourceChange::Insert { element }).await;

// Get all stored events (for inspection or testing)
let events = provider.get_stored_events().await;
println!("Stored {} events", events.len());

// Clear stored events (for testing or reset)
provider.clear_stored_events().await;
```

### Label-Based Filtering

The provider automatically filters events based on query requirements:

```rust
// When a query requests bootstrap with specific labels:
// BootstrapRequest {
//     query_id: "my-query",
//     node_labels: vec!["Person", "Employee"],
//     relation_labels: vec!["WORKS_FOR"],
// }

// The provider will only replay events that match:
// - Nodes with labels "Person" OR "Employee"
// - Relations with label "WORKS_FOR"
// - All events if both label lists are empty
```

## API Reference

### ApplicationBootstrapProvider

#### Constructor Methods

##### `new() -> Self`
Creates a new provider with isolated bootstrap data storage.

**Returns**: A provider instance with its own independent storage

**Example**:
```rust
let provider = ApplicationBootstrapProvider::new();
```

---

##### `with_shared_data(bootstrap_data: Arc<RwLock<Vec<SourceChange>>>) -> Self`
Creates a provider sharing bootstrap data with an Application Source.

**Parameters**:
- `bootstrap_data`: Shared reference to the Application Source's bootstrap data

**Returns**: A provider instance connected to shared storage

**Example**:
```rust
let shared_data = Arc::new(RwLock::new(Vec::new()));
let provider = ApplicationBootstrapProvider::with_shared_data(shared_data);
```

---

##### `builder() -> ApplicationBootstrapProviderBuilder`
Creates a builder for fluent configuration.

**Returns**: Builder instance for constructing the provider

**Example**:
```rust
let provider = ApplicationBootstrapProvider::builder()
    .with_shared_data(shared_data)
    .build();
```

---

##### `default() -> Self`
Creates a provider using default settings (same as `new()`).

**Returns**: A provider instance with isolated storage

---

#### Event Management Methods

##### `store_insert_event(&self, change: SourceChange) -> ()`
Stores an insert event for future bootstrap replay.

**Parameters**:
- `change`: A `SourceChange::Insert` event (other variants are ignored)

**Behavior**:
- Only stores `Insert` events; ignores `Update`, `Delete`, and `Future` events
- Events are appended to the bootstrap data vector
- Thread-safe (uses async write lock)

**Example**:
```rust
let element = Element::Node { /* ... */ };
provider.store_insert_event(SourceChange::Insert { element }).await;
```

---

##### `get_stored_events(&self) -> Vec<SourceChange>`
Returns a copy of all stored insert events.

**Returns**: Vector of stored `SourceChange::Insert` events

**Use Cases**:
- Testing and verification
- Debugging bootstrap behavior
- Inspecting stored data

**Example**:
```rust
let events = provider.get_stored_events().await;
println!("Bootstrap data contains {} events", events.len());
```

---

##### `clear_stored_events(&self) -> ()`
Removes all stored events from bootstrap storage.

**Use Cases**:
- Resetting state between tests
- Clearing stale data
- Reinitializing bootstrap state

**Example**:
```rust
provider.clear_stored_events().await;
assert_eq!(provider.get_stored_events().await.len(), 0);
```

---

### ApplicationBootstrapProviderBuilder

#### Methods

##### `new() -> Self`
Creates a new builder instance.

**Example**:
```rust
let builder = ApplicationBootstrapProviderBuilder::new();
```

---

##### `with_shared_data(self, data: Arc<RwLock<Vec<SourceChange>>>) -> Self`
Configures the builder to use shared bootstrap data.

**Parameters**:
- `data`: Shared reference to bootstrap data storage

**Returns**: Builder instance for method chaining

**Example**:
```rust
let builder = ApplicationBootstrapProviderBuilder::new()
    .with_shared_data(shared_data);
```

---

##### `build(self) -> ApplicationBootstrapProvider`
Constructs the final `ApplicationBootstrapProvider` instance.

**Returns**: Configured provider instance

**Example**:
```rust
let provider = ApplicationBootstrapProviderBuilder::new()
    .with_shared_data(shared_data)
    .build();
```

---

##### `default() -> Self`
Creates a builder using default settings.

**Example**:
```rust
let provider = ApplicationBootstrapProviderBuilder::default().build();
```

---

### BootstrapProvider Trait Implementation

##### `bootstrap(&self, request: BootstrapRequest, context: &BootstrapContext, event_tx: BootstrapEventSender) -> Result<usize>`

Processes a bootstrap request from a query subscription.

**Parameters**:
- `request`: Bootstrap request containing query ID and required labels
- `context`: Bootstrap context (currently unused by this provider)
- `event_tx`: Channel for sending bootstrap events (currently unused - ApplicationSource handles event sending)

**Returns**:
- `Result<usize>`: Number of matching events found, or error if bootstrap fails

**Behavior**:
1. Logs the bootstrap request details
2. Reads stored insert events from shared storage
3. Filters events based on requested node and relation labels
4. Counts matching events
5. Returns the count (actual event transmission is handled by ApplicationSource)

**Label Matching Logic**:
- Node labels: Event matches if any node label matches any requested label
- Relation labels: Event matches if any relation label matches any requested label
- Empty label lists: Matches all events of that type
- Both lists empty: Matches all events

---

## Integration with Application Source

The Application Bootstrap Provider is designed to work seamlessly with the Application Source:

### Event Flow

```
Application Code
       |
       | handle.send_node_insert(...)
       v
ApplicationSource
       |
       +-- Stores Insert Event in bootstrap_data
       |
       +-- Forwards Event to Query Engine
       v
Query Engine
```

### Bootstrap Flow

```
Query Subscription Request
       |
       v
ApplicationSource.subscribe()
       |
       | Reads bootstrap_data
       |
       +-- Filters by Requested Labels
       |
       +-- Sends Bootstrap Events
       v
Query Initialization
```

### Important Notes

1. **Current Implementation**: As of the current version, ApplicationSource handles bootstrap directly in its `subscribe()` method (lines 337-384 in `sources/application/mod.rs`). The ApplicationBootstrapProvider exists for testing and potential future integration where bootstrap logic might be delegated to the provider system.

2. **Data Connection**: To connect the provider to an Application Source, you must share the same `Arc<RwLock<Vec<SourceChange>>>` reference between them.

3. **Event Storage**: Only `Insert` events are stored. `Update` and `Delete` events are not included in bootstrap data, as bootstrap represents the initial creation state, not the current state.

## Data Types

### SourceChange

Events that can be stored and replayed:

```rust
pub enum SourceChange {
    Insert { element: Element },     // Stored for bootstrap
    Update { element: Element },     // NOT stored for bootstrap
    Delete { metadata: ElementMetadata }, // NOT stored for bootstrap
    Future { future_ref: FutureElementRef }, // NOT supported in bootstrap
}
```

### BootstrapRequest

Request sent by queries when subscribing:

```rust
pub struct BootstrapRequest {
    pub query_id: String,
    pub node_labels: Vec<String>,     // Filter nodes by these labels
    pub relation_labels: Vec<String>, // Filter relations by these labels
}
```

## Performance Considerations

### Memory Usage

- **All events stored in memory**: The provider stores every insert event in a `Vec<SourceChange>`
- **No size limits**: The vector grows unbounded with the number of insert events
- **Recommendation**: For large datasets, consider using a persistent bootstrap provider (PostgreSQL, Platform)

### Concurrency

- **Thread-safe**: Uses `RwLock` for concurrent access to bootstrap data
- **Read-heavy optimization**: Multiple readers can access bootstrap data simultaneously
- **Write blocking**: Storing events acquires a write lock, blocking all readers and writers

### Scalability

| Scenario | Performance Impact |
|----------|-------------------|
| Few insert events (< 1000) | Excellent - minimal overhead |
| Medium datasets (1000-10000) | Good - vector operations efficient |
| Large datasets (> 10000) | Poor - consider persistent storage |
| High write rate | Moderate - write lock contention possible |
| Many concurrent queries | Good - read locks don't block each other |

## Comparison with Other Bootstrap Providers

### Application Bootstrap vs PostgreSQL Bootstrap

| Feature | Application | PostgreSQL |
|---------|-------------|------------|
| **Storage** | In-memory | Database snapshot |
| **Persistence** | Lost on restart | Survives restarts |
| **Data Source** | Programmatic | Database tables |
| **Performance** | Fastest (in-memory) | Slower (database query) |
| **Use Case** | Testing, embedded | Production systems |

### Application Bootstrap vs Platform Bootstrap

| Feature | Application | Platform |
|---------|-------------|----------|
| **Storage** | In-memory vector | Remote Query API + Redis Streams |
| **Data Source** | Direct API calls | External Drasi instance |
| **Complexity** | Minimal | High (requires external services) |
| **Use Case** | Single-instance apps | Multi-environment integration |

## Known Limitations

1. **No Persistence**: All bootstrap data is lost when the application restarts
2. **Memory Bounded**: Large datasets will consume significant memory
3. **Insert Events Only**: Updates and deletes are not reflected in bootstrap data
4. **No Deduplication**: Duplicate insert events with the same element ID are stored
5. **Manual Management**: Application code must ensure bootstrap data is populated correctly
6. **No Automatic Cleanup**: Old or deleted elements remain in bootstrap storage
7. **Testing Focused**: Primarily designed for testing scenarios, not production use

## Best Practices

### For Testing

```rust
// Clear bootstrap data between tests
#[tokio::test]
async fn test_scenario() {
    let provider = ApplicationBootstrapProvider::new();
    provider.clear_stored_events().await;

    // Populate test data
    // ... send insert events

    // Run test
    // ... verify behavior

    // Clean up
    provider.clear_stored_events().await;
}
```

### For Embedded Applications

```rust
// Share bootstrap data with Application Source
let bootstrap_data = Arc::new(RwLock::new(Vec::new()));

let provider = ApplicationBootstrapProvider::with_shared_data(bootstrap_data.clone());
let (source, handle) = ApplicationSource::with_bootstrap_data(config, bootstrap_data);

// Bootstrap data is automatically populated as you send events
handle.send_node_insert("node-1", vec!["Person"], props).await?;
```

### For Development

```rust
// Inspect bootstrap data during debugging
let events = provider.get_stored_events().await;
for (i, event) in events.iter().enumerate() {
    println!("Bootstrap Event {}: {:?}", i, event);
}
```

## Troubleshooting

### Bootstrap Data Not Replaying

**Problem**: Queries don't receive expected bootstrap data

**Solutions**:
1. Verify the provider shares the same `Arc<RwLock<Vec<SourceChange>>>` as the Application Source
2. Check that insert events are being stored (use `get_stored_events()`)
3. Ensure query label filters match stored event labels
4. Verify the provider is registered with the bootstrap system

### Memory Usage Growing

**Problem**: Application memory usage increases over time

**Solutions**:
1. Periodically call `clear_stored_events()` if bootstrap data is no longer needed
2. Consider implementing a maximum event limit
3. Switch to a persistent bootstrap provider for production
4. Use a different source type (PostgreSQL, Platform) for large datasets

### Bootstrap Events Don't Match Current State

**Problem**: Bootstrap data reflects old state after updates/deletes

**Explanation**: This is expected behavior. Bootstrap only stores insert events, not the cumulative effect of updates and deletes.

**Solutions**:
1. Accept that bootstrap represents creation events, not current state
2. Use a different source type that supports state snapshots (PostgreSQL)
3. Manually rebuild bootstrap data from current state when needed

## Thread Safety

The Application Bootstrap Provider is fully thread-safe:

- **Concurrent Reads**: Multiple threads can call `get_stored_events()` simultaneously
- **Concurrent Writes**: `store_insert_event()` safely serializes writes
- **Async-Safe**: All methods use async locks compatible with Tokio runtime
- **Clone-Safe**: The underlying `Arc<RwLock<>>` can be cloned and shared

## Dependencies

The provider requires these crates:

```toml
[dependencies]
drasi-lib = { path = "../../../lib" }
drasi-core = { path = "../../../core" }
anyhow = "1.0"
async-trait = "0.1"
log = "0.4"
tokio = { version = "1.0", features = ["sync"] }
```

## License

Licensed under the Apache License, Version 2.0. See the LICENSE file for details.

## Further Reading

- **Application Source Documentation**: See `/drasi-core/components/sources/application/src/README.md` for details on the Application Source
- **Bootstrap System**: See `/drasi-lib/src/bootstrap/mod.rs` for the bootstrap provider trait
- **Source Changes**: See `/drasi-core/core/src/models/` for `SourceChange` and `Element` definitions
