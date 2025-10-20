# Adaptive gRPC Reaction

The Adaptive gRPC reaction extends the standard gRPC reaction with intelligent, throughput-based batching that automatically adjusts batch sizes and wait times based on real-time traffic patterns. It provides optimal performance across varying workloads without manual tuning.

## Purpose

The Adaptive gRPC reaction provides all the capabilities of the standard gRPC reaction while adding dynamic batching optimization:

- **Automatic Batch Optimization**: Dynamically adjusts batch size and wait time based on measured throughput
- **Traffic Classification**: Monitors message rates and classifies traffic into five levels (Idle, Low, Medium, High, Burst)
- **Latency-Throughput Balance**: Automatically balances between low latency (small batches) and high throughput (large batches)
- **No Manual Tuning Required**: Eliminates the need to manually tune batch parameters for different workload patterns
- **Graceful Adaptation**: Smoothly transitions between traffic levels using a sliding throughput window

For standard gRPC features (connection management, retry logic, metadata support), see the [standard gRPC reaction documentation](../grpc/README.md).

### Difference from Standard gRPC Reaction

| Feature | Standard gRPC | Adaptive gRPC |
|---------|---------------|---------------|
| **Batch Size** | Fixed (`batch_size` property) | Dynamic (adapts between `adaptive_min_batch_size` and `adaptive_max_batch_size`) |
| **Wait Time** | Fixed (`batch_flush_timeout_ms` property) | Dynamic (adapts between `adaptive_min_wait_ms` and `adaptive_max_wait_ms`) |
| **Throughput Monitoring** | None | Continuous monitoring with configurable window |
| **Traffic Classification** | None | Five-level classification (Idle, Low, Medium, High, Burst) |
| **Configuration Complexity** | Simple - 2 parameters | Moderate - 6 adaptive parameters |
| **Performance Tuning** | Manual - requires workload knowledge | Automatic - self-adjusting |
| **Overhead** | Minimal | Slight overhead for throughput monitoring |

### Benefits of Adaptive Batching

**1. Optimal Latency Under Low Load**
- Automatically uses small batches and minimal wait times when traffic is light
- Results are delivered quickly without unnecessary buffering
- Ideal for interactive applications and real-time alerts

**2. Maximum Throughput Under High Load**
- Scales batch sizes up during traffic bursts
- Reduces network overhead and server-side processing costs
- Optimizes resource utilization automatically

**3. Smooth Transitions**
- Gradually adapts as traffic patterns change
- Avoids performance cliffs from fixed batch sizes
- Handles variable workloads without reconfiguration

**4. Reduced Operational Complexity**
- No need to profile workloads and tune batch parameters
- Works well across development, staging, and production environments
- Automatically handles daily/seasonal traffic variations

### When to Use Adaptive vs Standard gRPC

**Use Adaptive gRPC when:**
- ✅ Traffic patterns are unpredictable or variable
- ✅ You want optimal performance without manual tuning
- ✅ Workload includes both idle periods and bursts
- ✅ You need to balance latency and throughput automatically
- ✅ Operating across multiple environments with different traffic characteristics
- ✅ You want to minimize operational overhead

**Use Standard gRPC when:**
- ✅ Traffic patterns are stable and predictable
- ✅ You have specific batch size requirements
- ✅ Workload is consistently high-throughput or consistently low-latency
- ✅ You want to minimize CPU/memory overhead
- ✅ You need deterministic batching behavior for compliance/auditing
- ✅ Simpler configuration is preferred

## Configuration Properties

The Adaptive gRPC reaction inherits all standard gRPC properties and adds adaptive-specific parameters.

### Standard gRPC Properties

See the [standard gRPC reaction documentation](../grpc/README.md#configuration-properties) for details on:
- `endpoint` - gRPC server endpoint
- `timeout_ms` - Request timeout
- `max_retries` - Retry attempts for failed requests
- `connection_retry_attempts` - Connection establishment retries
- `initial_connection_timeout_ms` - Initial connection timeout
- `metadata` - gRPC metadata headers

**Note:** The standard `batch_size` and `batch_flush_timeout_ms` properties are **ignored** when using the Adaptive gRPC reaction. Use the adaptive-specific properties instead.

### Adaptive-Specific Properties

| Property | Type | Default | Required | Description |
|----------|------|---------|----------|-------------|
| `adaptive_max_batch_size` | number | `1000` | No | Maximum number of items in a batch. Used during burst traffic. |
| `adaptive_min_batch_size` | number | `10` | No | Minimum number of items to batch. Used during idle/low traffic. |
| `adaptive_max_wait_ms` | number | `100` | No | Maximum time (milliseconds) to wait before flushing a partial batch. |
| `adaptive_min_wait_ms` | number | `1` | No | Minimum time (milliseconds) to wait for message coalescing. |
| `adaptive_window_secs` | number | `5` | No | Window size (seconds) for throughput calculation. |
| `adaptive_enabled` | boolean | `true` | No | Enable adaptive batching. If `false`, falls back to fixed batching with `min_batch_size` and `min_wait_ms`. |

#### Property Details

##### adaptive_max_batch_size
The upper limit for batch size during high-throughput scenarios. The adaptive algorithm will never exceed this size.

**Considerations:**
- Too high: May exceed protobuf message size limits (default 4MB)
- Too low: Limits throughput optimization during bursts
- Recommended: 500-2000 for most workloads

##### adaptive_min_batch_size
The lower limit for batch size during low-throughput scenarios. Ensures some batching even during idle periods.

**Considerations:**
- Too high: Increases latency during low traffic
- Too low: Increases network overhead
- Recommended: 5-20 for latency-sensitive applications, 20-50 for throughput-focused

##### adaptive_max_wait_ms
Maximum time to wait before sending a partial batch. Ensures bounded latency.

**Considerations:**
- Too high: Unacceptable latency during low traffic
- Too low: Reduces batching effectiveness
- Recommended: 50-200ms for balanced performance, 10-50ms for low-latency, 200-1000ms for throughput

##### adaptive_min_wait_ms
Minimum wait time to allow message coalescing. Prevents immediate sends when messages arrive in quick succession.

**Considerations:**
- Too high: Adds unnecessary latency
- Too low: May send many small batches
- Recommended: 1-10ms for most workloads

##### adaptive_window_secs
Time window for calculating throughput (messages per second). Larger windows provide smoother adaptation but slower response to traffic changes.

**Considerations:**
- Too large: Slow to detect traffic changes
- Too small: May oscillate between levels
- Recommended: 5-10 seconds for most workloads

##### adaptive_enabled
Disables adaptive behavior when set to `false`. Uses fixed batching with `adaptive_min_batch_size` and `adaptive_min_wait_ms`.

**Use cases:**
- Testing/comparison with standard batching
- Troubleshooting adaptive behavior
- Enforcing deterministic batch sizes

## Configuration Examples

### YAML Configuration

#### Basic Adaptive Configuration
```yaml
reactions:
  - id: "adaptive-grpc-reaction"
    reaction_type: "grpc_adaptive"
    queries:
      - "sensor-alerts"
    auto_start: true
    properties:
      # Standard gRPC properties
      endpoint: "grpc://localhost:50052"

      # Adaptive configuration uses defaults
      # adaptive_max_batch_size: 1000
      # adaptive_min_batch_size: 10
      # adaptive_max_wait_ms: 100
      # adaptive_min_wait_ms: 1
      # adaptive_window_secs: 5
```

#### High-Throughput Configuration
Optimized for maximum throughput with acceptable latency:

```yaml
reactions:
  - id: "high-throughput-adaptive"
    reaction_type: "grpc_adaptive"
    queries:
      - "high-volume-events"
    auto_start: true
    properties:
      endpoint: "grpc://event-processor.production:9090"

      # Connection settings
      timeout_ms: 15000
      max_retries: 5
      connection_retry_attempts: 10
      initial_connection_timeout_ms: 30000

      # Adaptive settings optimized for throughput
      adaptive_max_batch_size: 2000      # Large batches during bursts
      adaptive_min_batch_size: 50        # Maintain efficiency during lulls
      adaptive_max_wait_ms: 500          # Accept higher latency for better batching
      adaptive_min_wait_ms: 5            # Allow coalescing
      adaptive_window_secs: 10           # Longer window for stable adaptation

      # Authentication
      metadata:
        authorization: "Bearer ${API_TOKEN}"
        x-service-id: "drasi-core-prod"
```

#### Low-Latency Configuration
Optimized for minimal latency while maintaining reasonable throughput:

```yaml
reactions:
  - id: "low-latency-adaptive"
    reaction_type: "grpc_adaptive"
    queries:
      - "critical-alerts"
    auto_start: true
    properties:
      endpoint: "grpc://alert-service:50052"

      # Fast timeouts for quick response
      timeout_ms: 3000
      max_retries: 2

      # Adaptive settings optimized for latency
      adaptive_max_batch_size: 100       # Smaller max to limit wait time
      adaptive_min_batch_size: 5         # Very small for immediate sends
      adaptive_max_wait_ms: 50           # Quick flush for low latency
      adaptive_min_wait_ms: 1            # Minimal wait
      adaptive_window_secs: 3            # Fast adaptation to traffic changes
```

#### Adaptive Disabled (Fallback to Fixed Batching)
```yaml
reactions:
  - id: "fixed-batch-adaptive"
    reaction_type: "grpc_adaptive"
    queries:
      - "audit-events"
    auto_start: true
    properties:
      endpoint: "grpc://audit-service:50052"

      # Disable adaptive behavior
      adaptive_enabled: false

      # These become fixed values
      adaptive_min_batch_size: 100       # Fixed batch size
      adaptive_min_wait_ms: 1000         # Fixed wait time
```

### JSON Configuration

#### Basic Adaptive Configuration
```json
{
  "reactions": [
    {
      "id": "adaptive-grpc-reaction",
      "reaction_type": "grpc_adaptive",
      "queries": ["sensor-alerts"],
      "auto_start": true,
      "properties": {
        "endpoint": "grpc://localhost:50052"
      }
    }
  ]
}
```

#### High-Throughput Configuration
```json
{
  "reactions": [
    {
      "id": "high-throughput-adaptive",
      "reaction_type": "grpc_adaptive",
      "queries": ["high-volume-events"],
      "auto_start": true,
      "properties": {
        "endpoint": "grpc://event-processor.production:9090",
        "timeout_ms": 15000,
        "max_retries": 5,
        "connection_retry_attempts": 10,
        "initial_connection_timeout_ms": 30000,
        "adaptive_max_batch_size": 2000,
        "adaptive_min_batch_size": 50,
        "adaptive_max_wait_ms": 500,
        "adaptive_min_wait_ms": 5,
        "adaptive_window_secs": 10,
        "metadata": {
          "authorization": "Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...",
          "x-service-id": "drasi-core-prod"
        }
      }
    }
  ]
}
```

#### Low-Latency Configuration
```json
{
  "reactions": [
    {
      "id": "low-latency-adaptive",
      "reaction_type": "grpc_adaptive",
      "queries": ["critical-alerts"],
      "auto_start": true,
      "properties": {
        "endpoint": "grpc://alert-service:50052",
        "timeout_ms": 3000,
        "max_retries": 2,
        "adaptive_max_batch_size": 100,
        "adaptive_min_batch_size": 5,
        "adaptive_max_wait_ms": 50,
        "adaptive_min_wait_ms": 1,
        "adaptive_window_secs": 3
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Basic Usage

```rust
use drasi_server_core::api::{Reaction, Properties};

// Create an adaptive gRPC reaction with defaults
let reaction = Reaction::grpc_adaptive("my-adaptive-reaction")
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

// Create a high-throughput adaptive reaction
let reaction = Reaction::grpc_adaptive("high-throughput-reaction")
    .subscribe_to("event-stream")
    .subscribe_to("metric-stream")
    .with_properties(
        Properties::new()
            // Standard gRPC settings
            .with_string("endpoint", "grpc://event-processor:9090")
            .with_number("timeout_ms", 15000)
            .with_number("max_retries", 5)
            .with_number("connection_retry_attempts", 10)
            .with_number("initial_connection_timeout_ms", 30000)

            // Adaptive batching settings
            .with_number("adaptive_max_batch_size", 2000)
            .with_number("adaptive_min_batch_size", 50)
            .with_number("adaptive_max_wait_ms", 500)
            .with_number("adaptive_min_wait_ms", 5)
            .with_number("adaptive_window_secs", 10)

            // Metadata
            .with("metadata", json!({
                "authorization": "Bearer token123",
                "x-tenant-id": "prod-456"
            }))
    )
    .build();
```

### Low-Latency Configuration

```rust
use drasi_server_core::api::{Reaction, Properties};

// Create a low-latency adaptive reaction
let reaction = Reaction::grpc_adaptive("realtime-alerts")
    .subscribe_to("critical-events")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "grpc://alert-service:50052")
            .with_number("timeout_ms", 3000)
            .with_number("max_retries", 2)

            // Low-latency adaptive settings
            .with_number("adaptive_max_batch_size", 100)
            .with_number("adaptive_min_batch_size", 5)
            .with_number("adaptive_max_wait_ms", 50)
            .with_number("adaptive_min_wait_ms", 1)
            .with_number("adaptive_window_secs", 3)
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

    // The adaptive gRPC reaction will now:
    // 1. Monitor throughput continuously
    // 2. Adjust batch sizes based on traffic
    // 3. Send results to the configured endpoint

    // Keep the application running
    tokio::signal::ctrl_c().await?;

    // Graceful shutdown - flushes final batches
    core.stop().await?;

    Ok(())
}
```

### Disabling Adaptive Behavior

```rust
use drasi_server_core::api::{Reaction, Properties};

// Use fixed batching (adaptive disabled)
let reaction = Reaction::grpc_adaptive("fixed-batch-reaction")
    .subscribe_to("audit-events")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "grpc://audit-service:50052")

            // Disable adaptive behavior
            .with_bool("adaptive_enabled", false)

            // These become fixed values
            .with_number("adaptive_min_batch_size", 100)
            .with_number("adaptive_min_wait_ms", 1000)
    )
    .build();
```

## Input Data Format

The Adaptive gRPC reaction receives the same input format as the standard gRPC reaction. See the [standard gRPC reaction documentation](../grpc/README.md#input-data-format) for details on the `QueryResult` structure and result item formats (ADD, UPDATE, DELETE operations).

## Output Data Format

The Adaptive gRPC reaction sends data using the same Protocol Buffers format as the standard gRPC reaction. See the [standard gRPC reaction documentation](../grpc/README.md#output-data-format) for details on:
- `ProcessResultsRequest` structure
- `QueryResult` protobuf message
- `QueryResultItem` structure
- Field descriptions and examples

For the complete protobuf schema, refer to `/Users/allenjones/dev/agentofreality/drasi/drasi-core/server-core/proto/drasi/v1/reaction.proto`.

## Adaptive Batching Behavior

The Adaptive gRPC reaction implements a sophisticated throughput-based batching algorithm that automatically optimizes performance.

### How the Adaptive Algorithm Works

The adaptive algorithm operates in a continuous cycle:

1. **Receive First Message**: Block waiting for at least one message to arrive
2. **Calculate Throughput**: Measure messages per second over the configured window
3. **Classify Traffic Level**: Determine current traffic level (Idle, Low, Medium, High, Burst)
4. **Adjust Parameters**: Update `current_batch_size` and `current_wait_time` based on traffic level
5. **Collect Batch**: Gather messages up to `current_batch_size` or until `current_wait_time` expires
6. **Send Batch**: Forward batch to gRPC endpoint
7. **Record Metrics**: Update throughput history with batch size
8. **Repeat**: Go to step 1

### Throughput Measurement

The `ThroughputMonitor` component tracks message rates:

- **Sliding Window**: Maintains a configurable time window (default: 5 seconds)
- **Event Recording**: Records each batch with timestamp and size
- **Automatic Cleanup**: Removes events older than the window
- **Rate Calculation**: `messages_per_second = total_messages_in_window / window_duration`

### Traffic Level Classification

Messages per second determine the traffic level:

| Level | Messages/Second | Batch Size Adjustment | Wait Time |
|-------|----------------|----------------------|-----------|
| **Idle** | < 1 | `min_batch_size` | `min_wait_time` (1ms) |
| **Low** | 1 - 100 | `min_batch_size × 2` | 1ms |
| **Medium** | 100 - 1,000 | 25% between min and max | 10ms |
| **High** | 1,000 - 10,000 | 50% between min and max | 25ms |
| **Burst** | > 10,000 | `max_batch_size` | 50ms |

### Batch Size Adjustment Formula

For Medium traffic:
```
batch_size = ((max_batch_size - min_batch_size) / 4 + min_batch_size).min(max_batch_size)
```

For High traffic:
```
batch_size = ((max_batch_size - min_batch_size) / 2 + min_batch_size).min(max_batch_size)
```

Example with defaults (min=10, max=1000):
- Medium: `(990/4 + 10) = 257` messages
- High: `(990/2 + 10) = 505` messages

### Wait Time Constraints

The algorithm ensures wait times stay within bounds:
```
current_wait_time = calculated_wait_time
    .max(adaptive_min_wait_ms)
    .min(adaptive_max_wait_ms)
```

### Burst Detection

The batcher includes special burst detection logic:
- Checks for immediately available messages using `try_recv()`
- Estimates pending messages based on throughput level
- If many messages are pending (> `current_batch_size × 2`), fills batch completely
- Prevents partial sends during burst traffic

### Adaptive Window Behavior

The throughput window acts as a smoothing buffer:

**Short Window (1-3 seconds):**
- ✅ Fast response to traffic changes
- ❌ May oscillate between levels
- Best for: Variable workloads with quick transitions

**Medium Window (5-10 seconds, default):**
- ✅ Stable adaptation
- ✅ Tolerates brief traffic spikes
- Best for: Most production workloads

**Long Window (15-30 seconds):**
- ✅ Very smooth adaptation
- ❌ Slow to detect traffic changes
- Best for: Steady workloads with gradual changes

### Min/Max Batch Size Constraints

The adaptive algorithm never exceeds configured limits:

```rust
// Always enforced
current_batch_size >= adaptive_min_batch_size
current_batch_size <= adaptive_max_batch_size
```

**Effects of Constraints:**

**Narrow Range (e.g., min=50, max=100):**
- Limited adaptation range
- Predictable batch sizes
- Less responsive to traffic extremes

**Wide Range (e.g., min=5, max=2000):**
- Maximum adaptation flexibility
- Optimal for variable workloads
- May have large swings in batch size

### Min/Max Wait Time Effects

Wait times directly impact latency:

**Short Max Wait (10-50ms):**
- Low latency even during idle periods
- More frequent sends
- Higher network overhead

**Long Max Wait (200-1000ms):**
- Better batching during low traffic
- Higher latency during idle periods
- Reduced network overhead

**Typical Latency Characteristics:**

| Traffic Level | Wait Time | Expected Latency |
|---------------|-----------|------------------|
| Idle | min_wait_ms (1ms) | 1-10ms |
| Low | 1ms | 1-10ms |
| Medium | 10ms | 10-20ms |
| High | 25ms | 25-50ms |
| Burst | 50ms | 10-50ms (often immediate when batch fills) |

### Metrics That Influence Adaptation

The following metrics drive adaptive decisions:

1. **Messages Per Second**: Primary metric for traffic classification
2. **Batch Size History**: Recorded for throughput calculation
3. **Timestamps**: Used for window management and rate calculation
4. **Pending Messages Estimate**: Influences burst detection behavior

### Example Adaptation Sequence

```
Time  | Traffic | Msgs/Sec | Level  | Batch Size | Wait Time | Action
------|---------|----------|--------|------------|-----------|------------------
0s    | Start   | 0        | Idle   | 10         | 1ms       | Initialize
5s    | Ramp up | 50       | Low    | 20         | 1ms       | Small batches
15s   | Steady  | 500      | Medium | 257        | 10ms      | Moderate batches
30s   | Burst   | 15000    | Burst  | 1000       | 50ms      | Large batches
45s   | Decline | 200      | Medium | 257        | 10ms      | Scale down
60s   | Idle    | 0.5      | Idle   | 10         | 1ms       | Minimal latency
```

## Two-Task Architecture

The Adaptive gRPC reaction uses a two-task architecture for separation of concerns and efficient processing.

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                 Adaptive gRPC Reaction                      │
│                                                              │
│  ┌──────────────────┐         ┌──────────────────────┐     │
│  │  Receiver Task   │ ─────> │   Batcher Task       │     │
│  │                  │  mpsc   │                      │     │
│  │ - Receives query │ channel │ - AdaptiveBatcher    │     │
│  │   results        │         │ - Monitors throughput│     │
│  │ - Converts to    │         │ - Adjusts parameters │     │
│  │   protobuf       │         │ - Sends batches      │─────>
│  │ - Groups by      │         │   to gRPC server     │     │
│  │   query_id       │         │                      │     │
│  └──────────────────┘         └──────────────────────┘     │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

### Receiver Task Responsibilities

**Purpose**: Transform and forward query results to the adaptive batcher

**Operations**:
1. Receives `QueryResult` messages from internal query result channel
2. Checks reaction status (exits if not Running)
3. Skips empty results
4. Tracks current query ID for batching by query
5. Converts JSON result items to protobuf format (`ProtoQueryResultItem`)
6. Batches items up to 100 before forwarding (pre-batching optimization)
7. Sends `(query_id, Vec<ProtoQueryResultItem>)` tuples to batcher task
8. Flushes remaining items on shutdown
9. Closes batcher channel when complete

**Key Code Locations**:
- Implementation: Lines 346-427 in `grpc_adaptive.rs`
- Channel creation: Line 218 (`mpsc::channel(1000)`)
- Protobuf conversion: Lines 384-399

**Error Handling**:
- Logs error and exits if send to batcher fails
- Gracefully handles channel closure

### Batcher Task Responsibilities

**Purpose**: Apply adaptive batching and send to gRPC endpoint

**Operations**:
1. Creates `AdaptiveBatcher` with configured parameters
2. Waits for next adaptive batch from `AdaptiveBatcher::next_batch()`
3. Establishes gRPC client connection (lazy, on first batch)
4. Groups batch items by query_id
5. Sends each query's batch to gRPC endpoint with retry logic
6. Tracks metrics (successful/failed sends)
7. Handles connection failures with exponential backoff
8. Logs performance metrics every 100 successful sends
9. Completes when receiver channel closes

**Key Code Locations**:
- Implementation: Lines 221-343 in `grpc_adaptive.rs`
- AdaptiveBatcher usage: Line 222 (`AdaptiveBatcher::new`)
- Batch retrieval: Line 233 (`batcher.next_batch().await`)
- Connection management: Lines 241-265
- Batch sending: Lines 268-336

**Connection Management**:
- Lazy connection establishment (only when first batch arrives)
- Connection recreation on failures
- Exponential backoff: `100ms * 2^failures` (capped at reasonable limits)

**Metrics Tracking**:
- `successful_sends`: Count of successful batch transmissions
- `failed_sends`: Count of failed batch transmissions
- `consecutive_failures`: Current failure streak (resets on success)

### Task Communication

**Channel Configuration**:
- Type: `mpsc::channel<(String, Vec<ProtoQueryResultItem>)>`
- Buffer size: 1000 messages
- Direction: Receiver task → Batcher task

**Message Format**:
```rust
(
    query_id: String,                        // Which query produced these results
    items: Vec<ProtoQueryResultItem>         // Batch of result items (pre-converted to protobuf)
)
```

**Channel Lifecycle**:
1. Created in `run_internal()` (line 218)
2. Sender (`batch_tx`) owned by receiver task
3. Receiver (`batch_rx`) moved into `AdaptiveBatcher`
4. Receiver task drops sender when complete, signaling batcher to finish
5. Batcher returns `None` from `next_batch()` when channel closes

### Channel Buffer Sizes

**Internal Channel (Receiver → Batcher)**:
- **Size**: 1000 messages
- **Purpose**: Buffers pre-processed batches during burst traffic
- **Pressure**: Provides backpressure if batcher can't keep up with receiver

**AdaptiveBatcher Internal Buffering**:
- **Type**: `mpsc::Receiver<T>` (size determined by caller, 1000 in this case)
- **Behavior**: Messages accumulate in channel buffer until consumed by `next_batch()`
- **Burst Handling**: Large buffer allows spike absorption

**Implications**:
- 1000-message buffer can hold ~100 pre-batches (if receiver sends 10-item batches)
- Memory usage: `1000 × sizeof((String, Vec<ProtoQueryResultItem>))`
- Backpressure prevents unbounded memory growth

### Task Coordination

**Startup Sequence**:
1. `run_internal()` called from `start()` method
2. Creates mpsc channel for task communication
3. Spawns batcher task with receiver end of channel
4. Spawns receiver task with sender end of channel
5. Both tasks run concurrently

**Shutdown Sequence**:
1. Reaction `stop()` called or status changes to non-Running
2. Receiver task exits loop (line 351-357)
3. Receiver task sends remaining items (lines 416-418)
4. Receiver task drops `batch_tx`, closing channel
5. Batcher task receives `None` from `next_batch()`, exits
6. Receiver task awaits batcher completion (line 424)
7. Both tasks complete

**Error Propagation**:
- Tasks are independent (spawned with `tokio::spawn`)
- Errors logged but don't crash other task
- Channel closure naturally propagates shutdown
- Final status determined by receiver task completion

## Performance Tuning

### Setting Adaptive Parameters for Different Workloads

#### High-Throughput Scenarios

**Goal**: Maximize messages per second, accept higher latency

**Configuration**:
```yaml
properties:
  adaptive_max_batch_size: 2000
  adaptive_min_batch_size: 100
  adaptive_max_wait_ms: 1000
  adaptive_min_wait_ms: 10
  adaptive_window_secs: 10
```

**Rationale**:
- Large max batch (2000): Maximizes network efficiency
- Higher min batch (100): Maintains efficiency during lulls
- Long max wait (1000ms): Allows batches to fill during medium traffic
- Longer window (10s): Stable adaptation, avoids oscillation

**Expected Behavior**:
- Burst traffic: 2000-item batches every 50-100ms
- Medium traffic: 500-1000 item batches every 200-500ms
- Low traffic: 100-200 item batches every second

**Monitoring**:
```
Adapted batching parameters - Level: Burst, Rate: 25000.0 msgs/sec, Batch: 2000, Wait: 50ms
```

#### Low-Latency Scenarios

**Goal**: Minimize time from result to delivery

**Configuration**:
```yaml
properties:
  adaptive_max_batch_size: 50
  adaptive_min_batch_size: 1
  adaptive_max_wait_ms: 20
  adaptive_min_wait_ms: 1
  adaptive_window_secs: 3
```

**Rationale**:
- Small max batch (50): Limits wait time even during bursts
- Tiny min batch (1): Immediate sends during idle
- Short max wait (20ms): Bounded latency
- Short window (3s): Fast response to traffic changes

**Expected Behavior**:
- Idle traffic: Single-message batches, <5ms latency
- Low traffic: 2-10 item batches, <20ms latency
- Burst traffic: 50-item batches, ~20ms latency

**Monitoring**:
```
Adapted batching parameters - Level: Low, Rate: 15.0 msgs/sec, Batch: 2, Wait: 1ms
```

#### Variable Load Scenarios

**Goal**: Balance throughput and latency, handle unpredictable patterns

**Configuration (Recommended Defaults)**:
```yaml
properties:
  adaptive_max_batch_size: 1000
  adaptive_min_batch_size: 10
  adaptive_max_wait_ms: 100
  adaptive_min_wait_ms: 1
  adaptive_window_secs: 5
```

**Rationale**:
- Wide adaptation range (10-1000): Handles extremes
- Moderate max wait (100ms): Acceptable latency for most use cases
- Medium window (5s): Balances stability and responsiveness

**Expected Behavior**:
- Automatically optimizes for current traffic level
- Smooth transitions between idle and burst
- Good performance across diverse workloads

### Tuning Guidelines by Metric

#### If Latency is Too High

**Symptoms**:
- Messages delayed during low traffic
- P99 latency > 500ms

**Solutions**:
1. Reduce `adaptive_max_wait_ms`: `100ms → 50ms → 20ms`
2. Reduce `adaptive_min_batch_size`: `10 → 5 → 1`
3. Reduce `adaptive_max_batch_size`: `1000 → 500 → 100`

#### If Throughput is Too Low

**Symptoms**:
- Cannot handle traffic bursts
- Messages backing up in queues
- CPU/Network underutilized

**Solutions**:
1. Increase `adaptive_max_batch_size`: `1000 → 2000 → 5000`
2. Increase `adaptive_max_wait_ms`: `100ms → 500ms → 1000ms`
3. Increase `adaptive_window_secs`: `5s → 10s → 15s`

#### If Adaptation is Unstable

**Symptoms**:
- Frequent oscillation between traffic levels
- Log spam with parameter changes

**Solutions**:
1. Increase `adaptive_window_secs`: Smooth out spikes
2. Narrow the batch size range: Reduce difference between min and max
3. Increase `adaptive_min_wait_ms`: Prevent overly aggressive sends

### How to Monitor Adaptive Behavior

#### Log Messages to Watch

**Throughput Classification**:
```
TRACE: Adapted batching parameters - Level: High, Rate: 5432.1 msgs/sec, Batch: 505, Wait: 25ms
```
- Shows current traffic level and adaptive decisions
- Verify level matches expected traffic

**Batch Collection**:
```
DEBUG: Adaptive batch collected - Size: 487, Target: 505, Wait: 25ms, Level: High
```
- Compare actual batch size to target
- Check if batches are filling close to target

**Performance Metrics**:
```
INFO: Adaptive metrics - Successful: 500, Failed: 3
```
- Monitor success/failure ratio
- High failures indicate connection or server issues

#### Enable Debug Logging

```bash
RUST_LOG=drasi_server_core::reactions::grpc_adaptive=debug,drasi_server_core::utils::adaptive_batcher=trace cargo run
```

Or in configuration:
```rust
env_logger::Builder::from_env(
    env_logger::Env::default().default_filter_or(
        "drasi_server_core::reactions::grpc_adaptive=debug"
    )
).init();
```

#### Key Metrics to Track

1. **Messages Per Second**: Current throughput from logs
2. **Traffic Level**: Should match workload (Idle/Low/Medium/High/Burst)
3. **Batch Fill Ratio**: `actual_size / target_size` (should be > 0.7 for efficiency)
4. **Success Rate**: `successful_sends / (successful_sends + failed_sends)` (should be > 0.99)

### Performance Optimization Checklist

**For Maximum Throughput**:
- [ ] Set `adaptive_max_batch_size` ≥ 1000
- [ ] Set `adaptive_min_batch_size` ≥ 50
- [ ] Set `adaptive_max_wait_ms` ≥ 200ms
- [ ] Set `adaptive_window_secs` ≥ 10s
- [ ] Increase `timeout_ms` to handle large batches
- [ ] Monitor batch fill ratio (should approach 1.0 during bursts)

**For Minimum Latency**:
- [ ] Set `adaptive_max_batch_size` ≤ 100
- [ ] Set `adaptive_min_batch_size` ≤ 10
- [ ] Set `adaptive_max_wait_ms` ≤ 50ms
- [ ] Set `adaptive_window_secs` ≤ 5s
- [ ] Monitor P99 latency (should be < 100ms)

**For Stability**:
- [ ] Set `adaptive_window_secs` ≥ 5s
- [ ] Avoid extreme min/max ranges
- [ ] Test with representative traffic patterns
- [ ] Monitor for oscillation in logs

## Troubleshooting

### Standard gRPC Issues

The Adaptive gRPC reaction inherits all troubleshooting scenarios from the standard gRPC reaction. See the [standard gRPC reaction troubleshooting section](../grpc/README.md#troubleshooting) for:
- Connection errors (GoAway frames, Unavailable service, etc.)
- Timeout issues (DeadlineExceeded, Connection timeout)
- Retry exhaustion
- Metadata configuration issues
- TLS/mTLS configuration
- Performance issues
- Debugging tips

### Adaptive-Specific Issues

#### Adaptive Batching Not Working (Stays at Min or Max)

**Symptom**:
```
DEBUG: Adaptive batch collected - Size: 10, Target: 10, Wait: 1ms, Level: Idle
# OR
DEBUG: Adaptive batch collected - Size: 1000, Target: 1000, Wait: 50ms, Level: Idle
```
Batch size never changes, always at configured min or max.

**Possible Causes**:

1. **Adaptive disabled**:
```yaml
# Check configuration
properties:
  adaptive_enabled: false  # ← This disables adaptation
```
**Solution**: Set `adaptive_enabled: true` or remove the property (defaults to true)

2. **Throughput window too large**:
```yaml
properties:
  adaptive_window_secs: 60  # Very large window
```
**Solution**: Reduce window size to 5-10 seconds for responsive adaptation

3. **Extreme traffic pattern**:
- Consistently at idle level → Will stay at min
- Consistently at burst level → Will stay at max

**Solution**: This is expected behavior. Verify with logs:
```
TRACE: Adapted batching parameters - Level: Idle, Rate: 0.5 msgs/sec, Batch: 10, Wait: 1ms
```

4. **Min/Max too close**:
```yaml
properties:
  adaptive_min_batch_size: 90
  adaptive_max_batch_size: 100  # Only 10 message range
```
**Solution**: Increase range for more adaptation headroom

#### Throughput Calculation Issues

**Symptom**:
```
TRACE: Adapted batching parameters - Level: Idle, Rate: 0.0 msgs/sec, Batch: 10, Wait: 1ms
```
Throughput shows as 0 despite active traffic.

**Possible Causes**:

1. **First batch after startup**:
   - Throughput monitor starts empty
   - First batch always shows Idle until history accumulates

**Solution**: This is normal. Check subsequent batches:
```
TRACE: Adapted batching parameters - Level: Medium, Rate: 250.0 msgs/sec, Batch: 257, Wait: 10ms
```

2. **Window size mismatch**:
   - Very short window may not capture enough history

**Solution**: Ensure window ≥ 3 seconds

3. **Long idle periods**:
   - Events expire from window during idle
   - Next batch will show Idle level

**Solution**: Expected behavior. Adaptation will resume when traffic resumes.

#### Parameter Tuning Problems

**Symptom**: Performance doesn't improve despite adjusting parameters

**Debugging Steps**:

1. **Verify parameters are applied**:
```bash
# Check logs at startup
grep "Adaptive gRPC reaction starting" logs.txt
```

2. **Check effective ranges**:
```yaml
# Bad: No room to adapt
adaptive_min_batch_size: 100
adaptive_max_batch_size: 100

# Good: Wide range
adaptive_min_batch_size: 10
adaptive_max_batch_size: 1000
```

3. **Monitor actual behavior**:
```bash
# Watch adaptation in real-time
tail -f logs.txt | grep "Adapted batching parameters"
```

4. **Verify traffic matches expectations**:
```bash
# Check throughput levels align with workload
grep "msgs/sec" logs.txt | tail -20
```

**Common Mistakes**:
- Setting `adaptive_max_wait_ms` < `adaptive_min_wait_ms` (silently clamped)
- Setting min > max for batch sizes (may cause issues)
- Expecting immediate adaptation (requires window time to accumulate)

#### Batches Not Filling to Target Size

**Symptom**:
```
DEBUG: Adaptive batch collected - Size: 45, Target: 257, Wait: 10ms, Level: Medium
```
Actual batch size much smaller than target.

**Possible Causes**:

1. **Insufficient traffic**:
   - Not enough messages arriving during wait period
   - Expected behavior for bursty traffic

**Solution**: Verify with throughput logs. If traffic is genuinely low, consider reducing `adaptive_min_batch_size`.

2. **Wait time too short**:
```yaml
properties:
  adaptive_max_wait_ms: 10  # Very short
```
**Solution**: Increase to allow batch filling:
```yaml
properties:
  adaptive_max_wait_ms: 100  # More time to accumulate
```

3. **Query ID changes**:
   - Receiver task flushes batch when query_id changes (line 366-379)
   - Each query gets separate batches

**Solution**: Expected behavior for multi-query reactions. Each query has independent batching.

4. **Pre-batching limit**:
   - Receiver task sends to batcher when reaching 100 items (line 402)
   - This is an optimization to prevent unbounded buffering

**Solution**: Expected behavior. Batcher will receive 100-item chunks which may be smaller than target.

#### High Memory Usage

**Symptom**: Reaction consuming excessive memory during operation

**Possible Causes**:

1. **Large batch sizes**:
```yaml
properties:
  adaptive_max_batch_size: 10000  # Very large
```
Memory usage = `batch_size × average_item_size × number_of_batches_in_flight`

**Solution**: Reduce `adaptive_max_batch_size` to reasonable level (500-2000)

2. **Channel buffer full**:
   - 1000-message channel buffer (line 218)
   - If batcher is slow, buffer fills with pending batches

**Solution**:
- Investigate why batcher is slow (network, server issues)
- Monitor send success rate

3. **Throughput monitor history**:
   - Stores batch history for window duration
   - Large window + high frequency = more events stored

**Solution**: Reduce `adaptive_window_secs` if memory constrained

#### Unexpected Latency Spikes

**Symptom**: Occasional high-latency deliveries despite low-latency configuration

**Debugging**:

1. **Check wait times during spikes**:
```bash
grep "Adaptive batch collected" logs.txt | grep "Wait: [5-9][0-9]ms"
```

2. **Correlate with traffic levels**:
```bash
grep "Level: High\|Level: Burst" logs.txt
```

**Possible Causes**:

1. **Burst detection triggering**:
   - Batcher detects many pending messages
   - Waits to fill large batch completely
   - Line 234-243 in `adaptive_batcher.rs`

**Solution**: This is expected during bursts. If problematic:
```yaml
properties:
  adaptive_max_batch_size: 200  # Smaller max reduces max latency
```

2. **Traffic level transitions**:
   - Adaptation lags behind actual traffic change by window duration
   - High wait time persists during transition to low traffic

**Solution**: Reduce `adaptive_window_secs` for faster response

3. **Network/server latency**:
   - Adaptive algorithm only controls batching
   - Network and server-side processing add latency

**Solution**: Check gRPC send logs for transmission time

### Diagnostic Commands

#### Check Current Configuration
```bash
# Extract adaptive config from logs
grep "Adaptive gRPC reaction starting" logs.txt -A 10
```

#### Monitor Adaptation in Real-Time
```bash
# Watch parameter adjustments
tail -f logs.txt | grep "Adapted batching\|Adaptive batch collected"
```

#### Analyze Throughput Pattern
```bash
# Extract throughput over time
grep "msgs/sec" logs.txt | awk '{print $NF}' | sed 's/,//' > throughput.dat
# Plot with gnuplot or spreadsheet
```

#### Check Success Rate
```bash
# Calculate success rate
grep "Adaptive metrics" logs.txt | tail -1
# Example output: "Adaptive metrics - Successful: 5000, Failed: 12"
# Success rate = 5000 / (5000 + 12) = 99.76%
```

## Limitations

### Standard gRPC Limitations

The Adaptive gRPC reaction inherits all limitations from the standard gRPC reaction. See the [standard gRPC reaction limitations section](../grpc/README.md#limitations) for:
- Protobuf message size limits (4MB default)
- Connection pooling (single connection per reaction)
- Memory usage considerations
- Network bandwidth constraints
- CPU usage for serialization
- Eventual consistency behavior
- Data loss scenarios
- Version compatibility
- TLS/security limitations
- No built-in observability

### Adaptive-Specific Limitations

#### Algorithm Overhead

**CPU Overhead**:
- Throughput monitoring requires timestamp tracking and calculations
- Per-batch overhead: ~0.1-1% CPU for monitoring logic
- Negligible impact for most workloads
- More noticeable under extreme throughput (>100K msgs/sec)

**Memory Overhead**:
- Throughput monitor stores batch history within window
- Memory = `number_of_batches_in_window × (timestamp + batch_size)`
- Typical: 5-second window × 100 batches/sec = 500 events × ~24 bytes = ~12KB
- Insignificant compared to batch buffering

**Comparison to Standard gRPC**:
- Standard: Fixed-size batch logic (minimal overhead)
- Adaptive: +0.1-1% CPU, +10-50KB memory
- Trade-off: Overhead for automatic optimization

#### Tuning Complexity

**Configuration Parameters**: 6 adaptive-specific parameters vs 2 for standard gRPC
- `adaptive_max_batch_size`
- `adaptive_min_batch_size`
- `adaptive_max_wait_ms`
- `adaptive_min_wait_ms`
- `adaptive_window_secs`
- `adaptive_enabled`

**Challenges**:
- Understanding parameter interactions
- Finding optimal values for specific workloads
- Debugging unexpected adaptive behavior

**Mitigation**:
- Use defaults for most scenarios
- Only tune for extreme workloads
- Enable debug logging during tuning
- Document configuration decisions

#### Sensitivity to Workload Patterns

**Works Best With**:
- ✅ Gradual traffic changes
- ✅ Sustained traffic levels
- ✅ Periodic patterns (daily cycles)

**Works Poorly With**:
- ❌ Extremely short bursts (<1 second)
- ❌ Rapid oscillation between idle and burst
- ❌ Multi-modal distributions (simultaneous high and low traffic)

**Example Problematic Pattern**:
```
Time:  0s    1s    2s    3s    4s    5s    6s
Load:  Idle  Burst Idle  Burst Idle  Burst Idle
       ↓     ↓     ↓     ↓     ↓     ↓     ↓
Effect: Cannot adapt quickly enough
Result: Suboptimal batching for both levels
```

**Solution for Problematic Patterns**:
- Use standard gRPC reaction with tuned fixed parameters
- Reduce `adaptive_window_secs` to minimum (1-3s)
- Accept that adaptation may lag traffic changes

#### Throughput Window Lag

**Inherent Delay**: Adaptation lags traffic changes by ~1/2 window duration

**Example**:
```
Window: 10 seconds
Traffic at t=0: Idle (10 msgs/sec)
Traffic at t=5: Burst (20,000 msgs/sec)
Adaptation at t=5: Still Medium (average over 10s window)
Adaptation at t=10: High (window now shows 5s burst)
Adaptation at t=15: Burst (window mostly burst traffic)
```

**Impact**:
- 5-10 second delay before full adaptation to new traffic level
- May use suboptimal batch sizes during transition
- Trade-off: Slower adaptation vs stability

**Mitigation**:
- Reduce window for faster response (increases oscillation risk)
- Accept lag as necessary for stable adaptation
- Use fixed batching if consistent behavior required

#### Memory Overhead vs Standard gRPC

**Standard gRPC Memory**:
- Single batch buffer: `batch_size × average_item_size`
- Minimal metadata overhead

**Adaptive gRPC Memory**:
- Batch buffer: `adaptive_max_batch_size × average_item_size` (potentially larger)
- Throughput history: `~10-50KB` for monitoring
- Channel buffer: 1000 messages (shared with standard)

**Comparison**:
```
Standard (batch_size=100):
  Batch: 100 × 1KB = 100KB
  Total: ~100KB

Adaptive (max=1000):
  Batch: 1000 × 1KB = 1000KB
  Monitor: ~20KB
  Total: ~1020KB
```

**Recommendation**:
- For memory-constrained environments, use standard gRPC or reduce `adaptive_max_batch_size`
- Memory difference is typically negligible (<10MB per reaction)

#### No Guaranteed Batch Size

**Behavior**: Actual batch sizes vary based on traffic

**Implications**:
- Cannot guarantee batch size for downstream systems
- Some batches may be very small (1-10 items)
- Some batches may be very large (up to max)
- Downstream systems must handle variable batch sizes

**Issues for**:
- Systems expecting fixed batch sizes
- Rate limiters based on batch count
- Billing systems charging per batch

**Solution**:
- Use standard gRPC if fixed batch size required
- Design downstream systems to handle variable batching
- Set narrow min/max range to constrain variation

#### Limited Metrics Exposure

**Current Monitoring**:
- Log messages only (no structured metrics)
- No Prometheus/OpenTelemetry export
- Success/failure counts logged periodically
- No histograms or percentiles

**Missing Metrics**:
- P50/P95/P99 latency
- Batch size distribution
- Traffic level distribution over time
- Throughput trend visualization
- Adaptation frequency

**Workaround**:
- Parse logs for metrics extraction
- Enable debug logging and post-process
- Monitor at gRPC server side

#### Determinism and Reproducibility

**Non-Deterministic Behavior**:
- Batch sizes vary based on timing
- Throughput classification depends on message arrival patterns
- Different runs produce different batching sequences

**Challenges for**:
- Unit testing with expected batch counts
- Performance benchmarking (variable results)
- Compliance/audit requirements (reproducible processing)

**Solution**:
- Disable adaptive mode for deterministic testing:
```yaml
properties:
  adaptive_enabled: false
```
- Focus testing on end results, not batching behavior
- Document that batching is optimization, not specification

### When to Avoid Adaptive gRPC

Consider using standard gRPC reaction instead if:
- ❌ You need deterministic, reproducible batch sizes
- ❌ Memory is extremely constrained
- ❌ Workload is consistently high or consistently low (no variation)
- ❌ Downstream systems require fixed batch sizes
- ❌ Simplicity is more important than optimization
- ❌ You need guaranteed latency bounds (use fixed small batches)
- ❌ Traffic patterns change faster than adaptive window can track
