# gRPC Reaction

Sends query results to external gRPC services using Protocol Buffers format with automatic connection management, batching, and retry logic.

## Quick Start

### YAML Configuration
```yaml
reactions:
  - id: "my-grpc-reaction"
    queries: ["my-query"]
    reaction_type: "grpc"
    auto_start: true
    endpoint: "grpc://localhost:50052"
    batch_size: 100
    batch_flush_timeout_ms: 1000
    timeout_ms: 5000
    max_retries: 3
    metadata:
      authorization: "Bearer ${TOKEN}"
```

### Rust API
```rust
use drasi_server_core::{DrasiLib, DrasiLibConfig};

let config = DrasiLibConfig::load_from_file("config.yaml")?;
let mut core = DrasiLib::new(Arc::new(config.into()));
core.initialize().await?;
core.start().await?;
```

## Configuration

### Configuration Settings

The gRPC Reaction supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the reaction | Any string | **(Required)** |
| `queries` | Array[String] | IDs of queries this reaction subscribes to | Array of query IDs | **(Required)** |
| `reaction_type` | String | Reaction type discriminator | "grpc" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this reaction | true, false | `true` |
| `endpoint` | String | gRPC server endpoint URL. Must start with `grpc://` or `grpcs://` for secure connections | Valid gRPC URL (e.g., `grpc://localhost:50052`) | `"grpc://localhost:50052"` |
| `timeout_ms` | u64 | Request timeout in milliseconds. Applies to individual gRPC calls | Any positive integer | `5000` |
| `batch_size` | usize | Maximum number of items to include in a single batch. Results are batched together when multiple results for the same query are available | Any positive integer | `100` |
| `batch_flush_timeout_ms` | u64 | Maximum time to wait before flushing a partial batch (milliseconds). Ensures results are sent even if batch size is not reached | Any positive integer | `1000` |
| `max_retries` | u32 | Maximum number of retry attempts for failed batch send requests. Uses exponential backoff between retries | 0-100 | `3` |
| `connection_retry_attempts` | u32 | Number of connection retry attempts when establishing initial connection to gRPC server | 0-100 | `5` |
| `initial_connection_timeout_ms` | u64 | Timeout in milliseconds for the initial connection attempt to the gRPC server | Any positive integer | `10000` |
| `metadata` | HashMap<String, String> | gRPC metadata headers to include in all requests. Key-value pairs are converted to gRPC metadata | Map of header names to values | Empty HashMap |
| `priority_queue_capacity` | Integer (Optional) | Maximum events in priority queue before backpressure. Controls event queuing before the reaction processes them. Higher values allow more buffering but use more memory | Any positive integer | `10000` |

## Data Format

### Input (Query Result)
```json
{
  "type": "ADD" | "UPDATE" | "DELETE",
  "data": { ... },           // For ADD/DELETE
  "before": { ... },         // For UPDATE
  "after": { ... }           // For UPDATE
}
```

### Output (Protocol Buffers)
```protobuf
message ProcessResultsRequest {
    QueryResult results = 1;
    map<string, string> metadata = 2;
}

message QueryResult {
    string query_id = 1;
    repeated QueryResultItem results = 2;
    google.protobuf.Timestamp timestamp = 3;
}

message QueryResultItem {
    string type = 1;
    google.protobuf.Struct data = 2;
    google.protobuf.Struct before = 3;
    google.protobuf.Struct after = 4;
}
```

## Features

### Lazy Connection
Connection is established only when first data arrives, not at startup.

### Automatic Batching
Results are batched and sent when:
- Batch size is reached
- Flush timeout expires
- Query ID changes
- Reaction stops

### Connection Recovery
Automatically handles:
- HTTP/2 GoAway frames (graceful reconnection)
- Network failures (exponential backoff)
- Service unavailability (retry with backoff)
- Resource exhaustion (aggressive backoff)

### Backoff Strategy

**Request-level retries:**
- Base: 100ms, Multiplier: 2x, Max: 5s
- Random jitter: 0-100ms
- Max total duration: 60s

**Connection-level retries:**
- Base: 500ms, Multiplier: 2x, Max: 30s
- Triggered on connection failures

## Configuration Examples

### High-Throughput
```yaml
reactions:
  - id: "high-throughput-grpc"
    reaction_type: "grpc"
    batch_size: 500
    batch_flush_timeout_ms: 2000
    timeout_ms: 10000
    max_retries: 5
```

### Low-Latency
```yaml
reactions:
  - id: "low-latency-grpc"
    reaction_type: "grpc"
    batch_size: 10
    batch_flush_timeout_ms: 100
    timeout_ms: 2000
    max_retries: 2
```

### Production with Auth
```yaml
reactions:
  - id: "production-grpc"
    queries: ["production-query"]
    reaction_type: "grpc"
    priority_queue_capacity: 25000
    endpoint: "grpc://api.example.com:443"
    connection_retry_attempts: 10
    initial_connection_timeout_ms: 30000
    metadata:
      authorization: "Bearer ${API_TOKEN}"
      x-tenant-id: "prod-123"
```

## Troubleshooting

### GoAway Errors
**Normal behavior** - server is refreshing connections. Handled automatically with immediate reconnection.

### Unavailable Service
```yaml
# Increase retries and timeout
reactions:
  - id: "my-grpc-reaction"
    reaction_type: "grpc"
    connection_retry_attempts: 10
    initial_connection_timeout_ms: 30000
```

### DeadlineExceeded
```yaml
# Increase timeout and reduce batch size
reactions:
  - id: "my-grpc-reaction"
    reaction_type: "grpc"
    timeout_ms: 15000
    batch_size: 50
```

### Authentication Failures
Verify metadata configuration:
```yaml
reactions:
  - id: "my-grpc-reaction"
    reaction_type: "grpc"
    metadata:
      authorization: "Bearer valid-token"  # Must be string
```

## Module Structure

The gRPC reaction is organized into focused modules:

- **`proto.rs`** - Protocol buffer definitions and generated code
- **`helpers.rs`** - JSON to protobuf conversion utilities (used by grpc_adaptive)
- **`connection.rs`** - Connection management and retry logic
- **`mod.rs`** - Main reaction implementation

## Limitations

- **Message size**: 4MB default gRPC limit (configure server for larger messages)
- **No connection pooling**: One connection per reaction instance
- **No persistence**: Failed batches during shutdown may be lost
- **No built-in metrics**: Extensive logging but no Prometheus/OpenTelemetry
- **Batch flushing**: Batches are flushed when query_id changes or batch size is reached. The `batch_flush_timeout_ms` parameter is currently not enforced (batches are not flushed based on time alone)

## Performance Considerations

- **Memory**: Batches held in memory until sent
- **Network**: Protocol Buffers efficient but not compressed
- **CPU**: Minimal overhead for typical workloads

## Debugging

Enable debug logging:
```bash
RUST_LOG=drasi_server_core::reactions::grpc=debug cargo run
```

Monitor connection state transitions and metrics in logs every 100 successful sends.
