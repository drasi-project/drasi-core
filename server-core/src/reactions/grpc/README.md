# gRPC Reaction

The gRPC reaction sends query results to external gRPC services using the Protocol Buffers format. It provides reliable, efficient streaming of continuous query results to remote services with automatic connection management, batching, and retry logic.

## Purpose

The gRPC reaction enables integration with external systems that expose gRPC endpoints for receiving query result changes. It:

- Sends query results to external gRPC services using the `drasi.v1.ReactionService` interface
- Provides efficient binary serialization via Protocol Buffers
- Implements automatic connection management with lazy initialization
- Batches results for optimal network efficiency
- Handles connection failures with exponential backoff and retry logic
- Supports metadata injection for authentication and routing

### When to Use This Reaction

Use the gRPC reaction when:
- You need to integrate with microservices that expose gRPC endpoints
- You require efficient binary serialization for high-throughput scenarios
- You want strongly-typed data contracts via Protocol Buffers
- You need reliable delivery with automatic retry mechanisms
- You're building distributed systems where low latency and efficiency are critical

## Configuration Properties

All configuration properties are specified in the `properties` section of the reaction configuration.

| Property | Type | Default | Required | Description |
|----------|------|---------|----------|-------------|
| `endpoint` | string | `"grpc://localhost:50052"` | No | gRPC server endpoint URL. Use `grpc://` prefix (converted to `http://` internally) |
| `batch_size` | number | `100` | No | Maximum number of result items to batch before sending |
| `batch_flush_timeout_ms` | number | `1000` | No | Maximum time (milliseconds) to wait before flushing partial batches |
| `timeout_ms` | number | `5000` | No | Request timeout in milliseconds for individual RPC calls |
| `max_retries` | number | `3` | No | Maximum retry attempts for failed RPC requests |
| `connection_retry_attempts` | number | `5` | No | Maximum attempts to establish initial connection |
| `initial_connection_timeout_ms` | number | `10000` | No | Timeout for initial connection establishment |
| `metadata` | object | `{}` | No | Key-value pairs to send as gRPC metadata headers with each request |

### Property Details

#### endpoint
The gRPC server endpoint to send results to. Specify using the `grpc://` scheme which is internally converted to HTTP/2. For TLS connections, use standard gRPC endpoint configuration.

**Examples:**
- `"grpc://localhost:50052"` - Local development
- `"grpc://reaction-service:9090"` - Kubernetes service
- `"grpc://api.example.com:443"` - Remote service

#### batch_size
Controls how many query result items are accumulated before sending to the gRPC service. Larger batch sizes improve network efficiency but increase latency. Smaller batches reduce latency but increase network overhead.

#### batch_flush_timeout_ms
Ensures partial batches are sent even when `batch_size` is not reached. This prevents data from being held indefinitely in low-throughput scenarios. The timer resets each time the batch is modified.

#### max_retries
Number of retry attempts for transient failures. Non-connection errors (e.g., invalid data) fail immediately. Connection errors trigger connection recreation.

#### connection_retry_attempts
Only applies during initial connection establishment when the reaction starts or when creating a fresh connection after GoAway frames. Does not apply to request-level retries.

#### metadata
Arbitrary key-value pairs sent as gRPC metadata headers with every `ProcessResults` request. Useful for authentication tokens, request IDs, tenant identifiers, or routing hints.

**Example:**
```yaml
metadata:
  authorization: "Bearer eyJhbGc..."
  x-tenant-id: "tenant-123"
  x-request-source: "drasi-core"
```

## Configuration Examples

### YAML Configuration

#### Basic Configuration
```yaml
reactions:
  - id: "grpc-reaction"
    reaction_type: "grpc"
    queries:
      - "high-temperature-alerts"
    auto_start: true
    properties:
      endpoint: "grpc://localhost:50052"
```

#### Production Configuration with Batching and Retries
```yaml
reactions:
  - id: "sensor-grpc-reaction"
    reaction_type: "grpc"
    queries:
      - "temperature-query"
      - "humidity-query"
    auto_start: true
    properties:
      # Connection settings
      endpoint: "grpc://reaction-service.production.svc.cluster.local:9090"
      connection_retry_attempts: 10
      initial_connection_timeout_ms: 30000

      # Batching settings for high throughput
      batch_size: 500
      batch_flush_timeout_ms: 2000

      # Request settings
      timeout_ms: 10000
      max_retries: 5

      # Authentication and routing
      metadata:
        authorization: "Bearer ${API_TOKEN}"
        x-tenant-id: "production-tenant"
        x-service-version: "v2"
```

#### Low-Latency Configuration
```yaml
reactions:
  - id: "realtime-alerts"
    reaction_type: "grpc"
    queries:
      - "critical-events"
    auto_start: true
    properties:
      endpoint: "grpc://alert-service:50052"

      # Small batches for low latency
      batch_size: 10
      batch_flush_timeout_ms: 100

      # Fast timeouts for quick failure detection
      timeout_ms: 2000
      max_retries: 2
```

### JSON Configuration

#### Basic Configuration
```json
{
  "reactions": [
    {
      "id": "grpc-reaction",
      "reaction_type": "grpc",
      "queries": ["high-temperature-alerts"],
      "auto_start": true,
      "properties": {
        "endpoint": "grpc://localhost:50052"
      }
    }
  ]
}
```

#### Production Configuration
```json
{
  "reactions": [
    {
      "id": "sensor-grpc-reaction",
      "reaction_type": "grpc",
      "queries": ["temperature-query", "humidity-query"],
      "auto_start": true,
      "properties": {
        "endpoint": "grpc://reaction-service.production.svc.cluster.local:9090",
        "connection_retry_attempts": 10,
        "initial_connection_timeout_ms": 30000,
        "batch_size": 500,
        "batch_flush_timeout_ms": 2000,
        "timeout_ms": 10000,
        "max_retries": 5,
        "metadata": {
          "authorization": "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
          "x-tenant-id": "production-tenant",
          "x-service-version": "v2"
        }
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Basic Usage

```rust
use drasi_server_core::api::{Reaction, Properties};

// Create a basic gRPC reaction
let reaction = Reaction::grpc("my-grpc-reaction")
    .subscribe_to("temperature-query")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "grpc://localhost:50052")
    )
    .build();
```

### Advanced Configuration

```rust
use drasi_server_core::api::{Reaction, Properties};
use serde_json::json;

// Create a production gRPC reaction with full configuration
let reaction = Reaction::grpc("production-reaction")
    .subscribe_to("sensor-query")
    .subscribe_to("alert-query")
    .with_properties(
        Properties::new()
            // Connection settings
            .with_string("endpoint", "grpc://reaction-service:9090")
            .with_number("connection_retry_attempts", 10)
            .with_number("initial_connection_timeout_ms", 30000)

            // Batching settings
            .with_number("batch_size", 500)
            .with_number("batch_flush_timeout_ms", 2000)

            // Request settings
            .with_number("timeout_ms", 10000)
            .with_number("max_retries", 5)

            // Metadata for authentication
            .with("metadata", json!({
                "authorization": "Bearer eyJhbGc...",
                "x-tenant-id": "prod-123",
                "x-service-version": "v2"
            }))
    )
    .build();
```

### Starting the Reaction with DrasiServerCore

```rust
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration from file
    let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    let runtime_config = Arc::new(config.into());

    // Create and initialize server core
    let mut core = DrasiServerCore::new(runtime_config);
    core.initialize().await?;

    // Start all components (sources, queries, reactions)
    core.start().await?;

    // The gRPC reaction will now send results to the configured endpoint
    // Keep the application running
    tokio::signal::ctrl_c().await?;

    // Graceful shutdown
    core.stop().await?;

    Ok(())
}
```

### Error Handling Example

```rust
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig};
use log::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    let runtime_config = Arc::new(config.into());

    let mut core = DrasiServerCore::new(runtime_config);

    // Initialize with error handling
    if let Err(e) = core.initialize().await {
        error!("Failed to initialize DrasiServerCore: {}", e);
        return Err(e);
    }

    // Start with error handling
    match core.start().await {
        Ok(_) => {
            info!("DrasiServerCore started successfully");
            info!("gRPC reactions are now sending data");
        }
        Err(e) => {
            error!("Failed to start DrasiServerCore: {}", e);
            return Err(e);
        }
    }

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Received shutdown signal");

    // Graceful shutdown - flushes final batches
    core.stop().await?;
    info!("DrasiServerCore stopped cleanly");

    Ok(())
}
```

## Input Data Format

The gRPC reaction receives data from queries via the internal `QueryResult` structure:

```rust
pub struct QueryResult {
    pub query_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<serde_json::Value>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub profiling: Option<ProfilingMetadata>,
}
```

### QueryResult Fields

- **query_id**: Unique identifier of the query that produced these results
- **timestamp**: When the results were generated
- **results**: Array of result items, each containing change data
- **metadata**: Optional metadata about the query execution
- **profiling**: Optional performance tracking data

### Result Item Structure

Each item in the `results` array contains:

```json
{
  "type": "ADD" | "UPDATE" | "DELETE",
  "data": {
    // The actual result data for ADD/DELETE
  },
  "before": {
    // Previous values (UPDATE only)
  },
  "after": {
    // New values (UPDATE only)
  }
}
```

**Examples:**

**ADD Operation:**
```json
{
  "type": "ADD",
  "data": {
    "id": "sensor-1",
    "temperature": 28.5,
    "location": "room-101"
  }
}
```

**UPDATE Operation:**
```json
{
  "type": "UPDATE",
  "before": {
    "id": "sensor-1",
    "temperature": 28.5
  },
  "after": {
    "id": "sensor-1",
    "temperature": 32.1
  }
}
```

**DELETE Operation:**
```json
{
  "type": "DELETE",
  "data": {
    "id": "sensor-1",
    "temperature": 32.1
  }
}
```

## Output Data Format

The gRPC reaction sends data using the Protocol Buffers format defined in `proto/drasi/v1/reaction.proto` and `proto/drasi/v1/common.proto`.

### ProcessResultsRequest Structure

```protobuf
message ProcessResultsRequest {
    QueryResult results = 1;
    map<string, string> metadata = 2; // Optional metadata from config
}
```

### QueryResult Protobuf Message

```protobuf
message QueryResult {
    string query_id = 1;
    repeated QueryResultItem results = 2;
    google.protobuf.Timestamp timestamp = 3;
}
```

### QueryResultItem Structure

```protobuf
message QueryResultItem {
    string type = 1;                    // "ADD", "UPDATE", "DELETE"
    google.protobuf.Struct data = 2;    // Main data payload
    google.protobuf.Struct before = 3;  // For UPDATE - previous values
    google.protobuf.Struct after = 4;   // For UPDATE - new values
}
```

### Field Descriptions

- **type**: Operation type as string - one of "ADD", "UPDATE", or "DELETE"
- **data**: The result data as a Protocol Buffers Struct (equivalent to JSON object)
- **before**: For UPDATE operations, contains the previous state
- **after**: For UPDATE operations, contains the new state

### Example Protobuf Representation

**ADD Operation in Protobuf:**
```
QueryResultItem {
  type: "ADD"
  data: Struct {
    fields: {
      "id": Value { string_value: "sensor-1" }
      "temperature": Value { number_value: 28.5 }
      "location": Value { string_value: "room-101" }
    }
  }
}
```

**UPDATE Operation in Protobuf:**
```
QueryResultItem {
  type: "UPDATE"
  before: Struct {
    fields: {
      "id": Value { string_value: "sensor-1" }
      "temperature": Value { number_value: 28.5 }
    }
  }
  after: Struct {
    fields: {
      "id": Value { string_value: "sensor-1" }
      "temperature": Value { number_value: 32.1 }
    }
  }
}
```

**Complete Request Example:**
```
ProcessResultsRequest {
  results: QueryResult {
    query_id: "temperature-alerts"
    timestamp: Timestamp { seconds: 1706745600, nanos: 0 }
    results: [
      QueryResultItem {
        type: "ADD"
        data: Struct { ... }
      },
      QueryResultItem {
        type: "UPDATE"
        before: Struct { ... }
        after: Struct { ... }
      }
    ]
  }
  metadata: {
    "authorization": "Bearer eyJhbGc..."
    "x-tenant-id": "prod-123"
  }
}
```

### ProcessResultsResponse

The gRPC service must respond with:

```protobuf
message ProcessResultsResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    uint32 items_processed = 4;
}
```

- **success**: `true` if processing succeeded, `false` otherwise
- **message**: Optional human-readable status message
- **error**: Error description if success is `false`
- **items_processed**: Number of items successfully processed

## Connection Behavior

The gRPC reaction implements sophisticated connection management with lazy initialization and automatic recovery.

### Lazy Connection Initialization

The reaction does **not** establish a network connection when it starts. Instead:

1. Connection is created only when the first batch of data arrives
2. Initial connection uses `connect_lazy()` which defers socket establishment until the first RPC call
3. This prevents unnecessary connection overhead when queries produce no results

**Connection States:**

```rust
pub enum ConnectionState {
    Disconnected,   // No connection established yet
    Connecting,     // Attempting to establish connection
    Connected,      // Connection active and healthy
    Failed,         // Connection attempt failed
    Reconnecting,   // Attempting to recover connection
}
```

### Connection Lifecycle

1. **Initial State**: `Disconnected`
   - No connection exists
   - Waiting for first data to arrive

2. **First Data Arrives**: `Disconnected` → `Connecting`
   - Creates lazy gRPC channel
   - Attempts initial connection with retry logic
   - Uses `connection_retry_attempts` and `initial_connection_timeout_ms` settings

3. **Connection Success**: `Connecting` → `Connected`
   - Resets failure counter
   - Begins processing batches

4. **Connection Failure**: `Connecting` → `Failed`
   - Increments failure counter
   - Implements exponential backoff (500ms base, max 30s)
   - Keeps batch data for retry

5. **Automatic Reconnection**: `Failed` → `Reconnecting` → `Connected`
   - Triggered on next batch arrival
   - Waits for backoff period before retry
   - Creates fresh connection

### Retry Logic and Exponential Backoff

The reaction implements two levels of retry logic:

#### Request-Level Retries

For individual RPC calls, controlled by `max_retries`:

```
Retry Attempt  |  Backoff Duration  |  Total Wait Time
---------------|--------------------|-----------------
1              |  100ms + jitter    |  100ms
2              |  200ms + jitter    |  300ms
3              |  400ms + jitter    |  700ms
4              |  800ms + jitter    |  1.5s
5              |  1600ms + jitter   |  3.1s
...            |  ...               |  ...
Max            |  5000ms            |  (capped)
```

- Base backoff: 100ms
- Multiplier: 2x
- Max backoff: 5 seconds
- Jitter: 0-100ms random addition
- Max retry duration: 60 seconds total

#### Connection-Level Retries

For connection establishment failures, controlled by `connection_retry_attempts`:

```
Failure Count  |  Backoff Duration  |  Behavior
---------------|--------------------|--------------------------
0              |  None              |  Immediate connection
1              |  500ms             |  First retry after delay
2              |  1000ms            |  Exponential backoff
3              |  2000ms            |  Continues doubling
4              |  4000ms            |  ...
5+             |  8000ms            |  ...
Max            |  30000ms           |  Capped at 30 seconds
```

### GoAway Frame Handling

HTTP/2 servers can send GoAway frames to gracefully close connections. The reaction handles this automatically:

#### Normal GoAway (Connection Refresh)
```
1. Server sends GoAway frame (graceful shutdown)
2. Reaction detects GoAway error
3. Logs: "GoAway frame received - server closed connection gracefully"
4. Creates fresh connection immediately
5. Retries request with new connection
6. Increments GoAway counter for metrics
```

#### Immediate GoAway (StreamId(0))
```
1. Server sends GoAway with StreamId(0) (immediate rejection)
2. Reaction detects immediate rejection
3. Logs: "Server immediately rejected connection with GoAway(StreamId(0))"
4. Waits 2 seconds before retry (prevents tight loop)
5. Creates fresh connection
6. Retries request
```

**GoAway indicates:**
- Server is shutting down gracefully
- Connection should be refreshed
- Request should be retried on new connection
- This is **normal behavior**, not an error

### Connection Error Handling

Different error types are handled differently:

| Error Type | Behavior | Creates New Connection |
|------------|----------|------------------------|
| **GoAway** | Immediate reconnection | Yes |
| **Unavailable** | Service temporarily down, retry | Yes |
| **DeadlineExceeded** | Request timeout, retry | No |
| **ResourceExhausted** | Server overloaded, wait 5s | No |
| **Connection/Transport** | Network issue, reconnect | Yes |
| **BrokenPipe/ChannelClosed** | Connection dead, recreate | Yes |
| **Application Error** | Data/logic error, fail | No |

### Connection Metrics

The reaction tracks connection health metrics:

```rust
// Logged periodically (every 100 successful sends)
total_connection_attempts: u32  // Total times connection was attempted
successful_sends: u32            // Number of successful batch sends
failed_sends: u32                // Number of failed batch sends
goaway_count: u32                // Number of GoAway frames received
consecutive_failures: u32        // Current failure streak
```

**Example log output:**
```
Metrics - State: Connected, Successful: 500, Failed: 3, Connections: 8, GoAways: 2
```

## Batching Behavior

The gRPC reaction batches query results for optimal network efficiency while balancing latency.

### Batching Logic

Results are accumulated and sent when:

1. **Batch size reached**: Number of items equals `batch_size`
2. **Flush timeout elapsed**: `batch_flush_timeout_ms` milliseconds since last batch modification
3. **Query ID changes**: Different query's results arrive (previous batch is flushed)
4. **Shutdown**: Final batch is flushed when reaction stops

### Batch Size vs Timeout Interaction

```
Time  →
0ms:  Item arrives, batch = [item1], timer starts
500ms: Item arrives, batch = [item1, item2]
1000ms: Flush timeout! → Send batch (2 items)
1100ms: Item arrives, batch = [item1], timer restarts
1200ms: 98 more items arrive, batch reaches 100
      → Send batch immediately (batch_size reached)
```

### Optimal Batch Configuration

**High-Throughput Scenarios:**
- `batch_size`: 500-1000
- `batch_flush_timeout_ms`: 2000-5000
- Prioritizes efficiency over latency
- Reduces network overhead

**Low-Latency Scenarios:**
- `batch_size`: 10-50
- `batch_flush_timeout_ms`: 100-500
- Prioritizes latency over efficiency
- More frequent but smaller batches

**Balanced Configuration (Default):**
- `batch_size`: 100
- `batch_flush_timeout_ms`: 1000
- Good balance for most use cases

### Final Batch Flushing on Shutdown

When the reaction stops (due to shutdown or error):

1. Main processing loop exits
2. Remaining batch is checked
3. If batch is non-empty:
   - Creates connection if needed
   - Sends final batch with retry logic
   - Logs success/failure
4. Reaction transitions to `Stopped` state

**Important:** Final batch flushing ensures no data loss during graceful shutdown. If connection cannot be established, data in the final batch will be lost (logged as warning).

## Troubleshooting

### Connection Errors

#### GoAway Frames

**Symptom:**
```
GoAway frame received - server closed connection gracefully
Creating fresh connection after GoAway...
```

**Cause:** Server is shutting down connections (normal HTTP/2 behavior)

**Solution:** This is handled automatically. The reaction creates a new connection and retries.

**Prevention:**
- Ensure gRPC server has sufficient connection limits
- Monitor server logs for shutdown events
- Consider implementing connection pooling on server side

#### Immediate GoAway (StreamId(0))

**Symptom:**
```
Server immediately rejected connection with GoAway(StreamId(0))
This indicates the server rejected the connection before any streams were created
```

**Cause:**
- Server not ready to accept connections
- HTTP/2 settings incompatibility
- Protocol version mismatch
- Server under heavy load

**Solution:**
1. Check server is fully started and ready
2. Verify HTTP/2 compatibility
3. Review server logs for initialization errors
4. Increase `initial_connection_timeout_ms`
5. Increase `connection_retry_attempts`

#### Unavailable Service

**Symptom:**
```
Error categorized as 'Unavailable' - is_connection_error: true
Connection error detected - type: Unavailable
```

**Cause:**
- Service is down or not responding
- Network partition
- DNS resolution failure
- Service not yet started

**Solution:**
1. Verify service is running: `kubectl get pods` or `docker ps`
2. Check endpoint configuration: `endpoint: "grpc://service-name:port"`
3. Test connectivity: `grpcurl -plaintext service-name:port list`
4. Review network policies and firewall rules
5. Check DNS resolution

**Configuration adjustments:**
```yaml
properties:
  connection_retry_attempts: 10  # Increase retries
  initial_connection_timeout_ms: 30000  # 30 second timeout
```

### Timeout Issues

#### DeadlineExceeded

**Symptom:**
```
Request deadline exceeded - consider increasing timeout
Error categorized as 'DeadlineExceeded'
```

**Cause:**
- `timeout_ms` too low for request processing
- Server processing is slow
- Large batch sizes taking too long
- Network latency

**Solution:**
1. Increase `timeout_ms`:
```yaml
properties:
  timeout_ms: 15000  # 15 seconds instead of default 5s
```

2. Reduce batch size to decrease processing time:
```yaml
properties:
  batch_size: 50  # Smaller batches process faster
```

3. Investigate server-side performance
4. Monitor network latency

#### Connection Timeout

**Symptom:**
```
Failed to create client after 5 retries
Waiting before retrying connection
```

**Cause:**
- `initial_connection_timeout_ms` too low
- Server slow to start accepting connections
- Network connectivity issues

**Solution:**
```yaml
properties:
  initial_connection_timeout_ms: 30000  # 30 seconds
  connection_retry_attempts: 10         # More attempts
```

### Retry Exhaustion

**Symptom:**
```
Max retries exceeded - giving up. Error: ...
gRPC call failed after 3 retries: ...
```

**Cause:**
- Persistent server errors
- Invalid data causing repeated failures
- Network instability

**Solution:**
1. Check server logs for error details
2. Verify data format matches server expectations
3. Increase `max_retries`:
```yaml
properties:
  max_retries: 10  # More retry attempts
```

4. Investigate network stability
5. Add circuit breaker pattern on server side

### Metadata Configuration Issues

#### Authentication Failures

**Symptom:**
```
gRPC call failed (type: application): status: 16, message: "unauthenticated"
```

**Cause:**
- Missing or invalid authorization token in metadata
- Incorrect metadata key names
- Token expired

**Solution:**
1. Verify metadata configuration:
```yaml
properties:
  metadata:
    authorization: "Bearer valid-token-here"
```

2. Check server expects authorization in metadata
3. Ensure token is valid and not expired
4. Review server authentication requirements

#### Metadata Format Issues

**Symptom:**
```
Failed to send batch: invalid metadata format
```

**Cause:**
- Metadata values must be strings in configuration
- Non-string values in metadata object

**Solution:**
```yaml
# Correct - all values are strings
metadata:
  tenant-id: "123"
  request-id: "req-456"

# Incorrect - non-string values
metadata:
  tenant-id: 123  # Should be "123"
```

### TLS/mTLS Configuration

**Note:** The current implementation uses `http://` endpoints (HTTP/2 without TLS). For TLS support:

1. Use gRPC with TLS-enabled endpoints
2. Configure certificates if using mTLS
3. Verify server TLS configuration

**Example TLS endpoint:**
```yaml
properties:
  endpoint: "grpc://secure-service.example.com:443"
  # Additional TLS configuration may be needed
```

### Performance Issues

#### High Latency

**Symptom:** Results arrive slowly at the gRPC service

**Diagnosis:**
1. Check batch configuration:
```yaml
# Current (high latency)
batch_size: 1000
batch_flush_timeout_ms: 10000

# Optimized (low latency)
batch_size: 20
batch_flush_timeout_ms: 200
```

2. Monitor batch send logs:
```
Successfully sent batch - size: 1000, time: 2.5s
```

**Solution:**
- Reduce `batch_size` for faster sends
- Reduce `batch_flush_timeout_ms` for quicker flush
- Increase server processing capacity

#### Data Loss During Shutdown

**Symptom:**
```
WARNING: 47 items in final batch will be lost!
Failed to create client for final batch: connection refused
```

**Cause:**
- Server shut down before client
- Network partition during shutdown
- Connection cannot be established for final batch

**Solution:**
1. Ensure proper shutdown ordering (stop reactions last)
2. Increase final batch retry attempts
3. Implement persistent queue for critical data
4. Monitor shutdown sequences

### Debugging Tips

#### Enable Debug Logging

```rust
// Set environment variable
RUST_LOG=drasi_server_core::reactions::grpc=debug

// Or in code
env_logger::Builder::from_env(Env::default().default_filter_or("debug")).init();
```

#### Monitor Connection State

Look for state transition logs:
```
State transition: Disconnected -> Connecting (attempt 1, total: 1)
State transition: Connecting -> Connected (after 0 failures, total attempts: 1)
State transition: Connected -> Reconnecting
```

#### Check Metrics

Every 100 successful sends:
```
Metrics - State: Connected, Successful: 500, Failed: 3, Connections: 8, GoAways: 2
```

High `goaway_count`: Server frequently restarts connections
High `failed_sends`: Review error logs for patterns
High `Connections`: Frequent reconnection issues

#### Test gRPC Service Independently

```bash
# Test service availability
grpcurl -plaintext localhost:50052 list

# Test ProcessResults method
grpcurl -plaintext -d '{...}' localhost:50052 drasi.v1.ReactionService/ProcessResults
```

## Limitations

### Protobuf Message Size Limits

- **Default gRPC limit:** 4MB per message
- **Large batches** with complex data can exceed this limit
- **Impact:** Request fails with "message too large" error

**Mitigation:**
- Reduce `batch_size` for large result objects
- Configure server with higher message size limits
- Monitor message sizes in production

### Connection Pooling

- **Current implementation:** Single connection per reaction
- **Impact:** Limited parallelism for high-throughput scenarios
- **Considerations:**
  - One reaction instance = one TCP connection
  - Multiple reactions to same endpoint = multiple connections
  - HTTP/2 multiplexing handles multiple streams over single connection

### Performance Considerations

#### Memory Usage
- Batches held in memory until sent
- Large `batch_size` × complex data = high memory usage
- Failed batches retained for retry

**Recommendation:**
- Monitor memory usage with large batches
- Balance `batch_size` vs memory constraints

#### Network Bandwidth
- Protocol Buffers are efficient but not compressed
- High-frequency updates can saturate network
- Consider batch size vs network capacity

#### CPU Usage
- JSON to Protobuf conversion for each result
- Serialization overhead for large batches
- Minimal impact for typical workloads

### Known Behaviors

#### Eventual Consistency
- Connection failures may delay delivery
- Retries ensure eventual delivery (not immediate)
- No guarantee of strict ordering across batches

#### Data Loss Scenarios
- Final batch lost if shutdown connection fails
- No persistent queue for retry
- Unrecoverable errors discard batch

**Mitigation:**
- Implement idempotent receivers
- Use transaction logs for critical data
- Monitor failed batch logs

#### Concurrent Query Results
- Results from different queries are batched separately
- Query ID change triggers immediate batch flush
- May result in smaller batches than configured

### Version Compatibility

- **Protocol:** Uses `drasi.v1` protobuf package
- **Compatibility:** Requires compatible gRPC service implementation
- **Breaking changes:** Protocol updates may require service updates

### TLS/Security Limitations

- Current implementation uses `http://` scheme
- TLS configuration not explicitly documented
- mTLS support depends on tonic/gRPC configuration

**For production:**
- Verify TLS requirements with your security team
- Implement proper certificate management
- Consider mutual TLS for sensitive data

### No Built-in Observability

The reaction logs extensively but does not expose:
- Prometheus metrics
- Distributed tracing (OpenTelemetry)
- Health check endpoints

**Enhancement opportunities:**
- Add metrics export
- Integrate distributed tracing
- Expose health/readiness probes
