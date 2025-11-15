# Application Reaction

The Application reaction provides direct programmatic access to query results for embedded scenarios. It delivers results to your Rust application code through an in-process channel, eliminating network overhead.

## Purpose

The Application reaction enables:

- **Embedded Integration**: Direct access to query results when DrasiServerCore is used as a library
- **Low-Latency Processing**: In-process data consumption without network overhead
- **Custom Logic**: Full control over result processing with multiple consumption patterns
- **Testing**: Simplified integration testing by consuming results directly in test code

### When to Use

**Use Application Reaction:**
- For embedded library scenarios requiring direct result access
- When you need low-latency, in-process data consumption
- To implement custom result processing logic in Rust
- For automated tests that verify query results

**Use Other Reactions:**
- HTTP/gRPC/Platform - Send results to external systems
- SSE - Expose results via Server-Sent Events
- Log - Debug logging of results
- Custom - External API/webhook integrations

## Configuration

### Configuration Settings

The Application Reaction supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the reaction | Any string | **(Required)** |
| `queries` | Array[String] | IDs of queries this reaction subscribes to | Array of query IDs | **(Required)** |
| `reaction_type` | String | Reaction type discriminator | "application" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this reaction | true, false | `true` |
| `properties` | HashMap<String, serde_json::Value> | Application-specific properties. This is a flexible container for any custom configuration values your application might need. All properties are stored as JSON values | Map of property names to JSON values | Empty HashMap |
| `priority_queue_capacity` | Integer (Optional) | Maximum events in priority queue before backpressure. Controls event queuing before the reaction processes them. Higher values allow more buffering but use more memory | Any positive integer | `10000` |

**Note**: The Application reaction uses a flexible configuration approach where all properties are stored in a flattened HashMap. Unlike other reactions with strongly-typed configuration, you can add any custom properties you need and access them as JSON values.

### Standard Reaction Properties

These are part of the general `ReactionConfig` structure, not ApplicationReactionConfig:

- `id` (String, Required): Unique identifier for the reaction
- `reaction_type` (String): Must be `"application"`
- `queries` (Vec<String>): List of query IDs to subscribe to
- `auto_start` (bool): Whether to automatically start the reaction
- `priority_queue_capacity` (Option<usize>): Optional capacity for the priority queue

### Configuration Examples

**YAML:**
```yaml
reactions:
  - id: "app-reaction"
    queries: ["sensor-monitor", "inventory-check"]
    reaction_type: "application"
    auto_start: true
    priority_queue_capacity: 20000
```

**JSON:**
```json
{
  "id": "app-reaction",
  "queries": ["sensor-monitor"],
  "reaction_type": "application",
  "auto_start": true
}
```

## Programmatic Usage

### Basic Setup

```rust
use drasi_server_core::{DrasiServerCore, Reaction};

let core = DrasiServerCore::builder()
    .add_reaction(
        Reaction::application("app-reaction")
            .subscribe_to("my-query")
            .build()
    )
    .build()
    .await?;

// Get handle and start server
let handle = core.reaction_handle("app-reaction")?;
core.start().await?;
```

### Multiple Reactions

```rust
let core = DrasiServerCore::builder()
    .add_reaction(Reaction::application("alerts").subscribe_to("alert-query").build())
    .add_reaction(Reaction::application("metrics").subscribe_to("metrics-query").build())
    .build()
    .await?;

let alerts_handle = core.reaction_handle("alerts")?;
let metrics_handle = core.reaction_handle("metrics")?;
```

## Consuming Results

The `ApplicationReactionHandle` provides multiple consumption patterns:

### 1. Callback (Simple)

```rust
handle.subscribe(|result| {
    println!("Query: {}, Results: {}", result.query_id, result.results.len());
    for value in &result.results {
        // Process each value
    }
}).await?;
```

### 2. Filtered Callback

```rust
handle.subscribe_filtered(vec!["query-1".to_string()], |result| {
    println!("Filtered result: {}", result.query_id);
}).await?;
```

### 3. Async Stream

```rust
if let Some(mut stream) = handle.as_stream().await {
    while let Some(result) = stream.next().await {
        // Process result
    }
}
```

### 4. Subscription with Options (Recommended)

```rust
use drasi_server_core::SubscriptionOptions;

let options = SubscriptionOptions::default()
    .with_buffer_size(2000)
    .with_query_filter(vec!["important-query".to_string()])
    .with_batch_size(10);

let mut subscription = handle.subscribe_with_options(options).await?;
while let Some(result) = subscription.recv().await {
    // Process result
}
```

### 5. Batch Processing

```rust
let options = SubscriptionOptions::default().with_batch_size(50);
let mut subscription = handle.subscribe_with_options(options).await?;

loop {
    let batch = subscription.recv_batch().await;
    if batch.is_empty() { break; }

    println!("Processing {} results", batch.len());
    for result in batch {
        // Process batch
    }
}
```

## Data Format

### QueryResult Structure

```rust
pub struct QueryResult {
    pub query_id: String,                                    // Query that produced this result
    pub timestamp: chrono::DateTime<chrono::Utc>,           // When result was generated
    pub results: Vec<serde_json::Value>,                    // Result rows as JSON
    pub metadata: HashMap<String, serde_json::Value>,       // Additional metadata
    pub profiling: Option<ProfilingMetadata>,               // Optional profiling data
}
```

### Example Result

```json
{
  "query_id": "sensor-monitor",
  "timestamp": "2025-01-15T10:30:45Z",
  "results": [
    {"sensor_id": "temp-001", "temperature": 72.5, "status": "normal"},
    {"sensor_id": "temp-002", "temperature": 68.3, "status": "normal"}
  ],
  "metadata": {"operation": "update", "source": "sensor-data"}
}
```

### Accessing Data

```rust
handle.subscribe(|result| {
    println!("Query: {}", result.query_id);

    // Access metadata
    if let Some(op) = result.metadata.get("operation") {
        println!("Operation: {}", op);
    }

    // Process results
    for value in &result.results {
        if let Some(sensor_id) = value.get("sensor_id").and_then(|v| v.as_str()) {
            println!("Sensor: {}", sensor_id);
        }
    }
}).await?;
```

## API Reference

### ApplicationReactionHandle

```rust
impl ApplicationReactionHandle {
    pub async fn subscribe<F>(&self, callback: F) -> Result<()>;
    pub async fn subscribe_filtered<F>(&self, filter: Vec<String>, callback: F) -> Result<()>;
    pub async fn as_stream(&self) -> Option<ResultStream>;
    pub async fn subscribe_with_options(&self, options: SubscriptionOptions) -> Result<Subscription>;
    pub async fn take_receiver(&self) -> Option<mpsc::Receiver<QueryResult>>;
    pub fn reaction_id(&self) -> &str;
}
```

### SubscriptionOptions

```rust
SubscriptionOptions::default()
    .with_buffer_size(usize)          // Buffer capacity (default: 1000)
    .with_query_filter(Vec<String>)   // Filter by query IDs (default: all)
    .with_timeout(Duration)           // Receive timeout (default: infinite)
    .with_batch_size(usize)           // Batch size (default: 10)
```

### Subscription

```rust
impl Subscription {
    pub async fn recv(&mut self) -> Option<QueryResult>;
    pub fn try_recv(&mut self) -> Option<QueryResult>;
    pub async fn recv_batch(&mut self) -> Vec<QueryResult>;
    pub fn into_stream(self) -> SubscriptionStream;
}
```

### Channel Behavior

- **Buffer**: 1000 messages by default
- **Backpressure**: Producer waits when buffer is full
- **Single Consumer**: Receiver can only be taken once
- **Async**: Requires Tokio runtime

## Troubleshooting

### Common Issues

**Receiver Already Taken**
- Symptom: Methods return `None` or error
- Cause: Receiver can only be consumed once
- Solution: Use one consumption method per handle; create multiple reactions for multiple consumers

```rust
// Wrong - second call fails
handle.subscribe(|r| {}).await?;
handle.as_stream().await; // Returns None!

// Right - use one method
handle.subscribe(|r| {}).await?;
```

**Channel Buffer Full**
- Symptom: Results delayed or reaction stops
- Cause: Consumer not processing fast enough
- Solution: Increase buffer size or use batching

```rust
let options = SubscriptionOptions::default().with_buffer_size(10000);
let mut subscription = handle.subscribe_with_options(options).await?;
```

**Missing Query Results**
- Symptom: Not receiving expected results
- Cause: Query filter excludes the query
- Solution: Check filter configuration

```rust
// Receive all queries
Reaction::application("all").build()

// Receive specific queries
Reaction::application("filtered").subscribe_to("query-1").build()
```

**Send Trait Errors**
- Symptom: Compilation errors about `Send`
- Cause: Callback captures non-Send types
- Solution: Use `Arc` for shared state

```rust
let state = Arc::new(Mutex::new(Vec::new()));
let state_clone = state.clone();

handle.subscribe(move |result| {
    // Use Arc/Mutex for shared state
    let state = state_clone.clone();
    tokio::spawn(async move {
        state.lock().await.push(result);
    });
}).await?;
```

## Limitations

### Architecture Constraints

- **Single Receiver**: Channel supports one consumer only. Create multiple reactions for multiple consumers.
- **Buffer Size**: Default 1000 messages. Adjust with `SubscriptionOptions` based on throughput.
- **No Persistence**: In-memory only. Results lost on crash. Implement persistence in callbacks if needed.
- **Sequential Processing**: Results processed sequentially. Spawn tasks for parallelism:

```rust
handle.subscribe(|result| {
    tokio::spawn(async move {
        expensive_processing(result).await;
    });
}).await?;
```

### Runtime Requirements

- **Tokio Only**: Requires Tokio async runtime
- **No Retry**: Failed callbacks lose results. Implement retry logic if needed
- **Thread Safety**: Callbacks must be `Send + 'static`

### Performance Notes

- Buffer large enough to handle bursts but not waste memory
- Spawn tasks for heavy processing to avoid blocking
- High-volume queries consume significant memory
- Query filtering at reaction level, not subscription level
