# Platform Reaction

## Overview

The Platform Reaction is a Drasi component that publishes continuous query results to Redis Streams in CloudEvent format. It receives query results from Drasi queries and publishes them to Redis Streams, allowing downstream consumers to subscribe to specific query results.

### Key Capabilities

- **Redis Streams Publishing**: Publishes query results to Redis Streams with automatic stream management
- **CloudEvent Format**: Wraps all events in Dapr CloudEvent envelopes for standardization
- **Change Detection**: Publishes add, update, and delete operations from continuous queries
- **Control Events**: Optional lifecycle events (bootstrap started/completed, running, stopped)
- **Batch Processing**: Optional batching for high-throughput scenarios
- **Profiling Metadata**: Automatically includes timing and performance metadata when available
- **Stream Management**: Configurable stream length limits with automatic trimming
- **Retry Logic**: Built-in exponential backoff retry mechanism for resilient publishing

### Use Cases

- **Event-Driven Architectures**: Publish query results to Redis Streams for consumption by microservices
- **Real-Time Notifications**: Trigger notifications based on continuous query results
- **Data Synchronization**: Keep external systems synchronized with Drasi query results
- **Audit Trails**: Maintain a queryable log of all changes detected by continuous queries
- **Analytics Pipelines**: Feed query results into analytics and data processing pipelines
- **Integration with Dapr**: Seamless integration with Dapr pub/sub components

## Configuration

The Platform Reaction can be configured using either the builder pattern (preferred for programmatic usage) or the configuration struct approach (for YAML-based or dynamic configuration).

### Builder Pattern (Preferred)

```rust
use drasi_reaction_platform::PlatformReaction;

let reaction = PlatformReaction::builder("my-platform-reaction")
    .with_queries(vec!["query1".to_string(), "query2".to_string()])
    .with_redis_url("redis://localhost:6379")
    .with_pubsub_name("my-pubsub")
    .with_source_name("my-service")
    .with_max_stream_length(10000)
    .with_emit_control_events(true)
    .with_batch_enabled(true)
    .with_batch_max_size(100)
    .with_batch_max_wait_ms(50)
    .with_priority_queue_capacity(1000)
    .with_auto_start(true)
    .build()?;
```

### Configuration Struct Approach

```rust
use drasi_reaction_platform::{PlatformReaction, PlatformReactionConfig};

let config = PlatformReactionConfig {
    redis_url: "redis://localhost:6379".to_string(),
    pubsub_name: Some("my-pubsub".to_string()),
    source_name: Some("my-service".to_string()),
    max_stream_length: Some(10000),
    emit_control_events: true,
    batch_enabled: true,
    batch_max_size: 100,
    batch_max_wait_ms: 50,
};

let reaction = PlatformReaction::new(
    "my-platform-reaction",
    vec!["query1".to_string()],
    config,
)?;
```

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `id` | Unique identifier for the reaction | `String` | Any non-empty string | Required |
| `queries` | List of query IDs to subscribe to | `Vec<String>` | Query IDs | Required |
| `redis_url` | Redis connection URL | `String` | Valid Redis URL (e.g., `redis://host:port`) | Required |
| `pubsub_name` | PubSub name for CloudEvent metadata | `Option<String>` | Any string | `"drasi-pubsub"` |
| `source_name` | Source name for CloudEvent metadata | `Option<String>` | Any string | `"drasi-core"` |
| `max_stream_length` | Maximum length of Redis streams (enables trimming) | `Option<usize>` | Any positive number | `None` (unlimited) |
| `emit_control_events` | Whether to emit control events (lifecycle signals) | `bool` | `true`, `false` | `false` |
| `batch_enabled` | Enable batching of events before publishing | `bool` | `true`, `false` | `false` |
| `batch_max_size` | Maximum number of events per batch | `usize` | 1-10000 (recommended: 100-1000) | `100` |
| `batch_max_wait_ms` | Maximum wait time before flushing batch (milliseconds) | `u64` | 1-1000 (recommended: 1-100) | `100` |
| `priority_queue_capacity` | Capacity of internal priority queue | `Option<usize>` | Any positive number | `None` (default) |
| `auto_start` | Whether to automatically start the reaction | `bool` | `true`, `false` | `true` |

### Configuration Notes

- **redis_url**: Must be a valid Redis connection URL. Connection is validated when the reaction starts.
- **max_stream_length**: Uses Redis `MAXLEN ~` (approximate trimming) for efficiency. Useful for preventing unbounded stream growth.
- **batch_max_size**: Setting to 0 will cause validation error. Very large values (>10000) will trigger a warning.
- **batch_max_wait_ms**: Values >1000ms will trigger a warning about increased latency.
- **emit_control_events**: When enabled, publishes lifecycle events (bootstrap started/completed, running, stopped) to the same stream as data changes.

## Output Schema

The Platform Reaction publishes messages to Redis Streams in CloudEvent format. Each message is stored in a stream named `{query-id}-results`.

### Redis Stream Structure

Each entry in the Redis stream contains a single field:

```
XREAD STREAMS my-query-results 0
1) 1) "my-query-results"
   2) 1) 1) "1705318245123-0"
         2) 1) "data"
            2) "{...CloudEvent JSON...}"
```

### CloudEvent Envelope

All events are wrapped in a CloudEvent envelope conforming to the CloudEvents 1.0 specification:

```json
{
  "data": { /* ResultEvent - see below */ },
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

### Change Event (data field)

Change events contain the actual query results:

```json
{
  "kind": "change",
  "queryId": "my-query",
  "sequence": 42,
  "sourceTimeMs": 1705318245123,
  "addedResults": [
    {
      "id": "1",
      "name": "John Doe",
      "temperature": 98.6
    }
  ],
  "updatedResults": [
    {
      "before": {
        "id": "2",
        "value": 10
      },
      "after": {
        "id": "2",
        "value": 20
      },
      "groupingKeys": ["sensor_id"]
    }
  ],
  "deletedResults": [
    {
      "id": "3",
      "name": "Jane Smith"
    }
  ],
  "metadata": {
    "tracking": {
      "source": {
        "source_ns": 1744055144490466971,
        "changeRouterStart_ns": 1744055159124143047,
        "changeRouterEnd_ns": 1744055173551481387,
        "seq": 42
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

### Control Event (data field)

Control events signal lifecycle transitions:

```json
{
  "kind": "control",
  "queryId": "my-query",
  "sequence": 1,
  "sourceTimeMs": 1705318245123,
  "controlSignal": {
    "kind": "bootstrapStarted"
  }
}
```

**Control Signal Types:**
- `bootstrapStarted` - Query bootstrap process has started
- `bootstrapCompleted` - Query bootstrap process has completed
- `running` - Query is running normally
- `stopped` - Query has been stopped
- `deleted` - Query has been deleted

### Field Descriptions

#### CloudEvent Fields

- **data**: The actual event payload (ResultEvent)
- **datacontenttype**: Always `"application/json"`
- **id**: Unique UUID v4 for this event
- **pubsubname**: PubSub name from configuration
- **source**: Source identifier from configuration
- **specversion**: CloudEvents version (`"1.0"`)
- **time**: Event creation timestamp (ISO 8601 format)
- **topic**: Stream name in format `{query-id}-results`
- **type**: Always `"com.dapr.event.sent"`

#### ResultEvent Fields

- **kind**: Event type (`"change"` or `"control"`)
- **queryId**: ID of the query that produced this result
- **sequence**: Monotonically increasing sequence number (per reaction instance)
- **sourceTimeMs**: Original source timestamp in milliseconds since epoch
- **addedResults**: Array of objects added by the query (always present, may be empty)
- **updatedResults**: Array of update payloads (always present, may be empty)
- **deletedResults**: Array of objects removed by the query (always present, may be empty)
- **metadata**: Optional metadata including profiling/tracking information

#### Update Payload Fields

- **before**: Object state before the update
- **after**: Object state after the update
- **groupingKeys**: Optional array of grouping keys (for aggregation queries)

#### Metadata Tracking Fields

When profiling is enabled, the metadata includes timing information:

- **tracking.source**: Source-side timing information
  - `source_ns`: Original source timestamp
  - `changeRouterStart_ns`: When change router received the event
  - `changeRouterEnd_ns`: When change router sent the event
  - `reactivatorStart_ns`: When reactivator started processing
  - `reactivatorEnd_ns`: When reactivator finished processing
  - `seq`: Source sequence number

- **tracking.query**: Query-side timing information
  - `enqueue_ns`: When query was enqueued for processing
  - `dequeue_ns`: When query was dequeued for processing
  - `queryStart_ns`: When query core processing started
  - `queryEnd_ns`: When query core processing ended

## Usage Examples

### Basic Usage

```rust
use drasi_reaction_platform::PlatformReaction;

// Create a simple Platform reaction
let reaction = PlatformReaction::builder("sensor-reaction")
    .with_query("high-temperature-sensors")
    .with_redis_url("redis://localhost:6379")
    .build()?;

// Start the reaction (if auto_start is false)
reaction.start().await?;
```

### Multiple Query Subscription

```rust
use drasi_reaction_platform::PlatformReaction;

// Subscribe to multiple queries
let reaction = PlatformReaction::builder("multi-query-reaction")
    .with_queries(vec![
        "query1".to_string(),
        "query2".to_string(),
        "query3".to_string(),
    ])
    .with_redis_url("redis://redis-cluster:6379")
    .build()?;
```

### High-Throughput with Batching

```rust
use drasi_reaction_platform::PlatformReaction;

// Enable batching for high-throughput scenarios
let reaction = PlatformReaction::builder("high-volume-reaction")
    .with_query("real-time-analytics")
    .with_redis_url("redis://localhost:6379")
    .with_batch_enabled(true)
    .with_batch_max_size(500)  // Batch up to 500 events
    .with_batch_max_wait_ms(50)  // Or wait max 50ms
    .build()?;
```

### Custom CloudEvent Configuration

```rust
use drasi_reaction_platform::PlatformReaction;

// Customize CloudEvent metadata
let reaction = PlatformReaction::builder("custom-cloud-events")
    .with_query("my-query")
    .with_redis_url("redis://localhost:6379")
    .with_pubsub_name("production-pubsub")
    .with_source_name("sensor-service")
    .build()?;
```

### Stream Management with Length Limits

```rust
use drasi_reaction_platform::PlatformReaction;

// Limit stream length to prevent unbounded growth
let reaction = PlatformReaction::builder("managed-streams")
    .with_query("event-stream")
    .with_redis_url("redis://localhost:6379")
    .with_max_stream_length(10000)  // Keep only last 10k events
    .build()?;
```

### Control Events Enabled

```rust
use drasi_reaction_platform::PlatformReaction;

// Enable control events for lifecycle tracking
let reaction = PlatformReaction::builder("lifecycle-tracking")
    .with_query("monitored-query")
    .with_redis_url("redis://localhost:6379")
    .with_emit_control_events(true)
    .build()?;
```

### Priority Queue Configuration

```rust
use drasi_reaction_platform::PlatformReaction;

// Configure internal priority queue capacity
let reaction = PlatformReaction::builder("priority-queue-reaction")
    .with_query("time-sensitive-query")
    .with_redis_url("redis://localhost:6379")
    .with_priority_queue_capacity(5000)  // Buffer up to 5000 events
    .build()?;
```

### Reading Events from Redis Streams

```rust
use redis::Commands;

// Consumer reading from Redis Streams
let client = redis::Client::open("redis://localhost:6379")?;
let mut con = client.get_connection()?;

// Read from stream
let stream_key = "my-query-results";
let results: redis::streams::StreamReadReply = con.xread(
    &[stream_key],
    &["0"],  // Start from beginning
)?;

for stream in results.keys {
    for entry in stream.ids {
        let data: String = entry.map.get("data").unwrap().clone();
        let cloud_event: CloudEvent<ResultEvent> = serde_json::from_str(&data)?;

        match cloud_event.data {
            ResultEvent::Change(change) => {
                println!("Change event: {} added, {} updated, {} deleted",
                    change.added_results.len(),
                    change.updated_results.len(),
                    change.deleted_results.len()
                );
            }
            ResultEvent::Control(control) => {
                println!("Control event: {:?}", control.control_signal);
            }
        }
    }
}
```

## Implementation Details

### Architecture

The Platform Reaction follows a modular architecture:

- **platform.rs**: Main reaction implementation, lifecycle management, and processing loop
- **config.rs**: Configuration structures with serde support
- **publisher.rs**: Redis Streams publishing with retry logic and batching
- **transformer.rs**: Converts QueryResult to ResultEvent format
- **types.rs**: Type definitions for CloudEvents and result events
- **tests.rs**: Comprehensive integration tests

### Processing Flow

1. **Subscription**: Reaction subscribes to configured query IDs using the QueryProvider
2. **Reception**: Query results arrive via the priority queue (ordered by timestamp)
3. **Transformation**: Results are transformed from QueryResult to ResultEvent format
4. **CloudEvent Wrapping**: Events are wrapped in CloudEvent envelopes
5. **Publishing**: Events are published to Redis Streams (with batching if enabled)
6. **Retry**: Failed publishes are retried with exponential backoff

### Batching Behavior

When batching is enabled:

- Events accumulate in an internal buffer
- Batch is flushed when either:
  - Buffer size reaches `batch_max_size`
  - Time since last flush exceeds `batch_max_wait_ms`
- All events in a batch are published atomically using Redis pipelining
- On batch failure, fallback to individual publishing with retry

### Sequence Numbering

- Each reaction instance maintains its own monotonically increasing sequence counter
- Sequence numbers start at 0 and increment for each published event
- Both change events and control events increment the sequence
- Sequence numbers are included in profiling metadata for correlation

### Error Handling

- **Connection Failures**: Automatic retry with exponential backoff (up to 3 attempts)
- **Publishing Failures**: Retry with exponential backoff, batch fallback to individual
- **Serialization Errors**: Logged as errors, processing continues
- **Invalid Configuration**: Validation errors returned during construction

### Performance Considerations

- **Batching**: Significantly reduces Redis round trips for high-throughput scenarios
- **Pipelining**: Atomic batch publishing uses Redis pipelining for efficiency
- **Stream Trimming**: Uses approximate trimming (`MAXLEN ~`) to avoid blocking
- **Async Processing**: All I/O operations are async to avoid blocking
- **Priority Queue**: Ensures time-ordered processing of query results

## Troubleshooting

### Common Issues

**Connection refused errors**
- Verify Redis is running and accessible at the configured URL
- Check network connectivity and firewall rules
- Ensure Redis URL format is correct (`redis://host:port`)

**Events not appearing in streams**
- Check that the reaction is started (`reaction.start().await?`)
- Verify query IDs match actual query configurations
- Enable control events to see lifecycle signals
- Check Redis logs for permission or memory issues

**High memory usage**
- Enable `max_stream_length` to limit stream growth
- Reduce `batch_max_size` if buffering too many events
- Consider reducing `priority_queue_capacity`

**High latency**
- Reduce `batch_max_wait_ms` for lower latency (at cost of throughput)
- Disable batching for immediate publishing
- Check Redis performance and network latency

**Batch publishing failures**
- Check Redis max pipeline length limits
- Reduce `batch_max_size` if hitting Redis limits
- Monitor Redis memory and connection limits

### Logging

The reaction uses the `log` crate for structured logging:

```rust
// Enable debug logging
RUST_LOG=debug cargo run

// Module-specific logging
RUST_LOG=drasi_reaction_platform=debug cargo run
```

**Log Levels:**
- `error!` - Publishing failures, serialization errors
- `warn!` - Retry attempts, configuration warnings
- `info!` - Control events, lifecycle transitions
- `debug!` - Individual event publishing, batch operations

## Additional Resources

- [Drasi Platform Documentation](https://github.com/drasi-project/drasi-platform)
- [CloudEvents Specification](https://cloudevents.io/)
- [Redis Streams Documentation](https://redis.io/docs/data-types/streams/)
- [Dapr Pub/Sub](https://docs.dapr.io/developing-applications/building-blocks/pubsub/)

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
