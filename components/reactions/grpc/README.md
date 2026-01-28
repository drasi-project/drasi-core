# gRPC Reaction

A Drasi reaction plugin that sends continuous query results to external gRPC services. This component enables real-time integration with gRPC-based systems by streaming query change events as they occur.

## Overview

The gRPC Reaction component forwards query results from Drasi continuous queries to an external gRPC endpoint. It implements the Drasi Reaction Service protocol, providing a reliable way to push change events to downstream systems.

### Key Capabilities

- **Batched Processing**: Bundles multiple query results into efficient batches before sending
- **Automatic Retry Logic**: Implements exponential backoff for transient failures
- **Connection Management**: Handles connection failures with automatic reconnection
- **Lazy Connections**: Establishes connections only when needed, reducing overhead
- **Metadata Support**: Allows custom metadata headers in gRPC requests
- **Priority Queue**: Uses priority-based queuing to ensure orderly processing
- **Multiple Query Support**: Can subscribe to multiple continuous queries simultaneously

### Use Cases

- **Event-Driven Architectures**: Forward data changes to event processing systems
- **Microservices Integration**: Connect Drasi to gRPC-based microservices
- **Real-Time Analytics**: Stream query results to analytics platforms
- **Notification Systems**: Trigger alerts or notifications based on query results
- **Data Synchronization**: Keep external systems synchronized with Drasi data

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent API for creating gRPC reactions:

```rust
use drasi_reaction_grpc::GrpcReaction;

// Minimal configuration
let reaction = GrpcReaction::builder("my-grpc-reaction")
    .with_endpoint("grpc://localhost:50052")
    .with_query("sensor-alerts")
    .build()?;

// Full configuration with all options
let reaction = GrpcReaction::builder("my-grpc-reaction")
    .with_queries(vec!["query1".to_string(), "query2".to_string()])
    .with_endpoint("grpc://api.example.com:50052")
    .with_timeout_ms(10000)
    .with_batch_size(200)
    .with_batch_flush_timeout_ms(2000)
    .with_max_retries(5)
    .with_connection_retry_attempts(10)
    .with_initial_connection_timeout_ms(15000)
    .with_metadata("api-key", "your-api-key")
    .with_metadata("tenant-id", "tenant-123")
    .with_priority_queue_capacity(1000)
    .with_auto_start(true)
    .build()?;
```

### Config Struct Approach

Alternatively, create a configuration object and pass it to the constructor:

```rust
use drasi_reaction_grpc::{GrpcReaction, GrpcReactionConfig};
use std::collections::HashMap;

let mut metadata = HashMap::new();
metadata.insert("api-key".to_string(), "your-api-key".to_string());

let config = GrpcReactionConfig {
    endpoint: "grpc://api.example.com:50052".to_string(),
    timeout_ms: 10000,
    batch_size: 200,
    batch_flush_timeout_ms: 2000,
    max_retries: 5,
    connection_retry_attempts: 10,
    initial_connection_timeout_ms: 15000,
    metadata,
};

let reaction = GrpcReaction::new(
    "my-grpc-reaction",
    vec!["query1".to_string()],
    config
);
```

### Using Default Configuration

```rust
use drasi_reaction_grpc::{GrpcReaction, GrpcReactionConfig};

// Uses all default values
let config = GrpcReactionConfig::default();
let reaction = GrpcReaction::new("my-reaction", vec!["my-query".to_string()], config);
```

### Adaptive Batching Configuration

The gRPC reaction supports **intelligent, throughput-based adaptive batching** that automatically adjusts batch sizes and wait times based on real-time traffic patterns:

```rust
use drasi_reaction_grpc::GrpcReaction;

let reaction = GrpcReaction::builder("adaptive-pipeline")
    .with_endpoint("grpc://event-processor:9090")
    .with_queries(vec!["event-stream".to_string()])
    .with_adaptive_enable(true)
    .with_min_batch_size(50)
    .with_max_batch_size(2000)
    .with_timeout_ms(15000)
    .with_max_retries(5)
    .build()?;
```

**Adaptive Batching Benefits**:
- **Idle Traffic**: Minimal latency (1ms) - sends immediately with single items
- **Low Traffic**: Small batches with minimal wait
- **Medium/High Traffic**: Scales batch size proportionally to throughput
- **Burst Traffic**: Maximum batch size for optimal throughput
- **Dynamic Adjustment**: Automatically adapts without manual tuning

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `endpoint` | gRPC server URL | String | Valid gRPC URL (grpc://host:port) | `grpc://localhost:50052` |
| `timeout_ms` | Request timeout in milliseconds | u64 | Positive integer | `5000` |
| `batch_size` | Maximum number of items per batch (fixed mode) | usize | Positive integer | `100` |
| `batch_flush_timeout_ms` | Maximum time to wait before flushing partial batch | u64 | Positive integer | `1000` |
| `max_retries` | Maximum retry attempts for failed requests | u32 | 0 or positive integer | `3` |
| `connection_retry_attempts` | Number of connection retry attempts | u32 | Positive integer | `5` |
| `initial_connection_timeout_ms` | Initial connection timeout in milliseconds | u64 | Positive integer | `10000` |
| `metadata` | Custom metadata headers to include in requests | HashMap<String, String> | Key-value pairs | Empty map |
| `priority_queue_capacity` | Capacity of the internal priority queue | Option<usize> | None or positive integer | None (uses default) |
| `auto_start` | Whether to automatically start the reaction | bool | true or false | `true` |
| `adaptive_enable` | Enable intelligent throughput-based batching | bool | true or false | `false` |
| `adaptive_min_batch_size` | Minimum items per batch (adaptive mode) | usize | 1-10000 | `10` |
| `adaptive_max_batch_size` | Maximum items per batch (adaptive mode) | usize | 10-100000 | `1000` |
| `adaptive_window_size` | Throughput window size (units of 100ms) | usize | 1-300 (0.1s-30s) | `50` (5 seconds) |
| `adaptive_batch_timeout_ms` | Maximum wait time for batch completion | u64 | 1-10000 | `100` |

### Configuration Details

- **endpoint**: Must be in the format `grpc://hostname:port`. The protocol prefix is automatically converted to `http://` for the underlying transport.

- **timeout_ms**: Controls how long the client will wait for a response from the gRPC server before timing out.

- **batch_size**: Controls the maximum number of query result items to send in a single gRPC request. Larger batches improve throughput but increase memory usage.

- **batch_flush_timeout_ms**: If a batch doesn't reach the `batch_size` within this timeout, it will be sent anyway. Prevents delays when query results arrive slowly.

- **max_retries**: Number of times to retry a failed request. Uses exponential backoff starting at 100ms, doubling each retry, up to a maximum of 5 seconds.

- **connection_retry_attempts**: When establishing the initial connection, this controls how many times to retry before giving up.

- **initial_connection_timeout_ms**: Timeout for establishing the initial connection to the gRPC server.

- **metadata**: Custom headers sent with each gRPC request. Useful for authentication, tenant identification, or routing.

- **priority_queue_capacity**: Controls the size of the internal queue. If not set, uses the default from the reaction base.

- **auto_start**: If `true`, the reaction will start immediately when added to the Drasi system. If `false`, you must manually call `start()`.

### Adaptive vs Fixed Batching

**Fixed Batching (default, `adaptive_enable: false`)**:
- Uses fixed `batch_size` regardless of traffic patterns
- Reliable, predictable behavior
- Suitable for uniform traffic patterns
- Simple configuration

**Adaptive Batching (`adaptive_enable: true`)**:
- Intelligently adjusts batch size based on measured throughput
- Optimizes for both latency (low traffic) and throughput (high traffic)
- Ideal for variable or unpredictable workloads
- Requires configuring min/max batch sizes

### Adaptive Batching Behavior

When enabled, the reaction classifies traffic into five levels and adjusts parameters automatically:

| Throughput Level | Messages/Second | Batch Size Strategy | Wait Time |
|-----------------|-----------------|---------------------|-----------|
| **Idle** | < 1 | Minimum (optimize latency) | Minimal (1ms) |
| **Low** | 1-100 | 2× minimum | 1ms |
| **Medium** | 100-1,000 | 25% of range | 10ms |
| **High** | 1,000-10,000 | 50% of range | 25ms |
| **Burst** | > 10,000 | Maximum (optimize throughput) | 50ms |

**Channel Capacity**: Internal batching channel automatically uses `max_batch_size × 5` for optimal pipelining and burst absorption.

### Configuration Details

The gRPC Reaction sends data using the Protocol Buffer format defined in the `drasi.v1` package. All communication uses the `ReactionService` with the `ProcessResults` RPC method.

### ProcessResultsRequest

```protobuf
message ProcessResultsRequest {
    QueryResult results = 1;
    map<string, string> metadata = 2;  // Custom metadata headers
}
```

### QueryResult

```protobuf
message QueryResult {
    string query_id = 1;                      // ID of the source query
    repeated QueryResultItem results = 2;     // Batch of result items
    google.protobuf.Timestamp timestamp = 3;  // When results were generated
}
```

### QueryResultItem

```protobuf
message QueryResultItem {
    string type = 1;                     // Change type: "ADD", "UPDATE", "DELETE"
    google.protobuf.Struct data = 2;     // Current data
    google.protobuf.Struct before = 3;   // Previous state (for UPDATE)
    google.protobuf.Struct after = 4;    // New state (for UPDATE)
}
```

### ProcessResultsResponse

Your gRPC service should return:

```protobuf
message ProcessResultsResponse {
    bool success = 1;          // Whether processing succeeded
    string message = 2;        // Human-readable message
    string error = 3;          // Error details if success = false
    uint32 items_processed = 4;  // Number of items processed
}
```

### Example Data Flow

When a query detects changes, the gRPC reaction sends batches like this:

```json
{
  "results": {
    "query_id": "high-temperature-sensors",
    "results": [
      {
        "type": "ADD",
        "data": {
          "id": "sensor-001",
          "temperature": 85.5,
          "location": "Building A"
        }
      },
      {
        "type": "UPDATE",
        "data": {
          "id": "sensor-002",
          "temperature": 78.0,
          "location": "Building B"
        },
        "before": {
          "id": "sensor-002",
          "temperature": 72.0,
          "location": "Building B"
        },
        "after": {
          "id": "sensor-002",
          "temperature": 78.0,
          "location": "Building B"
        }
      },
      {
        "type": "DELETE",
        "data": {
          "id": "sensor-003",
          "temperature": 65.0,
          "location": "Building C"
        }
      }
    ],
    "timestamp": "2025-12-05T10:30:00.123456Z"
  },
  "metadata": {
    "api-key": "your-api-key",
    "tenant-id": "tenant-123"
  }
}
```

## Usage Examples

### Example 1: Basic Temperature Monitoring

```rust
use drasi_reaction_grpc::GrpcReaction;

// Monitor temperature sensors and send alerts to a gRPC service
let reaction = GrpcReaction::builder("temperature-alerts")
    .with_endpoint("grpc://alerts.example.com:50052")
    .with_query("high-temperature-sensors")
    .with_batch_size(50)
    .build()?;
```

### Example 2: Multi-Query Aggregation

```rust
use drasi_reaction_grpc::GrpcReaction;

// Aggregate results from multiple queries
let reaction = GrpcReaction::builder("multi-query-aggregator")
    .with_endpoint("grpc://aggregator.example.com:50052")
    .with_queries(vec![
        "sensor-data".to_string(),
        "device-status".to_string(),
        "alert-conditions".to_string(),
    ])
    .with_batch_size(500)
    .with_batch_flush_timeout_ms(5000)
    .build()?;
```

### Example 3: Authenticated API Integration

```rust
use drasi_reaction_grpc::GrpcReaction;

// Send results to an authenticated API with custom metadata
let reaction = GrpcReaction::builder("authenticated-integration")
    .with_endpoint("grpc://api.example.com:50052")
    .with_query("customer-events")
    .with_metadata("authorization", "Bearer token-xyz")
    .with_metadata("x-api-version", "v2")
    .with_metadata("x-tenant-id", "acme-corp")
    .with_timeout_ms(15000)
    .with_max_retries(5)
    .build()?;
```

### Example 4: High-Throughput Data Pipeline

```rust
use drasi_reaction_grpc::GrpcReaction;

// Configure for high-throughput scenarios
let reaction = GrpcReaction::builder("high-throughput-pipeline")
    .with_endpoint("grpc://pipeline.example.com:50052")
    .with_query("real-time-events")
    .with_batch_size(1000)
    .with_batch_flush_timeout_ms(100)
    .with_priority_queue_capacity(10000)
    .with_timeout_ms(30000)
    .with_connection_retry_attempts(10)
    .build()?;
```

### Example 5: Programmatic Control

```rust
use drasi_reaction_grpc::GrpcReaction;
use drasi_lib::Reaction;

// Create reaction without auto-start for manual control
let reaction = GrpcReaction::builder("manual-control")
    .with_endpoint("grpc://service.example.com:50052")
    .with_query("manual-query")
    .with_auto_start(false)
    .build()?;

// Start manually when ready
reaction.start().await?;

// Check status
let status = reaction.status().await;
println!("Reaction status: {:?}", status);

// Stop when done
reaction.stop().await?;
```

### Example 6: Adaptive Batching for Variable Traffic

```rust
use drasi_reaction_grpc::GrpcReaction;

// Optimize for highly variable traffic patterns
let reaction = GrpcReaction::builder("variable-traffic-handler")
    .with_endpoint("grpc://events.example.com:9090")
    .with_query("event-stream")
    .with_adaptive_enable(true)
    .with_min_batch_size(10)       
    .with_max_batch_size(5000)     
    .with_timeout_ms(15000)
    .with_max_retries(5)
    .with_priority_queue_capacity(50000)
    .build()?;
```

### Example 7: Low-Latency Adaptive Configuration

```rust
use drasi_reaction_grpc::GrpcReaction;

// Optimize for minimal latency with small batches
let reaction = GrpcReaction::builder("low-latency-alerts")
    .with_endpoint("grpc://realtime-api:8080")
    .with_query("sensor-alerts")
    .with_adaptive_enable(true)
    .with_min_batch_size(1)        // Send single items immediately when idle
    .with_max_batch_size(100)      // Batch up to 100 during traffic spikes
    .with_timeout_ms(5000)
    .build()?;
```

### Example 8: High-Throughput Adaptive Configuration

```rust
use drasi_reaction_grpc::GrpcReaction;

// Optimize for sustained high throughput
let reaction = GrpcReaction::builder("high-throughput-pipeline")
    .with_endpoint("grpc://pipeline.example.com:9090")
    .with_query("high-volume-events")
    .with_adaptive_enable(true)
    .with_min_batch_size(500)      // Larger minimum batches
    .with_max_batch_size(10000)    // Allow very large batches
    .with_timeout_ms(30000)
    .with_priority_queue_capacity(100000)
    .build()?;
```

## Performance Tuning Guidelines

### For Adaptive Batching

**Throughput Optimization** (> 10,000 msgs/sec):
```rust
.with_adaptive_enable(true)
.with_min_batch_size(500)
.with_max_batch_size(10000)
.with_timeout_ms(30000)
.with_priority_queue_capacity(100000)
```

**Latency Optimization** (< 10ms P99):
```rust
.with_adaptive_enable(true)
.with_min_batch_size(1)
.with_max_batch_size(100)
.with_timeout_ms(5000)
```

**Balanced Configuration** (typical workloads):
```rust
.with_adaptive_enable(true)
.with_min_batch_size(50)
.with_max_batch_size(1000)
.with_timeout_ms(10000)
```

## Implementing a gRPC Service

To receive results from the gRPC Reaction, implement a gRPC service using the `drasi.v1.ReactionService` protocol.

### Example Server (Rust)

```rust
use tonic::{Request, Response, Status};
use drasi::v1::reaction_service_server::{ReactionService, ReactionServiceServer};
use drasi::v1::{ProcessResultsRequest, ProcessResultsResponse};

pub struct MyReactionService;

#[tonic::async_trait]
impl ReactionService for MyReactionService {
    async fn process_results(
        &self,
        request: Request<ProcessResultsRequest>,
    ) -> Result<Response<ProcessResultsResponse>, Status> {
        let req = request.into_inner();
        let query_result = req.results.unwrap();

        println!("Received {} items from query: {}",
                 query_result.results.len(),
                 query_result.query_id);

        // Process each result item
        for item in query_result.results {
            match item.r#type.as_str() {
                "ADD" => {
                    println!("New item added: {:?}", item.data);
                }
                "UPDATE" => {
                    println!("Item updated from {:?} to {:?}",
                             item.before, item.after);
                }
                "DELETE" => {
                    println!("Item deleted: {:?}", item.data);
                }
                _ => {}
            }
        }

        Ok(Response::new(ProcessResultsResponse {
            success: true,
            message: "Processed successfully".to_string(),
            error: String::new(),
            items_processed: query_result.results.len() as u32,
        }))
    }
}
```

### Example Server (Python)

```python
import grpc
from concurrent import futures
from drasi.v1 import reaction_pb2, reaction_pb2_grpc

class ReactionServicer(reaction_pb2_grpc.ReactionServiceServicer):
    def ProcessResults(self, request, context):
        query_id = request.results.query_id
        items = request.results.results

        print(f"Received {len(items)} items from query: {query_id}")

        for item in items:
            if item.type == "ADD":
                print(f"New item: {item.data}")
            elif item.type == "UPDATE":
                print(f"Updated: {item.before} -> {item.after}")
            elif item.type == "DELETE":
                print(f"Deleted: {item.data}")

        return reaction_pb2.ProcessResultsResponse(
            success=True,
            message="Processed successfully",
            items_processed=len(items)
        )

# Start server
server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
reaction_pb2_grpc.add_ReactionServiceServicer_to_server(
    ReactionServicer(), server
)
server.add_insecure_port('[::]:50052')
server.start()
server.wait_for_termination()
```

## Adaptive vs Fixed Batching Comparison

| Feature | Adaptive Batching | Fixed Batching |
|---------|-------------------|-----------------|
| **Batching Strategy** | Dynamic, throughput-based | Fixed size |
| **Traffic Classification** | 5-level system (Idle/Low/Medium/High/Burst) | None |
| **Parameter Adjustment** | Automatic based on measured throughput | Manual configuration |
| **Idle Traffic Latency** | Minimal (1ms - sends immediately) | Fixed wait (batch_flush_timeout_ms) |
| **Burst Traffic Handling** | Optimal (fills batches quickly) | Fixed batch size |
| **Channel Capacity** | 5× max_batch_size (dynamic buffering) | Fixed size |
| **Best Use Case** | Variable or unpredictable traffic | Predictable, uniform traffic |
| **Configuration Complexity** | Higher (min/max batch, window size) | Lower (single batch_size) |
| **Overhead** | Minimal throughput monitoring | None |
| **Recommended When** | Traffic patterns vary significantly | Traffic is consistent |

### When to Use Adaptive Batching

✓ **E-commerce platforms** - Traffic varies by time of day, promotions, sales
✓ **SaaS multi-tenant** - Each tenant has different activity patterns
✓ **Real-time dashboards** - Mix of idle periods and burst updates
✓ **Log aggregation** - Application logging varies by operation type
✓ **Event sourcing** - Event rates depend on business activity
✓ **IoT systems** - Sensor data collection patterns vary

### When to Use Fixed Batching

✓ **Predictable workloads** - Consistent, uniform message rates
✓ **Simpler configuration** - Set one batch_size and forget it
✓ **Minimal overhead** - No throughput monitoring needed
✓ **Legacy systems** - Requirement for deterministic behavior

## Error Handling and Retry Behavior

The gRPC Reaction implements sophisticated error handling and retry logic:

### Connection Errors

- **GoAway**: When the server sends a GoAway signal, the reaction immediately creates a fresh connection
- **Broken Pipe/Connection Reset**: Triggers automatic reconnection with exponential backoff
- **Unavailable**: Retries with backoff, creates new connection after multiple failures
- **Timeout**: Retries up to `max_retries` times with exponential backoff

### Retry Strategy

1. **Exponential Backoff**: Starts at 100ms, doubles each retry, max 5 seconds
2. **Jitter**: Adds random 0-100ms to prevent thundering herd
3. **Max Retry Duration**: Total retry time capped at 60 seconds
4. **Connection Retry**: Separate retry logic for initial connections

### Failure Modes

- **Application Errors**: If server returns `success: false`, retries up to `max_retries`
- **Connection Failures**: Automatically attempts to reconnect
- **Max Retries Exceeded**: Returns error and stops processing that batch
- **Fatal Errors**: Component transitions to Failed status

## Performance Considerations

### Batching

- Larger batches (`batch_size`) improve throughput but increase latency
- Smaller batches reduce latency but increase overhead
- Use `batch_flush_timeout_ms` to balance latency vs efficiency

### Connection Management

- Lazy connections reduce initial overhead
- Connection pooling handled automatically by underlying transport
- Reconnection logic minimizes disruption from transient failures

### Queue Management

- Priority queue ensures orderly processing of results
- Configure `priority_queue_capacity` based on expected throughput
- Monitor queue depth to prevent memory exhaustion

### Best Practices

1. **Set appropriate timeouts**: Match `timeout_ms` to your server's processing time
2. **Configure retries**: Balance reliability vs latency with `max_retries`
3. **Use metadata wisely**: Keep metadata small to minimize overhead
4. **Monitor connection state**: Log connection failures for operational visibility
5. **Test failure scenarios**: Verify retry behavior matches your requirements

## Logging

The gRPC Reaction uses the standard Rust `log` crate with these levels:

- **ERROR**: Failed requests, connection failures, max retries exceeded
- **WARN**: Retry attempts, connection issues, server-side failures
- **INFO**: Successful connections, batch sends, state transitions
- **DEBUG**: Detailed request/response information, retry logic
- **TRACE**: Low-level connection details, individual result processing

Enable logging by configuring the `RUST_LOG` environment variable:

```bash
RUST_LOG=drasi_reaction_grpc=debug cargo run
```

## Thread Safety

The GrpcReaction is designed for concurrent use:

- Internal state protected by async-safe primitives (RwLock, Arc)
- Safe to share across async tasks
- Priority queue handles concurrent access automatically
- Connection state managed safely across retries

## Dependencies

Key dependencies:

- `drasi-lib`: Core Drasi library
- `tonic`: gRPC framework
- `prost`: Protocol Buffer implementation
- `tokio`: Async runtime
- `async-trait`: Async trait support
- `serde`: Configuration serialization
- `anyhow`: Error handling

## License

Copyright 2025 The Drasi Authors. Licensed under the Apache License, Version 2.0.
