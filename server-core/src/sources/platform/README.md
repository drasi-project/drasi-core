# Platform Source - Redis Streams Integration

The Platform Source enables drasi-server-core to consume events from external Drasi Platform sources via Redis Streams, allowing integration with the broader Drasi Platform infrastructure.

## Overview

The Platform Source implements a Redis Streams consumer that:
- Reads Source Change Events from Redis Streams
- Uses consumer groups for reliable, exactly-once delivery
- Transforms platform SDK event format to drasi-core's `SourceChange` format
- Supports horizontal scaling via multiple consumers
- Provides automatic reconnection and error handling

## Architecture

```
External Drasi Platform Source
        ↓
    Redis Stream
        ↓
Platform Source (Consumer Group)
        ↓
drasi-server-core Queries
```

The platform source acts as a bridge between external Drasi Platform sources and drasi-server-core queries, consuming events from a Redis Stream and publishing them as `SourceChange` events to internal query channels.

## Configuration

### Configuration Settings

The Platform Source supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the source | Any string | **(Required)** |
| `source_type` | String | Source type discriminator | "platform" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this source | true, false | `true` |
| `redis_url` | String | Redis connection URL | Valid Redis URL (e.g., `redis://localhost:6379`) | **(Required)** |
| `stream_key` | String | Redis stream key to read from | Any valid Redis stream key | **(Required)** |
| `consumer_group` | String | Consumer group name for coordinated consumption | Any valid consumer group name | `"drasi-core"` |
| `consumer_name` | String (Optional) | Unique consumer name within the group | Any unique identifier | Auto-generated from source ID |
| `batch_size` | Integer (usize) | Number of events to read per XREADGROUP call | Any positive integer | `100` |
| `block_ms` | Integer (u64) | Milliseconds to block waiting for new events | Any positive integer | `5000` |
| `dispatch_mode` | String (Optional) | Event dispatch mode: "channel" (isolated channels per subscriber with backpressure, zero message loss) or "broadcast" (shared channel, no backpressure, possible message loss) | "channel", "broadcast" | `"channel"` |
| `dispatch_buffer_capacity` | Integer (Optional) | Buffer size for dispatch channel | Any positive integer | `1000` |
| `bootstrap_provider` | Object (Optional) | Bootstrap provider configuration | See Bootstrap Providers section | `None` |

**Note**: The Platform Source does not use any common configuration types from `config/common.rs` (such as SslMode or TableKeyConfig).

### Example Configuration

```yaml
sources:
  - id: external_sensor_data
    source_type: platform
    auto_start: true
    dispatch_mode: "channel"  # Isolated channels with backpressure
    dispatch_buffer_capacity: 1500
    redis_url: "redis://localhost:6379"
    stream_key: "sensor-source:changes"
    consumer_group: "drasi-core"
    consumer_name: "consumer-1"
    batch_size: 10
    block_ms: 5000
    start_id: ">"
    always_create_consumer_group: false
```

### Kubernetes/Multi-Instance Configuration

```yaml
sources:
  - id: platform_events
    source_type: platform
    redis_url: "redis://redis.default.svc.cluster.local:6379"
    stream_key: "events:changes"
    consumer_group: "drasi-core-group"
    consumer_name: "${HOSTNAME}"  # Use pod name for unique consumer
    batch_size: 50
```

## Event Format

The platform source consumes events in the Drasi Platform SDK format and transforms them to drasi-core's `SourceChange` format.

### Platform Event Structure

Events are wrapped in CloudEvent format with a data array containing change events:

```json
{
  "id": "5095316c-f4b6-43db-9887-f2730cf1dc2b",
  "source": "hello-world-reactivator",
  "type": "com.dapr.event.sent",
  "specversion": "1.0",
  "datacontenttype": "application/json",
  "time": "2025-10-03T14:58:12Z",
  "data": [{
    "op": "u",  // "i" = insert, "u" = update, "d" = delete
    "payload": {
      "after": {  // or "before" for deletes
        "id": "node1",
        "labels": ["Person", "Employee"],
        "properties": {
          "name": "Alice",
          "age": 30
        }
      },
      "source": {
        "db": "my_database",
        "table": "node",  // or "rel" for relations
        "ts_ns": 1759503489836973000
      }
    },
    "reactivatorStart_ns": 1759503491640055712,
    "reactivatorEnd_ns": 1759503491747344212
  }]
}
```

**Note:**
- The `reactivatorStart_ns` and `reactivatorEnd_ns` fields are optional profiling timestamps that track processing time in the upstream reactivator.
- The `data` array can contain multiple events, which will all be processed in sequence from a single CloudEvent message.

### Relation Event Example

```json
{
  "data": [{
    "op": "i",
    "payload": {
      "after": {
        "id": "rel1",
        "labels": ["KNOWS"],
        "startId": "node1",
        "endId": "node2",
        "properties": { "since": 2020 }
      },
      "source": {
        "db": "my_database",
        "table": "rel",
        "ts_ns": 1000000000
      }
    }
  }]
}
```

### Event Transformation Mapping

| Platform Event | drasi-core SourceChange |
|----------------|-------------------------|
| `op: "i"` | `SourceChange::Insert` |
| `op: "u"` | `SourceChange::Update` |
| `op: "d"` | `SourceChange::Delete` |
| `payload.source.table: "node"` | `Element::Node` |
| `payload.source.table: "rel"` or `"relation"` | `Element::Relation` |
| `startId` | `out_node` |
| `endId` | `in_node` |
| `payload.source.ts_ns` | `effective_from` (already in nanoseconds) |

## Control Messages

The Platform Source supports control messages for coordinating query subscriptions. Control messages are distinguished from data messages by the `payload.source.db` field.

### Message Type Detection

- **Data Message**: `payload.source.db` is any value except "Drasi" (case-insensitive)
- **Control Message**: `payload.source.db` equals "Drasi" (case-insensitive)

The control message type is determined by `payload.source.table` field.

### Supported Control Types

#### SourceSubscription

Notifies the source about query subscriptions. Used to coordinate which queries are interested in which node and relation labels.

**Event Structure:**

```json
{
  "data": [{
    "op": "i",  // "i" = insert, "u" = update, "d" = delete
    "payload": {
      "after": {
        "queryId": "query1",
        "queryNodeId": "default",
        "nodeLabels": ["Person", "Employee"],
        "relLabels": ["KNOWS", "WORKS_FOR"]
      },
      "source": {
        "db": "Drasi",
        "table": "SourceSubscription",
        "ts_ns": 1000000000
      }
    }
  }]
}
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queryId` | String | Yes | Unique query identifier |
| `queryNodeId` | String | Yes | Query node identifier |
| `nodeLabels` | Array[String] | No | Node labels this query is interested in (defaults to empty) |
| `relLabels` | Array[String] | No | Relation labels this query is interested in (defaults to empty) |

**Behavior:**

- Unknown control types are silently skipped (logged at info level)
- Missing required fields (queryId, queryNodeId) cause event to be skipped with warning
- Missing optional fields (nodeLabels, relLabels) default to empty arrays

## Redis Streams Concepts

### Consumer Groups

Consumer groups enable multiple consumers to coordinate consumption of a stream:
- **Exactly-once delivery**: Each message delivered to only one consumer in the group
- **Acknowledgments**: Messages must be acknowledged (XACK) after processing
- **Failure recovery**: Unacknowledged messages can be recovered
- **Horizontal scaling**: Add more consumers to the same group to increase throughput

### Stream IDs

Redis stream IDs have the format `{timestamp_ms}-{sequence}`:
- Monotonically increasing
- Special IDs:
  - `0`: Start of stream
  - `>`: Only new messages (default)
  - `$`: Current latest position

### Commands Used

- `XGROUP CREATE {stream} {group} {start_id} MKSTREAM`: Create consumer group
- `XREADGROUP GROUP {group} {consumer} COUNT {n} BLOCK {ms} STREAMS {stream} {id}`: Read events
- `XACK {stream} {group} {id}`: Acknowledge processed event

## Lifecycle

1. **Start**:
   - Connect to Redis
   - Create consumer group (if not exists)
   - Spawn async task to consume stream
   - Update status to `Running`

2. **Running**:
   - Read events in batches using XREADGROUP
   - Transform each event to SourceChange
   - Publish to internal channels
   - Acknowledge processed events with XACK
   - Handle errors and reconnections

3. **Stop**:
   - Cancel consumer task
   - Close Redis connection
   - Update status to `Stopped`

## Error Handling

### Connection Errors

- **Initial connection failure**: Retry with exponential backoff (up to `max_retries`)
- **Connection lost during operation**: Auto-reconnect with exponential backoff
- **Redis unavailable**: Emit ComponentEvent, keep retrying in background

### Event Processing Errors

- **Malformed JSON**: Log warning, acknowledge event to skip, continue processing
- **Invalid event format**: Log error with details, acknowledge to avoid reprocessing
- **Transformation errors**: Send ComponentEvent, acknowledge event, continue

### Stream Errors

- **Consumer group already exists**: Ignore BUSYGROUP error, continue normally
- **Stream doesn't exist**: Created automatically via MKSTREAM flag
- **Read timeout**: Normal operation, continue loop

## Performance Considerations

### Throughput

- **Target**: >10,000 events/second per consumer
- **Tuning**: Adjust `batch_size` to balance latency and throughput
- **Scaling**: Add more consumers (same group) for horizontal scaling

### Latency

- **Target**: p99 < 10ms for transformation
- **Block timeout**: `block_ms` affects responsiveness to new events
- **Network latency**: Co-locate with Redis for best performance

### Memory Usage

- Stable under sustained load
- Minimal allocations in hot path
- Event batching controlled by `batch_size`

## Troubleshooting

### Connection Issues

**Symptom**: "Failed to connect to Redis" errors

**Solutions**:
- Verify `redis_url` is correct and Redis is accessible
- Check network connectivity and firewall rules
- Ensure Redis is running and accepting connections

### Consumer Group Conflicts

**Symptom**: BUSYGROUP error or duplicate processing

**Solutions**:
- Ensure `consumer_name` is unique per instance
- Use `${HOSTNAME}` or pod name in Kubernetes
- Check for zombie consumers: `XINFO CONSUMERS {stream} {group}`

### Replaying Events from Start

**Symptom**: Need to reprocess all events from the beginning of the stream

**Solutions**:
- Set `always_create_consumer_group: true` to force recreation of the consumer group on startup
- Set `start_id: "0"` to start from the beginning of the stream
- Note: This deletes the existing consumer group, so all consumers in the group will be affected
- Use with caution in production environments

### Missing Events

**Symptom**: Events not appearing in queries

**Solutions**:
- Verify `stream_key` matches external source's stream
- Check consumer group position: `XINFO GROUPS {stream}`
- Ensure events are being written to stream: `XLEN {stream}`
- Review logs for transformation errors

### Performance Issues

**Symptom**: High latency or low throughput

**Solutions**:
- Increase `batch_size` for better throughput
- Reduce `block_ms` for lower latency
- Add more consumers (same group) for scaling
- Check Redis performance and network latency
- Monitor consumer group lag: `XINFO GROUPS {stream}`

### Event Format Errors

**Symptom**: "Transformation error" in logs

**Solutions**:
- Verify external source produces correct platform event format
- Check required fields: type, elementType, time.ms, after/before, id, labels
- For relations, ensure startId and endId are present
- Review event structure in Redis: `XRANGE {stream} - + COUNT 1`

## Example Usage

### Basic Setup

```rust
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig, RuntimeConfig};

let config = DrasiServerCoreConfig {
    sources: vec![
        SourceConfig {
            id: "platform_source".to_string(),
            source_type: "platform".to_string(),
            auto_start: true,
            properties: {
                let mut props = HashMap::new();
                props.insert("redis_url".to_string(), json!("redis://localhost:6379"));
                props.insert("stream_key".to_string(), json!("events:changes"));
                props.insert("consumer_group".to_string(), json!("drasi-core"));
                props.insert("consumer_name".to_string(), json!("consumer-1"));
                props.insert("batch_size".to_string(), json!(10));
                props.insert("block_ms".to_string(), json!(5000));
                props.insert("start_id".to_string(), json!(">"));
                props.insert("always_create_consumer_group".to_string(), json!(false));
                props
            },
            bootstrap_provider: None,
        },
    ],
    queries: vec![
        QueryConfig {
            id: "monitor_changes".to_string(),
            query: "MATCH (n) RETURN n".to_string(),
        },
    ],
    reactions: vec![],
};

let mut core = DrasiServerCore::new(Arc::new(RuntimeConfig::from(config)));
core.initialize().await?;
core.start().await?;
```

### Publishing Events from External Source

Events should be written to Redis using XADD with CloudEvent format:

```bash
redis-cli XADD events:changes * \
  data '{"data":[{"op":"i","payload":{"after":{"id":"1","labels":["Test"],"properties":{}},"source":{"db":"mydb","table":"node","ts_ns":1000000000}}}],"id":"test-1","source":"test-source","type":"com.dapr.event.sent"}'
```

**Note:** Events can be written with multiple changes in the data array for batch processing.

## Monitoring

### Key Metrics

- **Consumer group lag**: `XINFO GROUPS {stream}` → `lag` field
- **Pending messages**: `XINFO CONSUMERS {stream} {group}` → `pending` field
- **Events processed**: Monitor ComponentEvent emissions
- **Error rate**: Count transformation errors in logs

### Health Checks

- **Source status**: Check `ComponentStatus::Running`
- **Redis connectivity**: Verify no connection errors in recent logs
- **Event flow**: Confirm events flowing to queries
