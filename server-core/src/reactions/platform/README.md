# Platform Reaction

## Purpose

The Platform Reaction publishes continuous query results to Redis Streams in CloudEvents format, serving as the integration bridge between DrasiServerCore and the Drasi Platform ecosystem.

**Key Features:**
- Publishes query results to Redis Streams with CloudEvents 1.0 format
- Per-query topic isolation (`{query-id}-results`)
- Sequence numbering for result ordering
- Optional control events for lifecycle management
- Performance tracking metadata for distributed tracing
- Batch processing support for high-throughput scenarios

## Architecture

### Data Flow
```
Sources → Queries → Platform Reaction → Redis Streams → Dapr PubSub → Downstream Reactions
```

### Why Redis Streams?
- Message persistence and durability
- Consumer groups with exactly-once delivery
- Messages remain until acknowledged
- Parallel processing with multiple consumers
- Stream length management prevents unbounded growth

### Integration Role
1. **Format Translation**: Converts DrasiServerCore results to CloudEvents format
2. **Persistence Layer**: Durably stores results in Redis Streams
3. **Topic Isolation**: Each query publishes to its own stream
4. **Observability**: Embeds profiling metadata for end-to-end tracking
5. **Lifecycle Management**: Emits control events (Running, Stopped, BootstrapStarted, BootstrapCompleted)
6. **Dapr Integration**: Produces CloudEvents for Dapr pubsub routing

## Configuration

### Required Properties

| Property | Type | Description |
|----------|------|-------------|
| `redis_url` | String | Redis connection URL (e.g., `redis://localhost:6379`) |

### Optional Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `pubsub_name` | String | `"drasi-pubsub"` | Dapr pubsub component name in CloudEvent |
| `source_name` | String | `"drasi-core"` | CloudEvent source identifier |
| `max_stream_length` | u64 | None | Max entries per stream (approximate trimming) |
| `emit_control_events` | bool | `true` | Emit lifecycle control events |
| `batch_enabled` | bool | `false` | Enable batch publishing for performance |
| `batch_max_size` | usize | `100` | Max events per batch (1-10000) |
| `batch_max_wait_ms` | u64 | `100` | Max wait time before flushing batch (ms) |

### Redis URL Formats
```
redis://localhost:6379                    # Basic connection
redis://:password@localhost:6379          # Password auth
redis://user:pass@localhost:6379          # Username + password (Redis 6+)
rediss://localhost:6380                   # TLS/SSL connection
redis://localhost:6379/0                  # Specific database
```

### Configuration Examples

#### Basic Configuration
```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries: ["my-query"]
    auto_start: true
    redis_url: "redis://localhost:6379"
```

#### With Stream Management
```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries: ["my-query", "another-query"]
    redis_url: "redis://localhost:6379"
    max_stream_length: 10000  # Keep last 10k results per stream
```

#### With Batching (High Throughput)
```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries: ["high-volume-query"]
    redis_url: "redis://localhost:6379"
    batch_enabled: true
    batch_max_size: 500        # Batch up to 500 events
    batch_max_wait_ms: 50      # Flush every 50ms
    max_stream_length: 50000
```

#### With Custom Names
```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries: ["my-query"]
    redis_url: "redis://localhost:6379"
    pubsub_name: "custom-pubsub"
    source_name: "my-application"
    emit_control_events: false
```

## CloudEvent Format

### Envelope Structure
```json
{
  "data": { /* ResultEvent payload */ },
  "datacontenttype": "application/json",
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "pubsubname": "drasi-pubsub",
  "source": "drasi-core",
  "specversion": "1.0",
  "time": "2025-01-15T10:30:45.123Z",
  "topic": "my-query-results",
  "type": "com.dapr.event.sent"
}
```

### Data Payload - Change Event
```json
{
  "kind": "change",
  "queryId": "my-query",
  "sequence": 42,
  "sourceTimeMs": 1705318245123,
  "addedResults": [
    {"id": "1", "value": "new"}
  ],
  "updatedResults": [
    {
      "before": {"id": "2", "value": 10},
      "after": {"id": "2", "value": 20},
      "groupingKeys": ["key1", "key2"]
    }
  ],
  "deletedResults": [
    {"id": "3"}
  ],
  "metadata": {
    "tracking": {
      "source": {
        "seq": 42,
        "source_ns": 1744055144490466971,
        "reactivatorStart_ns": 1744055140000000000,
        "reactivatorEnd_ns": 1744055142000000000,
        "changeRouterStart_ns": 1744055159124143047,
        "changeRouterEnd_ns": 1744055173551481387,
        "changeDispatcherStart_ns": 1744055173551481387,
        "changeDispatcherEnd_ns": 1744055173551481387
      },
      "query": {
        "enqueue_ns": 1744055173551481387,
        "dequeue_ns": 1744055178510629042,
        "queryStart_ns": 1744055178510650750,
        "queryEnd_ns": 1744055178510848750
      }
    }
  }
}
```

### Data Payload - Control Event
```json
{
  "kind": "control",
  "queryId": "my-query",
  "sequence": 1,
  "sourceTimeMs": 1705318245123,
  "controlSignal": {
    "kind": "running"  // or "stopped", "bootstrapStarted", "bootstrapCompleted", "deleted"
  }
}
```

### Topic Naming
Results are published to: `{query-id}-results`

Example: Query `stock-prices` publishes to stream `stock-prices-results`

## Batch Configuration

Batching improves throughput by publishing multiple events in a single Redis pipeline operation.

### When to Use Batching
- **High Volume**: Processing hundreds or thousands of results per second
- **Low Latency Tolerance**: Can accept 1-100ms additional latency
- **Throughput Priority**: Optimizing for total throughput over individual event latency

### Batch Parameters
- `batch_enabled`: Enable/disable batching (default: `false`)
- `batch_max_size`: Max events before flush (default: `100`, recommended: `100-1000`)
- `batch_max_wait_ms`: Max wait time before flush (default: `100ms`, recommended: `1-100ms`)

### Batch Behavior
Events are buffered and flushed when:
1. Buffer reaches `batch_max_size`, OR
2. `batch_max_wait_ms` milliseconds have elapsed since last flush

Control events always trigger an immediate flush before being sent.

### Performance Guidelines
- **batch_max_size**: Larger batches = higher throughput but more memory. Keep under 10,000.
- **batch_max_wait_ms**: Lower values = lower latency but less batching efficiency
- **Retry Logic**: Batches are retried 3 times with exponential backoff
- **Fallback**: Failed batches fall back to individual event publishing

### Example Trade-offs
```yaml
# Low latency (1-10ms added latency)
batch_max_size: 50
batch_max_wait_ms: 10

# Balanced (10-50ms added latency)
batch_max_size: 100
batch_max_wait_ms: 50

# High throughput (50-100ms added latency)
batch_max_size: 500
batch_max_wait_ms: 100
```

## Profiling Metadata

When profiling is enabled (via `QueryResult.profiling`), the Platform Reaction includes detailed timing information in the `metadata.tracking` field:

### Source Tracking
- `seq`: Sequence number
- `source_ns`: Original source timestamp
- `reactivatorStart_ns`, `reactivatorEnd_ns`: Reactivator timing
- `changeRouterStart_ns`, `changeRouterEnd_ns`: Change router timing
- `changeDispatcherStart_ns`, `changeDispatcherEnd_ns`: Change dispatcher timing

### Query Tracking
- `enqueue_ns`: When result was enqueued for query processing
- `dequeue_ns`: When result was dequeued for processing
- `queryStart_ns`: Query evaluation start time
- `queryEnd_ns`: Query evaluation end time

All timestamps are in nanoseconds since epoch.

## Use Cases

### When to Use Platform Reaction
- Integrating with Drasi Platform reaction ecosystem
- Multiple downstream consumers need same query results
- Need durable result persistence
- CloudEvents compliance required for interoperability
- Distributed tracing across data pipeline
- Exactly-once delivery semantics with consumer groups

### When NOT to Use
- Direct custom application integration (use `ApplicationReaction`)
- Simple logging only (use `LogReaction`)
- Direct HTTP/gRPC delivery (use `HttpReaction` or `GrpcReaction`)
- No Redis infrastructure available
- Ultra-low latency required without serialization overhead

## Error Handling

### Connection Retries
- Redis connections retry 3 times with exponential backoff (100ms, 200ms, 400ms)
- Publishing retries 3 times with exponential backoff
- Batch publish failures fall back to individual event publishing

### Stream Management
- Stream length trimming uses approximate mode (`MAXLEN ~`) for performance
- Trimming is per-stream, not global
- Failed publishes are logged but don't stop processing

## Implementation Notes

### Module Structure
- `mod.rs`: Main reaction implementation and lifecycle
- `transformer.rs`: QueryResult to ResultEvent transformation
- `publisher.rs`: Redis Stream publishing with retry logic
- `types.rs`: CloudEvent and ResultEvent type definitions
- `tests.rs`: Comprehensive unit and integration tests

### Testing
- Unit tests in each module (`mod.rs`, `transformer.rs`, `publisher.rs`, `types.rs`)
- Integration tests in `tests.rs` covering end-to-end flows
- Mock Redis tests for configuration validation
- CloudEvent format verification tests
- Batch processing tests
- Profiling metadata tests

Run tests:
```bash
cargo test -p drasi-server-core platform
```
