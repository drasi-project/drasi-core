# Application Reaction

The Application reaction enables embedded scenarios by providing direct programmatic access to query results within your Rust application. Unlike other reactions that push data to external systems (HTTP, gRPC, etc.), the Application reaction delivers query results directly to your application code through an in-process channel.

## Purpose

The Application reaction is designed for:

- **Embedded Scenarios**: When DrasiServerCore is embedded as a library in your Rust application and you need direct access to query results
- **In-Process Data Consumption**: Receiving query results within the same process without network overhead
- **Programmatic Control**: Building custom logic to process query results with full control over consumption patterns
- **Testing and Development**: Simplifying integration testing by consuming results directly in test code

### When to Use Application Reaction vs Other Reactions

**Use Application Reaction when:**
- DrasiServerCore is embedded as a library in your application
- You need low-latency, in-process access to query results
- You want to implement custom result processing logic in Rust
- You're building automated tests that need to verify query results

**Use Other Reactions when:**
- You need to send results to external systems (use HTTP, gRPC, or Platform reactions)
- You want to expose results via Server-Sent Events (use SSE reaction)
- You need to log results for debugging (use Log reaction)
- You're integrating with external APIs or webhooks

## Configuration Properties

The Application reaction is typically created programmatically rather than through YAML configuration, but it supports the standard reaction configuration schema:

| Property | Type | Default | Required | Description |
|----------|------|---------|----------|-------------|
| `id` | string | N/A | Yes | Unique identifier for the reaction |
| `reaction_type` | string | "application" | Yes | Must be set to "application" |
| `queries` | array[string] | `[]` | No | List of query IDs to subscribe to. Empty array means subscribe to all queries |
| `auto_start` | boolean | `true` | No | Whether to automatically start the reaction when the server starts |
| `properties` | object | `{}` | No | Additional properties (currently unused by Application reaction) |

### Property Details

#### id
Unique identifier for this reaction instance. Used for lifecycle management and handle retrieval.

#### reaction_type
Must be set to `"application"` to create an Application reaction.

#### queries
An array of query IDs to filter results. The reaction will only receive results from the specified queries. If empty or not specified, the reaction receives results from all queries.

**Example:**
- `[]` - Receives results from all queries
- `["query-1"]` - Only receives results from "query-1"
- `["query-1", "query-2"]` - Receives results from "query-1" and "query-2"

#### auto_start
Controls whether the reaction starts automatically when the DrasiServerCore instance starts. Set to `false` if you want to manually control when the reaction begins processing results.

#### properties
A map of additional configuration properties. Currently unused by the Application reaction but available for future extensions or custom implementations.

## Configuration Examples

### YAML Configuration

```yaml
reactions:
  - id: "app-reaction"
    reaction_type: "application"
    queries:
      - "sensor-monitor"
      - "inventory-check"
    auto_start: true
    properties: {}
```

### Minimal YAML Configuration

```yaml
reactions:
  - id: "app-reaction"
    reaction_type: "application"
    queries: []  # Subscribe to all queries
```

### JSON Configuration

```json
{
  "reactions": [
    {
      "id": "app-reaction",
      "reaction_type": "application",
      "queries": [
        "sensor-monitor",
        "inventory-check"
      ],
      "auto_start": true,
      "properties": {}
    }
  ]
}
```

### JSON Configuration with Multiple Reactions

```json
{
  "reactions": [
    {
      "id": "app-results",
      "reaction_type": "application",
      "queries": [],
      "auto_start": true,
      "properties": {}
    },
    {
      "id": "app-filtered",
      "reaction_type": "application",
      "queries": ["critical-alerts"],
      "auto_start": false,
      "properties": {}
    }
  ]
}
```

## Programmatic Construction in Rust

The Application reaction is designed primarily for programmatic use. Here are comprehensive examples:

### Basic Usage with Fluent API

```rust
use drasi_server_core::{DrasiServerCore, Reaction};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build server with Application reaction
    let core = DrasiServerCore::builder()
        .with_id("my-app")
        .add_reaction(
            Reaction::application("app-reaction")
                .subscribe_to("my-query")
                .auto_start(true)
                .build()
        )
        .build()
        .await?;

    // Get the reaction handle
    let handle = core.reaction_handle("app-reaction")?;

    // Start the server
    core.start().await?;

    // Consume results (see consumption examples below)

    Ok(())
}
```

### Creating Reaction Directly

```rust
use drasi_server_core::config::ReactionConfig;
use drasi_server_core::reactions::ApplicationReaction;
use tokio::sync::mpsc;
use std::collections::HashMap;

// Create configuration
let config = ReactionConfig {
    id: "my-reaction".to_string(),
    reaction_type: "application".to_string(),
    queries: vec!["query-1".to_string(), "query-2".to_string()],
    auto_start: true,
    properties: HashMap::new(),
};

// Create component event channel
let (event_tx, _event_rx) = mpsc::channel(100);

// Create the reaction (returns both the reaction and its handle)
let (reaction, handle) = ApplicationReaction::new(config, event_tx);
```

### Subscribing to All Queries

```rust
let core = DrasiServerCore::builder()
    .with_id("my-app")
    .add_reaction(
        Reaction::application("all-results")
            // Don't call subscribe_to() to receive all query results
            .build()
    )
    .build()
    .await?;
```

### Multiple Application Reactions

```rust
let core = DrasiServerCore::builder()
    .with_id("my-app")
    .add_reaction(
        Reaction::application("critical-alerts")
            .subscribe_to("alert-query")
            .build()
    )
    .add_reaction(
        Reaction::application("analytics")
            .subscribe_to("metrics-query")
            .build()
    )
    .build()
    .await?;

// Get separate handles for each reaction
let alerts_handle = core.reaction_handle("critical-alerts")?;
let analytics_handle = core.reaction_handle("analytics")?;
```

## Consuming Results from ApplicationReactionHandle

The `ApplicationReactionHandle` provides multiple patterns for consuming query results:

### Pattern 1: Callback Subscription

```rust
use drasi_server_core::channels::QueryResult;

// Subscribe with a callback that processes each result
handle.subscribe(|result: QueryResult| {
    println!("Received result from query: {}", result.query_id);
    println!("Timestamp: {}", result.timestamp);
    println!("Results count: {}", result.results.len());

    // Process results
    for value in &result.results {
        println!("  Result: {}", value);
    }
}).await?;
```

### Pattern 2: Filtered Callback Subscription

```rust
// Subscribe with filtering for specific queries
let query_filter = vec!["query-1".to_string(), "query-2".to_string()];

handle.subscribe_filtered(query_filter, |result| {
    // Only called for results from query-1 or query-2
    println!("Filtered result from: {}", result.query_id);
}).await?;
```

### Pattern 3: Async Stream

```rust
// Get results as an async stream
if let Some(mut stream) = handle.as_stream().await {
    while let Some(result) = stream.next().await {
        println!("Query: {}, Results: {}", result.query_id, result.results.len());

        // Process result
        for value in &result.results {
            // Handle each result value
        }
    }
}
```

### Pattern 4: Try Next (Non-Blocking)

```rust
// Get stream and try to receive without blocking
if let Some(mut stream) = handle.as_stream().await {
    loop {
        match stream.try_next() {
            Some(result) => {
                println!("Got result: {}", result.query_id);
            }
            None => {
                // No result available, do other work
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }
        }
    }
}
```

### Pattern 5: Direct Receiver

```rust
// Take the raw receiver for maximum control
if let Some(mut rx) = handle.take_receiver().await {
    while let Some(result) = rx.recv().await {
        println!("Direct receive: {} results", result.results.len());
    }
} else {
    println!("Receiver already taken!");
}
```

### Pattern 6: Subscription with Options

```rust
use drasi_server_core::reactions::application::{SubscriptionOptions, Subscription};
use std::time::Duration;

// Create subscription with custom options
let options = SubscriptionOptions::new()
    .with_buffer_size(2000)
    .with_query_filter(vec!["important-query".to_string()])
    .with_timeout(Duration::from_secs(30))
    .with_batch_size(10);

let mut subscription = handle.subscribe_with_options(options).await?;

// Receive single result with timeout
while let Some(result) = subscription.recv().await {
    println!("Received: {}", result.query_id);
}
```

### Pattern 7: Batch Processing

```rust
let options = SubscriptionOptions::new()
    .with_batch_size(10);

let mut subscription = handle.subscribe_with_options(options).await?;

// Receive results in batches
loop {
    let batch = subscription.recv_batch().await;
    if batch.is_empty() {
        break;
    }

    println!("Processing batch of {} results", batch.len());
    for result in batch {
        // Process each result in the batch
    }
}
```

### Pattern 8: Async Stream Iteration

```rust
let subscription = handle.subscribe_with_options(SubscriptionOptions::default()).await?;
let mut stream = subscription.into_stream();

while let Some(result) = stream.next().await {
    // Process result
}
```

## Input Data Format

The Application reaction receives `QueryResult` structures from the query engine. Each result represents a change in query output.

### QueryResult Structure

```rust
pub struct QueryResult {
    /// Unique identifier of the query that produced this result
    pub query_id: String,

    /// UTC timestamp when the result was generated
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Array of result rows as JSON values
    pub results: Vec<serde_json::Value>,

    /// Additional metadata about the result
    pub metadata: HashMap<String, serde_json::Value>,

    /// Optional profiling data (if profiling is enabled)
    pub profiling: Option<ProfilingMetadata>,
}
```

### Example JSON Representation

```json
{
  "query_id": "sensor-monitor",
  "timestamp": "2025-01-15T10:30:45.123456Z",
  "results": [
    {
      "sensor_id": "temp-001",
      "temperature": 72.5,
      "location": "warehouse-a",
      "status": "normal"
    },
    {
      "sensor_id": "temp-002",
      "temperature": 68.3,
      "location": "warehouse-b",
      "status": "normal"
    }
  ],
  "metadata": {
    "operation": "update",
    "source": "sensor-data"
  },
  "profiling": null
}
```

### Metadata Fields

The `metadata` field may contain various information depending on the query and source:

- `operation`: Type of change (e.g., "insert", "update", "delete")
- `source`: Source ID that triggered the change
- Custom fields added by queries or middleware

### Accessing Result Data

```rust
handle.subscribe(|result| {
    println!("Query: {}", result.query_id);

    // Access metadata
    if let Some(operation) = result.metadata.get("operation") {
        println!("Operation: {}", operation);
    }

    // Process results
    for value in &result.results {
        // Results are serde_json::Value - deserialize to your types
        if let Some(sensor_id) = value.get("sensor_id").and_then(|v| v.as_str()) {
            if let Some(temp) = value.get("temperature").and_then(|v| v.as_f64()) {
                println!("Sensor {} temperature: {}", sensor_id, temp);
            }
        }
    }
}).await?;
```

## Output Data Format

The Application reaction doesn't produce output in the traditional sense - it provides data to your application through the `ApplicationReactionHandle` interface.

### ApplicationReactionHandle Interface

```rust
impl ApplicationReactionHandle {
    /// Take the receiver (can only be called once)
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<QueryResult>>;

    /// Subscribe to query results with a callback
    pub async fn subscribe<F>(&self, callback: F) -> Result<()>
    where
        F: FnMut(QueryResult) + Send + 'static;

    /// Subscribe to query results with filtering
    pub async fn subscribe_filtered<F>(
        &self,
        query_filter: Vec<String>,
        callback: F,
    ) -> Result<()>
    where
        F: FnMut(QueryResult) + Send + 'static;

    /// Subscribe to query results as a stream
    pub async fn as_stream(&self) -> Option<ResultStream>;

    /// Create a subscription with options
    pub async fn subscribe_with_options(
        &self,
        options: SubscriptionOptions,
    ) -> Result<Subscription>;

    /// Get the reaction id
    pub fn reaction_id(&self) -> &str;
}
```

### Channel Semantics

The Application reaction uses Tokio MPSC (multi-producer, single-consumer) channels:

- **Buffer Size**: 1000 messages by default
- **Backpressure**: If the channel fills, the reaction will wait until space is available
- **Single Consumer**: The receiver can only be taken once - subsequent calls to `take_receiver()` return `None`
- **Async**: All operations are async and work with Tokio runtime

### SubscriptionOptions

```rust
pub struct SubscriptionOptions {
    /// Maximum number of results to buffer (default: 1000)
    pub buffer_size: usize,

    /// Filter by query names (empty = all queries, default: [])
    pub query_filter: Vec<String>,

    /// Maximum time to wait for results (default: None = infinite)
    pub timeout: Option<Duration>,

    /// Batch size for receiving multiple results at once (default: None)
    pub batch_size: Option<usize>,
}
```

### ResultStream

```rust
pub struct ResultStream {
    /// Receive the next result (async, blocks until available)
    pub async fn next(&mut self) -> Option<QueryResult>;

    /// Try to receive the next result without waiting
    pub fn try_next(&mut self) -> Option<QueryResult>;
}
```

### Subscription

```rust
pub struct Subscription {
    /// Receive the next result (respects timeout if configured)
    pub async fn recv(&mut self) -> Option<QueryResult>;

    /// Try to receive without blocking
    pub fn try_recv(&mut self) -> Option<QueryResult>;

    /// Receive a batch of results (blocks for first result, then tries to fill batch)
    pub async fn recv_batch(&mut self) -> Vec<QueryResult>;

    /// Convert to an async iterator
    pub fn into_stream(self) -> SubscriptionStream;
}
```

## Troubleshooting

### Receiver Already Taken

**Problem:** Calling `take_receiver()`, `subscribe()`, `as_stream()`, or `subscribe_with_options()` returns an error or `None`.

**Cause:** The receiver can only be consumed once. After the first consumption, the underlying channel is owned by that consumer.

**Solution:**
- Only call one consumption method per handle
- If you need multiple consumers, create multiple Application reactions with the same query filter
- Clone the handle before consumption if you need to keep a reference

```rust
// WRONG - Second call will fail
let handle = core.reaction_handle("my-reaction")?;
handle.subscribe(|r| {}).await?;
handle.as_stream().await; // Returns None!

// RIGHT - Clone for multiple references
let handle = core.reaction_handle("my-reaction")?;
let handle_clone = handle.clone();
handle.subscribe(|r| {}).await?;
// Can still get reaction_id from clone
println!("Reaction: {}", handle_clone.reaction_id());
```

### Channel Buffer Overflow

**Problem:** Results appear to be delayed or the reaction stops processing.

**Cause:** The default channel buffer (1000 messages) is full because the consumer isn't processing results fast enough.

**Solution:**
- Process results more quickly in your callback
- Use larger buffer size with `SubscriptionOptions`
- Use batching to process multiple results at once
- Consider using multiple Application reactions to parallelize processing

```rust
let options = SubscriptionOptions::new()
    .with_buffer_size(10000); // Larger buffer

let mut subscription = handle.subscribe_with_options(options).await?;
```

### Missing Query Results

**Problem:** Not receiving results from specific queries.

**Cause:** The `queries` configuration property filters which results the reaction receives.

**Solution:**
- Check the reaction configuration includes the query ID
- Use an empty `queries` array to receive all results
- Verify the query is actually running and producing results

```rust
// Subscribe to all queries
Reaction::application("all-results")
    .build()  // Don't call subscribe_to()

// Subscribe to specific queries
Reaction::application("filtered")
    .subscribe_to("query-1")
    .subscribe_to("query-2")
    .build()
```

### Thread Safety and Async Runtime

**Problem:** Compilation errors about `Send` trait or async runtime errors.

**Cause:** The callback passed to `subscribe()` must be `Send + 'static`.

**Solution:**
- Ensure captured variables implement `Send`
- Use `Arc` for shared state
- Don't capture non-Send types like `Rc` or `RefCell`

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

let shared_state = Arc::new(Mutex::new(Vec::new()));
let state_clone = shared_state.clone();

handle.subscribe(move |result| {
    // This closure is Send because Arc and Mutex are Send
    let state = state_clone.clone();
    tokio::spawn(async move {
        let mut guard = state.lock().await;
        guard.push(result);
    });
}).await?;
```

### Handle Lifecycle Management

**Problem:** Handle is dropped before results are received.

**Cause:** The handle must stay alive for the subscription to continue receiving results.

**Solution:**
- Keep the handle in scope for the lifetime of your application
- Store the handle in a struct or static if needed
- Don't drop the handle until you're done receiving results

```rust
// WRONG - Handle dropped immediately
{
    let handle = core.reaction_handle("my-reaction")?;
    handle.subscribe(|r| {}).await?;
} // Handle dropped here, callback might not receive all results

// RIGHT - Keep handle alive
let handle = core.reaction_handle("my-reaction")?;
handle.subscribe(|r| {}).await?;
// ... do other work while results are being processed
// Handle stays alive until end of scope
```

## Limitations

### Single Receiver Constraint

The underlying channel can only have one receiver. Once consumed (via `take_receiver()`, `subscribe()`, etc.), subsequent attempts to get the receiver will fail. This is by design to prevent multiple consumers from competing for the same messages.

**Workaround:** Create multiple Application reactions if you need multiple independent consumers:

```rust
let core = DrasiServerCore::builder()
    .add_reaction(
        Reaction::application("consumer-1")
            .subscribe_to("my-query")
            .build()
    )
    .add_reaction(
        Reaction::application("consumer-2")
            .subscribe_to("my-query")
            .build()
    )
    .build()
    .await?;
```

### Channel Buffer Size

The default buffer size is 1000 messages. If results are produced faster than they're consumed, the channel will fill and the reaction will apply backpressure (blocking until space is available).

**Considerations:**
- Large buffer sizes increase memory usage
- Small buffer sizes may cause backpressure if consumption is slow
- Choose buffer size based on expected result volume and processing speed

**Configuration:**

```rust
let options = SubscriptionOptions::new()
    .with_buffer_size(5000); // Adjust based on your needs
```

### No Built-in Persistence

The Application reaction delivers results in-memory only. If your application crashes or the receiver is not consuming, results may be lost.

**Mitigation:**
- Implement your own persistence in the callback
- Use checkpoint/acknowledgment patterns
- Consider using Platform or Log reactions for durable result storage

### Query Filtering Granularity

Query filtering happens at the reaction level, not at the individual subscription level. If you need different handling for different queries, use multiple Application reactions.

### Performance Considerations

- **Callback Overhead**: Heavy processing in callbacks can block result delivery. Consider spawning tasks for expensive operations:

```rust
handle.subscribe(|result| {
    // Spawn task to avoid blocking
    tokio::spawn(async move {
        expensive_processing(result).await;
    });
}).await?;
```

- **Memory Usage**: Results are buffered in memory. High-volume queries can consume significant memory.

- **Serialization**: Results are already deserialized from the query engine. Additional serialization (e.g., to JSON) adds overhead.

### Concurrency Model

The Application reaction processes results sequentially. If you need parallel processing:

```rust
use tokio::task::JoinSet;

let mut tasks = JoinSet::new();

handle.subscribe(move |result| {
    // Spawn concurrent tasks for parallel processing
    tasks.spawn(async move {
        process_result(result).await
    });
}).await?;
```

### Tokio Runtime Requirement

The Application reaction requires a Tokio async runtime. It will not work with other async runtimes without adaptation.

### No Automatic Retry

If the callback fails or panics, the result is lost. Implement your own retry logic if needed:

```rust
handle.subscribe(|result| {
    let mut attempts = 0;
    while attempts < 3 {
        match process_result(&result) {
            Ok(_) => break,
            Err(e) => {
                attempts += 1;
                eprintln!("Processing failed (attempt {}): {}", attempts, e);
            }
        }
    }
}).await?;
```
