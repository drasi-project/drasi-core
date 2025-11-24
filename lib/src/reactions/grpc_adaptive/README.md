# Adaptive gRPC Reaction

The Adaptive gRPC reaction extends the standard gRPC reaction with intelligent, throughput-based batching that automatically adjusts batch sizes and wait times based on real-time traffic patterns.

## Key Differences from Standard gRPC

| Feature | Standard gRPC | Adaptive gRPC |
|---------|---------------|---------------|
| **Batch Size** | Fixed (`batch_size`) | Dynamic (adapts between min/max) |
| **Wait Time** | Fixed (`batch_flush_timeout_ms`) | Dynamic (adapts between min/max) |
| **Throughput Monitoring** | None | Continuous with sliding window |
| **Traffic Classification** | None | Five levels (Idle/Low/Medium/High/Burst) |
| **Performance Tuning** | Manual | Automatic |

## When to Use

**Use Adaptive gRPC when:**
- Traffic patterns are unpredictable or variable
- You want optimal performance without manual tuning
- Workload includes both idle periods and bursts
- Operating across multiple environments with different traffic

**Use Standard gRPC when:**
- Traffic patterns are stable and predictable
- You have specific batch size requirements
- You need deterministic batching behavior
- Simpler configuration is preferred

## Configuration

### Configuration Settings

The Adaptive gRPC Reaction supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the reaction | Any string | **(Required)** |
| `queries` | Array[String] | IDs of queries this reaction subscribes to | Array of query IDs | **(Required)** |
| `reaction_type` | String | Reaction type discriminator | "grpc_adaptive" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this reaction | true, false | `true` |
| `endpoint` | String | gRPC server endpoint URL. Must start with `grpc://` or `grpcs://` for secure connections | Valid gRPC URL (e.g., `grpc://localhost:50052`) | `"grpc://localhost:50052"` |
| `timeout_ms` | u64 | Request timeout in milliseconds. Applies to individual gRPC calls | Any positive integer | `5000` |
| `max_retries` | u32 | Maximum number of retry attempts for failed batch send requests. Uses exponential backoff between retries | 0-100 | `3` |
| `connection_retry_attempts` | u32 | Number of connection retry attempts when establishing initial connection to gRPC server | 0-100 | `5` |
| `initial_connection_timeout_ms` | u64 | Timeout in milliseconds for the initial connection attempt to the gRPC server (used for lazy connection) | Any positive integer | `10000` |
| `metadata` | HashMap<String, String> | gRPC metadata headers to include in all requests. Key-value pairs are converted to gRPC metadata | Map of header names to values | Empty HashMap |
| `adaptive_min_batch_size` | usize | Minimum batch size (events per batch) used during idle/low traffic periods | Any positive integer | `1` |
| `adaptive_max_batch_size` | usize | Maximum batch size (events per batch) used during burst traffic periods | Any positive integer | `100` |
| `adaptive_window_size` | usize | Window size for throughput monitoring in 100ms units. For example: 10 = 1 second, 50 = 5 seconds, 100 = 10 seconds. Larger windows provide more stable adaptation but respond slower to traffic changes | 1-255 (in 100ms units) | `10` (1 second) |
| `adaptive_batch_timeout_ms` | u64 | Maximum time to wait before flushing a partial batch (milliseconds). Ensures results are delivered even when batch size is not reached | Any positive integer in milliseconds | `1000` |
| `priority_queue_capacity` | Integer (Optional) | Maximum events in priority queue before backpressure. Controls event queuing before the reaction processes them. Higher values allow more buffering but use more memory | Any positive integer | `10000` |

### Adaptive Algorithm Behavior

The adaptive batcher monitors throughput over the configured window and adjusts batch size and wait time based on traffic level:

| Throughput Level | Messages/Sec | Batch Size | Wait Time |
|-----------------|--------------|------------|-----------|
| Idle | < 1 | min_batch_size | 1ms |
| Low | 1-100 | 2 × min | 1ms |
| Medium | 100-1K | 25% of max | 10ms |
| High | 1K-10K | 50% of max | 25ms |
| Burst | > 10K | max_batch_size | 50ms |

**Note**: The standard gRPC properties `batch_size` and `batch_flush_timeout_ms` are not used in Adaptive gRPC - use the adaptive-specific properties instead.

## Configuration Examples

### Basic (Default Settings)

```yaml
reactions:
  - id: "adaptive-grpc-reaction"
    queries: ["sensor-alerts"]
    reaction_type: "grpc_adaptive"
    auto_start: true
    endpoint: "grpc://localhost:50052"
```

### High-Throughput (Optimize for Throughput)

```yaml
reactions:
  - id: "high-throughput"
    queries: ["high-volume-events"]
    reaction_type: "grpc_adaptive"
    auto_start: true
    priority_queue_capacity: 30000
    endpoint: "grpc://event-processor:9090"
    adaptive_max_batch_size: 2000
    adaptive_min_batch_size: 50
    adaptive_window_size: 100  # 10 seconds
    adaptive_batch_timeout_ms: 500
    timeout_ms: 15000
    max_retries: 5
```

### Low-Latency (Optimize for Speed)

```yaml
reactions:
  - id: "low-latency"
    queries: ["critical-alerts"]
    reaction_type: "grpc_adaptive"
    auto_start: true
    endpoint: "grpc://alert-service:50052"
    adaptive_max_batch_size: 100
    adaptive_min_batch_size: 5
    adaptive_window_size: 30  # 3 seconds
    adaptive_batch_timeout_ms: 50
    timeout_ms: 3000
```

### JSON Configuration

```json
{
  "reactions": [{
    "id": "adaptive-grpc-reaction",
    "queries": ["sensor-alerts"],
    "reaction_type": "grpc_adaptive",
    "auto_start": true,
    "endpoint": "grpc://localhost:50052",
    "adaptive_max_batch_size": 2000,
    "adaptive_min_batch_size": 50,
    "adaptive_window_size": 100,
    "adaptive_batch_timeout_ms": 500
  }]
}
```

### Rust API

```rust
use drasi_server_core::config::{ReactionConfig, ReactionSpecificConfig};
use drasi_server_core::config::typed::{GrpcAdaptiveReactionConfig, AdaptiveBatchConfig};
use std::collections::HashMap;

// Basic usage with defaults
let config = ReactionConfig {
    id: "my-reaction".to_string(),
    queries: vec!["temperature-query".to_string()],
    auto_start: true,
    config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
        endpoint: "grpc://localhost:50052".to_string(),
        timeout_ms: 5000,
        max_retries: 3,
        connection_retry_attempts: 5,
        initial_connection_timeout_ms: 10000,
        metadata: HashMap::new(),
        adaptive: AdaptiveBatchConfig {
            adaptive_min_batch_size: 1,
            adaptive_max_batch_size: 100,
            adaptive_window_size: 10,
            adaptive_batch_timeout_ms: 1000,
        },
    }),
    priority_queue_capacity: None,
};

// High-throughput configuration
let mut metadata = HashMap::new();
metadata.insert("authorization".to_string(), "Bearer token123".to_string());

let config = ReactionConfig {
    id: "high-throughput".to_string(),
    queries: vec!["event-stream".to_string()],
    auto_start: true,
    config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
        endpoint: "grpc://event-processor:9090".to_string(),
        timeout_ms: 15000,
        max_retries: 5,
        connection_retry_attempts: 5,
        initial_connection_timeout_ms: 10000,
        metadata,
        adaptive: AdaptiveBatchConfig {
            adaptive_min_batch_size: 50,
            adaptive_max_batch_size: 2000,
            adaptive_window_size: 100,  // 10 seconds
            adaptive_batch_timeout_ms: 500,
        },
    }),
    priority_queue_capacity: None,
};
```

## Adaptive Batching Algorithm

### Traffic Classification

Messages per second determine the traffic level and batching behavior:

| Level | Msgs/Sec | Batch Size | Wait Time |
|-------|----------|------------|-----------|
| **Idle** | < 1 | min_batch_size | min_wait_time (1ms) |
| **Low** | 1-100 | min_batch_size × 2 | 1ms |
| **Medium** | 100-1K | 25% between min/max | 10ms |
| **High** | 1K-10K | 50% between min/max | 25ms |
| **Burst** | > 10K | max_batch_size | 50ms |

### How It Works

1. **Receive First Message** - Block waiting for at least one message
2. **Calculate Throughput** - Measure msgs/sec over the configured window
3. **Classify Traffic** - Determine current traffic level
4. **Adjust Parameters** - Update batch size and wait time
5. **Collect Batch** - Gather messages up to current limits
6. **Send Batch** - Forward to gRPC endpoint
7. **Record Metrics** - Update throughput history
8. **Repeat** - Continue adaptive cycle

### Throughput Window

The sliding window acts as a smoothing buffer (measured in 100ms units):

- **Short (10-30 units / 1-3s)**: Fast response, may oscillate
- **Medium (50-100 units / 5-10s)**: Stable, tolerates spikes (default)
- **Long (150-255 units / 15-25.5s)**: Very smooth, slow to detect changes

## Proto Schema

Uses the same protobuf schema as standard gRPC reaction. See [standard gRPC docs](../grpc/README.md#output-data-format) for:
- `ProcessResultsRequest` structure
- `QueryResult` protobuf message
- `QueryResultItem` structure

Proto definition: `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/proto/drasi/v1/reaction.proto`

## Connection Management

### Lazy Connection
- Connection established only when first batch arrives
- No proactive connection on startup
- Reduces resource usage when idle

### Retry Behavior
- Exponential backoff: `100ms × 2^failures` (max 30s)
- Automatic reconnection on connection errors
- GoAway frames handled with fresh connections
- Batch preserved on connection failures

### Metrics
Logged periodically (every 100 successful sends):
- Successful sends
- Failed sends
- Connection attempts
- GoAway count

## Performance Tuning

### High-Throughput Settings

```yaml
adaptive_max_batch_size: 2000
adaptive_min_batch_size: 100
adaptive_window_size: 100  # 10 seconds
adaptive_batch_timeout_ms: 1000
```

**Effect**: Large batches during bursts, maintains efficiency during lulls

### Low-Latency Settings

```yaml
adaptive_max_batch_size: 50
adaptive_min_batch_size: 1
adaptive_window_size: 30  # 3 seconds
adaptive_batch_timeout_ms: 20
```

**Effect**: Small batches, minimal wait times, fast adaptation

### Balanced Settings (Default)

```yaml
adaptive_max_batch_size: 100
adaptive_min_batch_size: 1
adaptive_window_size: 10  # 1 second
adaptive_batch_timeout_ms: 1000
```

**Effect**: Good balance for variable workloads

## Troubleshooting

### Adaptive Not Working (Stays at Min or Max)

**Symptom**: Batch size never changes

**Causes:**
- Window too large - Reduce `adaptive_window_size`
- Min/max too close - Increase range
- Traffic is consistently at one extreme (expected behavior)

### Batches Not Filling

**Symptom**: Actual batch < target batch size

**Causes:**
- Insufficient traffic (expected for low traffic)
- Wait time too short - Increase `adaptive_batch_timeout_ms`
- Multiple queries (each batched separately)

### High Memory Usage

**Causes:**
- Very large `adaptive_max_batch_size` - Reduce to 500-2000
- Channel buffer full (batcher slow) - Check network/server
- Large throughput window - Reduce `adaptive_window_size`

### Enable Debug Logging

```bash
RUST_LOG=drasi_server_core::reactions::grpc_adaptive=debug,\
drasi_server_core::utils::adaptive_batcher=trace cargo run
```

**Key log messages:**
```
TRACE: Adapted batching parameters - Level: High, Rate: 5432.1 msgs/sec, Batch: 505, Wait: 25ms
DEBUG: Adaptive batch collected - Size: 487, Target: 505, Wait: 25ms, Level: High
INFO: Adaptive metrics - Successful: 500, Failed: 3
```

## Limitations

### Inherited from Standard gRPC
- Protobuf message size limits (4MB default)
- Single connection per reaction
- See [standard gRPC limitations](../grpc/README.md#limitations)

### Adaptive-Specific

**Algorithm Overhead**:
- +0.1-1% CPU for monitoring
- +10-50KB memory for throughput history
- Negligible for most workloads

**Tuning Complexity**:
- 4 adaptive parameters vs 2 for standard gRPC
- Understanding parameter interactions
- Use defaults for most scenarios
- `min_wait_time` is hardcoded to 100ms (not configurable) to avoid excessive gRPC calls

**Workload Sensitivity**:
- Works best: Gradual changes, sustained levels
- Works poorly: Extremely short bursts (<1s), rapid oscillation

**Adaptation Lag**:
- ~½ window duration delay to adapt
- 0.5-1s transition time for default settings (window_size=10)
- Trade-off for stability

**Non-Deterministic**:
- Batch sizes vary based on timing
- Different runs produce different sequences

## Architecture

### Two-Task Design

```
┌─────────────────────────────────────────────────────┐
│           Adaptive gRPC Reaction                    │
│                                                      │
│  ┌──────────────┐         ┌──────────────────┐     │
│  │ Receiver     │ ─────> │ Batcher          │     │
│  │ Task         │  mpsc   │ Task             │     │
│  │              │ channel │                  │     │
│  │ - Receives   │         │ - AdaptiveBatcher│────>│
│  │ - Converts   │         │ - Monitors       │ gRPC│
│  │ - Groups     │         │ - Sends batches  │     │
│  └──────────────┘         └──────────────────┘     │
└─────────────────────────────────────────────────────┘
```

**Receiver Task**: Transforms query results to protobuf, pre-batches up to 100 items

**Batcher Task**: Applies adaptive algorithm, manages gRPC connection, sends batches

**Channel**: 1000-message buffer for `(query_id, Vec<ProtoQueryResultItem>)` tuples

## See Also

- [Standard gRPC Reaction](../grpc/README.md) - For standard gRPC features
- [gRPC Service Requirements](../grpc/README.md#grpc-service-requirements) - Server implementation guide
- [Proto Schema](../grpc/README.md#proto-schema-documentation) - Detailed protobuf documentation
