# gRPC Adaptive Reaction

A high-performance gRPC reaction plugin for Drasi that intelligently adapts batching behavior based on real-time throughput patterns to optimize both latency and throughput.

## Overview

The GrpcAdaptiveReaction component sends continuous query results to external gRPC servers with intelligent, throughput-based batching. Unlike fixed-size batching, it dynamically adjusts batch sizes and wait times based on traffic patterns to provide optimal performance across varying workloads.

### Key Capabilities

- **Adaptive Batching**: Automatically adjusts batch size (min to max) based on measured throughput
- **Traffic Classification**: Five-level classification system (Idle/Low/Medium/High/Burst) for intelligent parameter tuning
- **Lazy Connection**: Defers gRPC connection establishment until first batch is ready, reducing resource usage
- **Automatic Retry**: Exponential backoff with configurable retries for transient failures
- **Metadata Support**: Custom gRPC headers for authentication, tenant isolation, and routing
- **Backpressure Handling**: Priority queue with configurable capacity prevents memory exhaustion
- **Performance Optimized**: Pipeline parallelism with buffered channels for sustained high throughput

### Use Cases

**High-Throughput Event Processing**
- Stream processing systems requiring efficient batch delivery
- Event aggregation pipelines handling thousands to millions of events per second
- Data ingestion systems with variable traffic patterns

**Real-Time Data Synchronization**
- Change data capture (CDC) pipelines
- Database replication systems
- Cache invalidation and synchronization

**Monitoring and Analytics**
- Metrics aggregation and forwarding
- Log shipping and centralization
- Time-series data collection

**Multi-Tenant Systems**
- SaaS platforms with per-tenant data streams
- Isolated processing pipelines using metadata headers
- Resource-efficient fan-out to multiple consumers

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a type-safe, ergonomic API for constructing reactions:

```rust
use drasi_reaction_grpc_adaptive::AdaptiveGrpcReaction;

let reaction = AdaptiveGrpcReaction::builder("event-processor")
    .with_endpoint("grpc://event-server:9090")
    .with_queries(vec!["event-stream".to_string()])
    .with_min_batch_size(50)
    .with_max_batch_size(2000)
    .with_timeout_ms(15000)
    .with_max_retries(5)
    .with_metadata("authorization", "Bearer token123")
    .with_metadata("x-tenant-id", "prod-456")
    .with_priority_queue_capacity(10000)
    .with_auto_start(true)
    .build()?;
```

### Config Struct Approach

For YAML-based configuration or programmatic construction:

```rust
use drasi_reaction_grpc_adaptive::GrpcAdaptiveReactionConfig;
use drasi_lib::reactions::common::AdaptiveBatchConfig;
use std::collections::HashMap;

let mut metadata = HashMap::new();
metadata.insert("authorization".to_string(), "Bearer token123".to_string());

let config = GrpcAdaptiveReactionConfig {
    endpoint: "grpc://event-server:9090".to_string(),
    timeout_ms: 15000,
    max_retries: 5,
    connection_retry_attempts: 5,
    initial_connection_timeout_ms: 10000,
    metadata,
    adaptive: AdaptiveBatchConfig {
        adaptive_min_batch_size: 50,
        adaptive_max_batch_size: 2000,
        adaptive_window_size: 100,  // 10 seconds (100 × 100ms)
        adaptive_batch_timeout_ms: 500,
    },
};

let reaction = AdaptiveGrpcReaction::new(
    "event-processor",
    vec!["event-stream".to_string()],
    config,
);
```

## Configuration Options

### Core Settings

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `endpoint` | gRPC server endpoint URL | String | `grpc://host:port` | `grpc://localhost:50052` |
| `timeout_ms` | Request timeout in milliseconds | u64 | 100-300000 | 5000 |
| `max_retries` | Maximum retries for failed requests | u32 | 0-100 | 3 |
| `connection_retry_attempts` | Connection retry attempts before giving up | u32 | 0-100 | 5 |
| `initial_connection_timeout_ms` | Initial connection timeout (lazy connect) | u64 | 100-60000 | 10000 |
| `metadata` | gRPC metadata headers | HashMap&lt;String, String&gt; | Key-value pairs | Empty |

### Adaptive Batching Settings

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `adaptive_min_batch_size` | Minimum items per batch (low traffic) | usize | 1-10000 | 10 |
| `adaptive_max_batch_size` | Maximum items per batch (high traffic) | usize | 10-100000 | 1000 |
| `adaptive_window_size` | Throughput window size (units of 100ms) | usize | 1-300 (0.1s-30s) | 50 (5 seconds) |
| `adaptive_batch_timeout_ms` | Maximum wait time for batch completion | u64 | 1-10000 | 100 |

### Additional Settings

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `priority_queue_capacity` | Queue capacity for backpressure handling | Option&lt;usize&gt; | None or 100-1000000 | None (unlimited) |
| `auto_start` | Start reaction automatically on creation | bool | true/false | true |

### Adaptive Batching Behavior

The reaction classifies traffic into five levels and adjusts parameters accordingly:

| Throughput Level | Messages/Second | Batch Size Strategy | Wait Time |
|-----------------|-----------------|---------------------|-----------|
| **Idle** | < 1 | Minimum (optimize latency) | Minimal (1ms) |
| **Low** | 1-100 | 2× minimum | 1ms |
| **Medium** | 100-1,000 | 25% of range | 10ms |
| **High** | 1,000-10,000 | 50% of range | 25ms |
| **Burst** | > 10,000 | Maximum (optimize throughput) | 50ms |

**Channel Capacity**: Internal batching channel uses `max_batch_size × 5` for optimal pipelining and burst absorption.

## Output Schema

The reaction sends data using the gRPC protobuf schema defined in `drasi.v1.ReactionService`:

### ProcessResults RPC

**Request Format** (`ProcessResultsRequest`):

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
    string type = 1;              // "ADD", "UPDATE", "DELETE"
    google.protobuf.Struct data = 2;
    google.protobuf.Struct before = 3;  // For UPDATE events
    google.protobuf.Struct after = 4;   // For UPDATE events
}
```

**Response Format** (`ProcessResultsResponse`):

```protobuf
message ProcessResultsResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    uint32 items_processed = 4;
}
```

### Example JSON Representation

**Add Event**:
```json
{
  "query_id": "event-stream",
  "results": [
    {
      "type": "ADD",
      "data": {
        "id": "sensor-123",
        "temperature": 75.5,
        "timestamp": 1704067200
      }
    }
  ],
  "timestamp": "2025-01-01T00:00:00Z"
}
```

**Update Event**:
```json
{
  "query_id": "event-stream",
  "results": [
    {
      "type": "UPDATE",
      "before": {
        "id": "sensor-123",
        "temperature": 75.5
      },
      "after": {
        "id": "sensor-123",
        "temperature": 80.2
      }
    }
  ],
  "timestamp": "2025-01-01T00:00:10Z"
}
```

**Delete Event**:
```json
{
  "query_id": "event-stream",
  "results": [
    {
      "type": "DELETE",
      "data": {
        "id": "sensor-123"
      }
    }
  ],
  "timestamp": "2025-01-01T00:00:20Z"
}
```

## Usage Examples

### Example 1: High-Throughput Event Stream

Optimized for sustained high throughput with large batches:

```rust
use drasi_reaction_grpc_adaptive::AdaptiveGrpcReaction;

let reaction = AdaptiveGrpcReaction::builder("high-throughput")
    .with_endpoint("grpc://event-processor:9090")
    .with_queries(vec!["event-stream".to_string()])
    .with_min_batch_size(100)
    .with_max_batch_size(5000)
    .with_timeout_ms(30000)
    .with_max_retries(5)
    .build()?;
```

**Traffic Pattern**: Handles 1,000-100,000 messages/second
**Batch Behavior**: Starts at 100, scales to 5,000 during bursts
**Use Case**: Log aggregation, metrics collection, event sourcing

### Example 2: Low-Latency Real-Time Updates

Optimized for minimal latency with small batches:

```rust
let reaction = AdaptiveGrpcReaction::builder("low-latency")
    .with_endpoint("grpc://realtime-api:8080")
    .with_queries(vec!["sensor-alerts".to_string()])
    .with_min_batch_size(1)
    .with_max_batch_size(50)
    .with_timeout_ms(5000)
    .build()?;
```

**Traffic Pattern**: 1-100 messages/second
**Batch Behavior**: Sends immediately when idle, batches up to 50 during spikes
**Use Case**: Real-time dashboards, instant notifications, monitoring alerts

### Example 3: Multi-Tenant with Authentication

Production configuration with tenant isolation:

```rust
let reaction = AdaptiveGrpcReaction::builder("tenant-processor")
    .with_endpoint("grpc://multi-tenant-api:9090")
    .with_queries(vec!["tenant-events".to_string()])
    .with_min_batch_size(25)
    .with_max_batch_size(1000)
    .with_metadata("authorization", "Bearer eyJhbGciOiJSUzI1...")
    .with_metadata("x-tenant-id", "prod-456")
    .with_metadata("x-region", "us-west-2")
    .with_priority_queue_capacity(50000)
    .with_timeout_ms(15000)
    .with_max_retries(3)
    .build()?;
```

**Features**: JWT authentication, tenant routing, regional metadata
**Traffic Pattern**: Variable (100-10,000 messages/second)
**Use Case**: SaaS platforms, multi-tenant analytics, isolated processing

### Example 4: Mission-Critical with Aggressive Retry

Maximum reliability configuration:

```rust
let mut config = GrpcAdaptiveReactionConfig::default();
config.endpoint = "grpc://critical-service:9090".to_string();
config.timeout_ms = 60000;  // 1 minute timeout
config.max_retries = 10;
config.connection_retry_attempts = 20;
config.initial_connection_timeout_ms = 30000;
config.adaptive.adaptive_min_batch_size = 10;
config.adaptive.adaptive_max_batch_size = 500;

let reaction = AdaptiveGrpcReaction::new(
    "mission-critical",
    vec!["critical-events".to_string()],
    config,
);
```

**Resilience**: 10 retries per batch, 20 connection retries
**Timeouts**: 60s request, 30s initial connection
**Use Case**: Financial transactions, healthcare data, compliance systems

### Example 5: Variable Traffic with Wide Adaptation Range

Handles both quiet periods and traffic bursts efficiently:

```rust
let reaction = AdaptiveGrpcReaction::builder("variable-traffic")
    .with_endpoint("grpc://adaptive-backend:9090")
    .with_queries(vec!["user-activity".to_string()])
    .with_min_batch_size(1)      // Single items during quiet periods
    .with_max_batch_size(10000)  // Large batches during peak
    .with_priority_queue_capacity(100000)
    .build()?;
```

**Adaptation Range**: 1-10,000 items per batch
**Traffic Pattern**: Highly variable (0-50,000 messages/second)
**Use Case**: E-commerce, social media, gaming analytics

## Performance Tuning Guidelines

### Throughput Optimization

For maximum throughput (> 10,000 msgs/sec):

```rust
.with_min_batch_size(500)
.with_max_batch_size(10000)
.with_timeout_ms(30000)
.with_priority_queue_capacity(100000)
```

### Latency Optimization

For minimum latency (< 10ms P99):

```rust
.with_min_batch_size(1)
.with_max_batch_size(50)
.with_timeout_ms(5000)
```

### Balanced Configuration

For typical workloads:

```rust
.with_min_batch_size(50)
.with_max_batch_size(1000)
.with_timeout_ms(10000)
```

## Error Handling

The reaction implements comprehensive error handling:

1. **Connection Failures**: Exponential backoff with configurable retry attempts
2. **Request Timeouts**: Automatic retry with exponential backoff (100ms × 2^retry)
3. **Batch Processing Errors**: Logged with metrics tracking (successful/failed sends)
4. **Channel Closure**: Graceful shutdown with pending batch completion
5. **Client Recreation**: Automatic client reconnection after persistent failures

### Monitoring and Logging

The reaction logs operational metrics:

```
INFO  Adaptive gRPC reaction starting for endpoint: grpc://server:9090 (lazy connection)
DEBUG Adaptive batch collected - Size: 1247, Target: 1000, Wait: 25ms, Level: High
INFO  Adaptive metrics - Successful: 100, Failed: 0
WARN  Failed to send batch (retry 1/3): connection reset
ERROR Failed to send batch after 3 retries
```

## Implementation Details

### Adaptive Batching Algorithm

1. **Throughput Monitoring**: Tracks message rates in a sliding window (default 5 seconds)
2. **Traffic Classification**: Categorizes throughput into five levels
3. **Parameter Adjustment**: Updates batch size and wait time based on classification
4. **Burst Detection**: Detects large channel backlogs and fills batches immediately
5. **Pipeline Optimization**: Uses 5× channel buffering for parallelism

### Component Lifecycle

1. **Creation**: Reaction initialized in Stopped state
2. **Start**: Subscribes to queries, sets status to Running, spawns processing tasks
3. **Processing**: Main loop dequeues from priority queue, adaptive batcher collects batches
4. **Stop**: Sets status to Stopping, unsubscribes from queries, waits for tasks
5. **Cleanup**: Flushes pending batches, closes channels, sets status to Stopped

### Thread Architecture

- **Main Thread**: Dequeues query results from priority queue
- **Batcher Thread**: Collects items into adaptive batches
- **Network Thread**: Sends batches via gRPC (within batcher thread)

## Dependencies

```toml
[dependencies]
drasi-lib = { path = "../../../lib" }
drasi-reaction-grpc = { path = "../grpc" }
anyhow = "1.0"
async-trait = "0.1"
log = "0.4"
tonic = "0.11"
prost = "0.12"
tokio = { version = "1.0", features = ["full"] }
serde = { version = "1.0", features = ["derive"] }
```

## Comparison with Standard gRPC Reaction

| Feature | GrpcAdaptiveReaction | GrpcReaction |
|---------|---------------------|--------------|
| Batching | Adaptive (dynamic) | Fixed size |
| Throughput Classification | 5 levels | None |
| Parameter Adjustment | Automatic | Manual |
| Latency (idle traffic) | Minimal (1ms) | Fixed wait |
| Throughput (burst traffic) | Optimal (large batches) | Fixed |
| Channel Capacity | 5× max batch | Fixed |
| Use Case | Variable traffic | Predictable traffic |

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
