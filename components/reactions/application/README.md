# Application Reaction

## Overview

The Application Reaction component provides programmatic access to continuous query results directly within your Rust application. Unlike other reaction types (HTTP, gRPC, SSE, Log) that send results to external systems, the Application Reaction delivers results through an in-process channel, enabling direct consumption of query results with zero network overhead.

### Key Capabilities

- **In-Process Result Delivery**: Receive query results via async channels without network calls
- **Multiple Consumption Patterns**: Callbacks, async streams, or flexible subscriptions
- **Filtering & Buffering**: Configure query filtering, buffer sizes, timeouts, and batch processing
- **Type-Safe API**: Strongly-typed Rust interfaces with comprehensive error handling
- **Zero-Copy Architecture**: Results delivered through efficient async channels
- **Single Consumer Model**: Each reaction creates one handle for result consumption

### Use Cases

1. **Embedded Applications**: Applications that embed Drasi as a library and need direct access to query results
2. **Real-Time Processing**: Low-latency reaction to data changes without network overhead
3. **Integration Testing**: Test harness for validating query behavior programmatically
4. **Custom Business Logic**: Complex application logic that responds to continuous query results
5. **Data Pipelines**: Stream query results to custom processing pipelines
6. **Analytics & Monitoring**: Real-time dashboards and monitoring systems built in Rust

## Architecture

```
┌─────────────────┐
│  Drasi Queries  │
└────────┬────────┘
         │ Query Results
         ▼
┌─────────────────────────┐
│  ApplicationReaction    │
│  (Priority Queue)       │
└────────┬────────────────┘
         │ mpsc::channel
         ▼
┌─────────────────────────┐
│ ApplicationReactionHandle│
└────────┬────────────────┘
         │
    ┌────┴─────┬──────────┬────────────┐
    ▼          ▼          ▼            ▼
Callback   Stream    Subscription   Raw Receiver
```

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent API for creating application reactions:

```rust
use drasi_reaction_application::ApplicationReaction;

let (reaction, handle) = ApplicationReaction::builder("my-app-reaction")
    .with_query("users")
    .with_query("orders")
    .with_priority_queue_capacity(5000)
    .with_auto_start(true)
    .build();

// Add to DrasiLib
drasi_lib.add_reaction(reaction).await?;

// Use handle to receive results
let mut subscription = handle.subscribe_with_options(Default::default()).await?;
while let Some(result) = subscription.recv().await {
    println!("Received {} results from {}", result.results.len(), result.query_id);
}
```

### Constructor Pattern

For simple cases, use the direct constructor:

```rust
use drasi_reaction_application::ApplicationReaction;

let (reaction, handle) = ApplicationReaction::new(
    "my-app-reaction",
    vec!["users".to_string(), "orders".to_string()]
);

drasi_lib.add_reaction(reaction).await?;
```

### Configuration Struct

The `ApplicationReactionConfig` struct is used for serialization/deserialization:

```rust
use drasi_reaction_application::ApplicationReactionConfig;
use std::collections::HashMap;

let config = ApplicationReactionConfig {
    properties: HashMap::new(),  // Flexible properties for future extensions
};
```

## Configuration Options

### Builder Methods

| Method | Description | Data Type | Default |
|--------|-------------|-----------|---------|
| `with_query(query_id)` | Add a single query ID to subscribe to | `String` | Empty list |
| `with_queries(query_ids)` | Set multiple query IDs to subscribe to | `Vec<String>` | Empty list |
| `with_priority_queue_capacity(capacity)` | Set the priority queue buffer size | `usize` | 1000 |
| `with_auto_start(auto_start)` | Set whether reaction auto-starts | `bool` | `true` |

### Subscription Options

Configure how results are received using `SubscriptionOptions`:

| Option | Description | Data Type | Default |
|--------|-------------|-----------|---------|
| `buffer_size` | Maximum number of results to buffer | `usize` | 1000 |
| `query_filter` | Filter results by query IDs (empty = all) | `Vec<String>` | Empty |
| `timeout` | Maximum time to wait for results | `Option<Duration>` | None (wait forever) |
| `batch_size` | Maximum results per batch | `Option<usize>` | None (10 for batches) |

## Output Schema

### QueryResult Structure

Results are delivered as `QueryResult` objects with the following schema:

```rust
pub struct QueryResult {
    /// The ID of the query that produced these results
    pub query_id: String,

    /// Timestamp when the result was generated
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Result rows as JSON values
    pub results: Vec<serde_json::Value>,

    /// Additional metadata about the results
    pub metadata: HashMap<String, serde_json::Value>,

    /// Optional profiling information for performance analysis
    pub profiling: Option<ProfilingMetadata>,
}
```

### Result Format

Each result in the `results` array is a JSON object representing a row:

```json
{
  "query_id": "users",
  "timestamp": "2025-12-05T10:30:00Z",
  "results": [
    {
      "id": 123,
      "name": "Alice",
      "email": "alice@example.com"
    },
    {
      "id": 456,
      "name": "Bob",
      "email": "bob@example.com"
    }
  ],
  "metadata": {},
  "profiling": null
}
```

## Usage Examples

### Example 1: Basic Subscription (Recommended)

The most flexible and recommended approach using subscriptions:

```rust
use drasi_reaction_application::{ApplicationReaction, subscription::SubscriptionOptions};

// Create reaction and handle
let (reaction, handle) = ApplicationReaction::builder("results")
    .with_query("users")
    .build();

// Add to DrasiLib
drasi_lib.add_reaction(reaction).await?;

// Create subscription with default options
let mut subscription = handle.subscribe_with_options(
    SubscriptionOptions::default()
).await?;

// Receive results one at a time
while let Some(result) = subscription.recv().await {
    println!("Query: {}, Results: {}", result.query_id, result.results.len());
    for row in result.results {
        println!("  {:?}", row);
    }
}
```

### Example 2: Callback Pattern

Process results with a callback function (spawns background task):

```rust
use drasi_reaction_application::ApplicationReaction;

let (reaction, handle) = ApplicationReaction::builder("results")
    .with_queries(vec!["users".to_string(), "orders".to_string()])
    .build();

drasi_lib.add_reaction(reaction).await?;

// Subscribe with callback - runs in background
handle.subscribe(|result| {
    println!("Query: {}", result.query_id);
    println!("Received {} results", result.results.len());

    for row in result.results {
        // Process each row
        println!("  {:?}", row);
    }
}).await?;

// Keep main task alive while callback processes results
tokio::time::sleep(Duration::from_secs(60)).await;
```

### Example 3: Async Stream Pattern

Use async iteration for processing results:

```rust
use drasi_reaction_application::ApplicationReaction;

let (reaction, handle) = ApplicationReaction::builder("results")
    .with_query("sensors")
    .build();

drasi_lib.add_reaction(reaction).await?;

// Convert to stream for async iteration
if let Some(mut stream) = handle.as_stream().await {
    while let Some(result) = stream.next().await {
        println!("Sensor reading: {:?}", result);
    }
}
```

### Example 4: Filtered Subscription

Only receive results from specific queries:

```rust
use drasi_reaction_application::{ApplicationReaction, subscription::SubscriptionOptions};

let (reaction, handle) = ApplicationReaction::builder("results")
    .with_queries(vec![
        "users".to_string(),
        "orders".to_string(),
        "products".to_string()
    ])
    .build();

drasi_lib.add_reaction(reaction).await?;

// Only receive "users" query results
let options = SubscriptionOptions::default()
    .with_query_filter(vec!["users".to_string()]);

let mut subscription = handle.subscribe_with_options(options).await?;

while let Some(result) = subscription.recv().await {
    // Only user results arrive here
    println!("User update: {:?}", result);
}
```

### Example 5: Batch Processing

Receive multiple results at once for high-throughput scenarios:

```rust
use drasi_reaction_application::{ApplicationReaction, subscription::SubscriptionOptions};

let (reaction, handle) = ApplicationReaction::builder("results")
    .with_query("high-volume-data")
    .with_priority_queue_capacity(10000)  // Large buffer
    .build();

drasi_lib.add_reaction(reaction).await?;

// Configure for batch processing
let options = SubscriptionOptions::default()
    .with_buffer_size(5000)
    .with_batch_size(100);  // Receive up to 100 results at a time

let mut subscription = handle.subscribe_with_options(options).await?;

loop {
    let batch = subscription.recv_batch().await;
    if batch.is_empty() {
        break;  // Channel closed
    }

    println!("Processing batch of {} results", batch.len());
    for result in batch {
        // Process each result
    }
}
```

### Example 6: Timeout Configuration

Use timeouts to prevent indefinite blocking:

```rust
use drasi_reaction_application::{ApplicationReaction, subscription::SubscriptionOptions};
use std::time::Duration;

let (reaction, handle) = ApplicationReaction::builder("results")
    .with_query("sporadic-data")
    .build();

drasi_lib.add_reaction(reaction).await?;

// Configure timeout
let options = SubscriptionOptions::default()
    .with_timeout(Duration::from_secs(30));

let mut subscription = handle.subscribe_with_options(options).await?;

loop {
    match subscription.recv().await {
        Some(result) => {
            println!("Received result: {:?}", result);
        }
        None => {
            println!("Timeout or channel closed");
            break;
        }
    }
}
```

### Example 7: Non-Blocking Polling

Check for results without blocking:

```rust
use drasi_reaction_application::ApplicationReaction;
use tokio::time::{sleep, Duration};

let (reaction, handle) = ApplicationReaction::builder("results")
    .with_query("events")
    .build();

drasi_lib.add_reaction(reaction).await?;

let mut subscription = handle.subscribe_with_options(Default::default()).await?;

loop {
    // Non-blocking check for results
    match subscription.try_recv() {
        Some(result) => {
            println!("Got result: {:?}", result);
        }
        None => {
            // Do other work
            println!("No results available, doing other work...");
            sleep(Duration::from_millis(100)).await;
        }
    }
}
```

### Example 8: Multiple Consumers (Separate Reactions)

If you need multiple consumers, create separate application reactions:

```rust
use drasi_reaction_application::ApplicationReaction;

// Create separate reactions for different consumers
let (reaction1, handle1) = ApplicationReaction::builder("consumer-1")
    .with_query("users")
    .build();

let (reaction2, handle2) = ApplicationReaction::builder("consumer-2")
    .with_query("users")
    .build();

// Add both reactions
drasi_lib.add_reaction(reaction1).await?;
drasi_lib.add_reaction(reaction2).await?;

// Each consumer gets its own copy of results
tokio::spawn(async move {
    let mut sub1 = handle1.subscribe_with_options(Default::default()).await.unwrap();
    while let Some(result) = sub1.recv().await {
        println!("Consumer 1: {:?}", result);
    }
});

tokio::spawn(async move {
    let mut sub2 = handle2.subscribe_with_options(Default::default()).await.unwrap();
    while let Some(result) = sub2.recv().await {
        println!("Consumer 2: {:?}", result);
    }
});
```

## Important Considerations

### Single Consumer Model

Each `ApplicationReactionHandle` can only be consumed once. The underlying receiver is taken on first use:

- ✅ **Valid**: Call one consumption method (subscribe, as_stream, subscribe_with_options, take_receiver)
- ❌ **Invalid**: Call multiple consumption methods on the same handle
- ✅ **Solution**: Create multiple application reactions for multiple consumers

```rust
let (reaction, handle) = ApplicationReaction::builder("results").build();

// This works - first call succeeds
let mut subscription = handle.subscribe_with_options(Default::default()).await?;

// This fails - receiver already taken
let result = handle.as_stream().await;
assert!(result.is_none());  // Returns None
```

### Cloning Handles

`ApplicationReactionHandle` is `Clone`, but all clones share the same receiver:

```rust
let (reaction, handle1) = ApplicationReaction::builder("results").build();
let handle2 = handle1.clone();

// Only ONE of these will succeed (whichever is called first)
let result1 = handle1.take_receiver().await;  // Gets the receiver
let result2 = handle2.take_receiver().await;  // Returns None
```

### Thread Safety

- `ApplicationReactionHandle` is thread-safe and can be shared across threads
- `Subscription` and `ResultStream` are NOT `Send` - use within a single task
- Callbacks must be `Send + 'static` as they run in background tasks

### Priority Queue Behavior

Results are delivered in timestamp order using a priority queue:

- Default capacity: 1000 results
- Configurable via `with_priority_queue_capacity()`
- Larger capacity = more memory, better handling of out-of-order results
- Results are automatically sorted by timestamp before delivery

### Error Handling

Methods return `anyhow::Result` for error handling:

```rust
use anyhow::Result;

async fn handle_results(handle: ApplicationReactionHandle) -> Result<()> {
    let mut subscription = handle.subscribe_with_options(Default::default()).await?;

    while let Some(result) = subscription.recv().await {
        // Process result
    }

    Ok(())
}
```

### Performance Considerations

1. **Buffer Sizing**: Larger buffers (both priority queue and subscription) handle bursts better
2. **Batch Processing**: Use `recv_batch()` for high-throughput scenarios
3. **Query Filtering**: Filter early with `query_filter` to reduce processing overhead
4. **Memory Usage**: Each buffered result consumes memory - size buffers appropriately
5. **Zero-Copy**: Results are cloned from the priority queue but use Arc internally

## API Reference

### ApplicationReaction

```rust
impl ApplicationReaction {
    // Builder pattern (recommended)
    pub fn builder(id: impl Into<String>) -> ApplicationReactionBuilder;

    // Direct constructor
    pub fn new(id: impl Into<String>, queries: Vec<String>)
        -> (Self, ApplicationReactionHandle);
}
```

### ApplicationReactionBuilder

```rust
impl ApplicationReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self;
    pub fn with_queries(self, queries: Vec<String>) -> Self;
    pub fn with_query(self, query_id: impl Into<String>) -> Self;
    pub fn with_priority_queue_capacity(self, capacity: usize) -> Self;
    pub fn with_auto_start(self, auto_start: bool) -> Self;
    pub fn build(self) -> (ApplicationReaction, ApplicationReactionHandle);
}
```

### ApplicationReactionHandle

```rust
impl ApplicationReactionHandle {
    // Flexible subscription (recommended)
    pub async fn subscribe_with_options(
        &self,
        options: SubscriptionOptions
    ) -> Result<Subscription>;

    // Callback pattern
    pub async fn subscribe<F>(&self, callback: F) -> Result<()>
        where F: FnMut(QueryResult) + Send + 'static;

    pub async fn subscribe_filtered<F>(
        &self,
        query_filter: Vec<String>,
        callback: F
    ) -> Result<()>
        where F: FnMut(QueryResult) + Send + 'static;

    // Stream pattern
    pub async fn as_stream(&self) -> Option<ResultStream>;

    // Low-level API
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<QueryResult>>;

    // Metadata
    pub fn reaction_id(&self) -> &str;
}
```

### SubscriptionOptions

```rust
impl SubscriptionOptions {
    pub fn new() -> Self;
    pub fn default() -> Self;
    pub fn with_buffer_size(self, size: usize) -> Self;
    pub fn with_query_filter(self, queries: Vec<String>) -> Self;
    pub fn with_timeout(self, timeout: Duration) -> Self;
    pub fn with_batch_size(self, size: usize) -> Self;
}
```

### Subscription

```rust
impl Subscription {
    // Blocking receive (with optional timeout)
    pub async fn recv(&mut self) -> Option<QueryResult>;

    // Non-blocking receive
    pub fn try_recv(&mut self) -> Option<QueryResult>;

    // Batch receive
    pub async fn recv_batch(&mut self) -> Vec<QueryResult>;

    // Convert to stream
    pub fn into_stream(self) -> SubscriptionStream;
}
```

### ResultStream / SubscriptionStream

```rust
impl ResultStream {
    pub async fn next(&mut self) -> Option<QueryResult>;
    pub fn try_next(&mut self) -> Option<QueryResult>;
}

impl SubscriptionStream {
    pub async fn next(&mut self) -> Option<QueryResult>;
}
```

## Testing

Run the component tests:

```bash
cd drasi-core/components/reactions/application
cargo test
```

Run with logging:

```bash
RUST_LOG=debug cargo test -- --nocapture
```

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
