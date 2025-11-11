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

### Standard Properties

Inherits all standard gRPC properties (see [standard gRPC docs](../grpc/README.md)):
- `endpoint` - gRPC server endpoint (required)
- `timeout_ms` - Request timeout (default: 5000)
- `max_retries` - Retry attempts (default: 3)
- `connection_retry_attempts` - Connection retries (default: 5)
- `initial_connection_timeout_ms` - Initial timeout (default: 10000)
- `metadata` - gRPC metadata headers

**Note:** `batch_size` and `batch_flush_timeout_ms` are ignored - use adaptive properties instead.

### Adaptive Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `adaptive_max_batch_size` | number | 1000 | Maximum batch size (burst traffic) |
| `adaptive_min_batch_size` | number | 10 | Minimum batch size (idle/low traffic) |
| `adaptive_max_wait_ms` | number | 100 | Maximum wait time before flush |
| `adaptive_min_wait_ms` | number | 1 | Minimum wait time for coalescing |
| `adaptive_window_secs` | number | 5 | Throughput calculation window |
| `adaptive_enabled` | boolean | true | Enable adaptive batching |

## Configuration Examples

### Basic (Default Settings)

```yaml
reactions:
  - id: "adaptive-grpc-reaction"
    reaction_type: "grpc_adaptive"
    queries: ["sensor-alerts"]
    auto_start: true
    properties:
      endpoint: "grpc://localhost:50052"
```

### High-Throughput (Optimize for Throughput)

```yaml
reactions:
  - id: "high-throughput"
    reaction_type: "grpc_adaptive"
    queries: ["high-volume-events"]
    auto_start: true
    properties:
      endpoint: "grpc://event-processor:9090"
      adaptive_max_batch_size: 2000
      adaptive_min_batch_size: 50
      adaptive_max_wait_ms: 500
      adaptive_min_wait_ms: 5
      adaptive_window_secs: 10
      timeout_ms: 15000
      max_retries: 5
```

### Low-Latency (Optimize for Speed)

```yaml
reactions:
  - id: "low-latency"
    reaction_type: "grpc_adaptive"
    queries: ["critical-alerts"]
    auto_start: true
    properties:
      endpoint: "grpc://alert-service:50052"
      adaptive_max_batch_size: 100
      adaptive_min_batch_size: 5
      adaptive_max_wait_ms: 50
      adaptive_min_wait_ms: 1
      adaptive_window_secs: 3
      timeout_ms: 3000
```

### JSON Configuration

```json
{
  "reactions": [{
    "id": "adaptive-grpc-reaction",
    "reaction_type": "grpc_adaptive",
    "queries": ["sensor-alerts"],
    "auto_start": true,
    "properties": {
      "endpoint": "grpc://localhost:50052",
      "adaptive_max_batch_size": 2000,
      "adaptive_min_batch_size": 50,
      "adaptive_max_wait_ms": 500,
      "adaptive_min_wait_ms": 5,
      "adaptive_window_secs": 10
    }
  }]
}
```

### Rust API

```rust
use drasi_server_core::api::{Reaction, Properties};
use serde_json::json;

// Basic usage with defaults
let reaction = Reaction::grpc_adaptive("my-reaction")
    .subscribe_to("temperature-query")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "grpc://localhost:50052")
    )
    .build();

// High-throughput configuration
let reaction = Reaction::grpc_adaptive("high-throughput")
    .subscribe_to("event-stream")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "grpc://event-processor:9090")
            .with_number("adaptive_max_batch_size", 2000)
            .with_number("adaptive_min_batch_size", 50)
            .with_number("adaptive_max_wait_ms", 500)
            .with_number("adaptive_min_wait_ms", 5)
            .with_number("adaptive_window_secs", 10)
            .with("metadata", json!({
                "authorization": "Bearer token123"
            }))
    )
    .build();
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

The sliding window acts as a smoothing buffer:

- **Short (1-3s)**: Fast response, may oscillate
- **Medium (5-10s)**: Stable, tolerates spikes (default)
- **Long (15-30s)**: Very smooth, slow to detect changes

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
adaptive_max_wait_ms: 1000
adaptive_min_wait_ms: 10
adaptive_window_secs: 10
```

**Effect**: Large batches during bursts, maintains efficiency during lulls

### Low-Latency Settings

```yaml
adaptive_max_batch_size: 50
adaptive_min_batch_size: 1
adaptive_max_wait_ms: 20
adaptive_min_wait_ms: 1
adaptive_window_secs: 3
```

**Effect**: Small batches, minimal wait times, fast adaptation

### Balanced Settings (Default)

```yaml
adaptive_max_batch_size: 1000
adaptive_min_batch_size: 10
adaptive_max_wait_ms: 100
adaptive_min_wait_ms: 1
adaptive_window_secs: 5
```

**Effect**: Good balance for variable workloads

## Troubleshooting

### Adaptive Not Working (Stays at Min or Max)

**Symptom**: Batch size never changes

**Causes:**
- `adaptive_enabled: false` - Check configuration
- Window too large - Reduce `adaptive_window_secs`
- Min/max too close - Increase range
- Traffic is consistently at one extreme (expected behavior)

### Batches Not Filling

**Symptom**: Actual batch < target batch size

**Causes:**
- Insufficient traffic (expected for low traffic)
- Wait time too short - Increase `adaptive_max_wait_ms`
- Multiple queries (each batched separately)

### High Memory Usage

**Causes:**
- Very large `adaptive_max_batch_size` - Reduce to 500-2000
- Channel buffer full (batcher slow) - Check network/server
- Large throughput window - Reduce `adaptive_window_secs`

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
- 6 parameters vs 2 for standard gRPC
- Understanding parameter interactions
- Use defaults for most scenarios

**Workload Sensitivity**:
- Works best: Gradual changes, sustained levels
- Works poorly: Extremely short bursts (<1s), rapid oscillation

**Adaptation Lag**:
- ~½ window duration delay to adapt
- 5-10s transition time for default settings
- Trade-off for stability

**Non-Deterministic**:
- Batch sizes vary based on timing
- Different runs produce different sequences
- Disable adaptive for reproducible testing

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
