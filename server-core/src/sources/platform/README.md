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

### Required Properties

| Property | Type | Description |
|----------|------|-------------|
| `redis_url` | String | Redis connection URL (e.g., `redis://localhost:6379`) |
| `stream_key` | String | Redis stream key to read from (e.g., `sensor-data:changes`) |
| `consumer_group` | String | Consumer group name for coordinated consumption |
| `consumer_name` | String | Unique consumer name within the group |

### Optional Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `batch_size` | Number | 10 | Number of events to read per XREADGROUP call |
| `block_ms` | Number | 5000 | Milliseconds to block waiting for new events |
| `start_id` | String | ">" | Stream position to start from (">" for new, "0" for all) |
| `max_retries` | Number | 3 | Maximum connection retry attempts |
| `retry_delay_ms` | Number | 1000 | Delay between connection retries (with exponential backoff) |

### Example Configuration

```yaml
sources:
  - id: external_sensor_data
    source_type: platform
    auto_start: true
    properties:
      redis_url: "redis://localhost:6379"
      stream_key: "sensor-source:changes"
      consumer_group: "drasi-core"
      consumer_name: "consumer-1"
      batch_size: 10
      block_ms: 5000
```

### Kubernetes/Multi-Instance Configuration

```yaml
sources:
  - id: platform_events
    source_type: platform
    properties:
      redis_url: "redis://redis.default.svc.cluster.local:6379"
      stream_key: "events:changes"
      consumer_group: "drasi-core-group"
      consumer_name: "${HOSTNAME}"  # Use pod name for unique consumer
      batch_size: 50
```

## Event Format

The platform source consumes events in the Drasi Platform SDK format and transforms them to drasi-core's `SourceChange` format.

### Platform Event Structure

```json
{
  "id": "event-123",
  "sourceId": "my_source",
  "type": "i",  // "i" = insert, "u" = update, "d" = delete
  "elementType": "node",  // or "rel" for relations
  "time": {
    "seq": 1,
    "ms": 1234567890000
  },
  "after": {  // or "before" for deletes
    "id": "element1",
    "labels": ["Person", "Employee"],
    "properties": {
      "name": "Alice",
      "age": 30
    }
  }
}
```

### Relation Event Example

```json
{
  "type": "i",
  "elementType": "rel",
  "time": { "ms": 1000 },
  "after": {
    "id": "rel1",
    "labels": ["KNOWS"],
    "startId": "node1",
    "endId": "node2",
    "properties": { "since": 2020 }
  }
}
```

### Event Transformation Mapping

| Platform Event | drasi-core SourceChange |
|----------------|-------------------------|
| `type: "i"` | `SourceChange::Insert` |
| `type: "u"` | `SourceChange::Update` |
| `type: "d"` | `SourceChange::Delete` |
| `elementType: "node"` | `Element::Node` |
| `elementType: "rel"` | `Element::Relation` |
| `startId` | `out_node` |
| `endId` | `in_node` |
| `time.ms` | `effective_from` (× 1,000,000 to convert ms to ns) |

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

Events should be written to Redis using XADD:

```bash
redis-cli XADD events:changes * \
  data '{"type":"i","elementType":"node","time":{"ms":1000},"after":{"id":"1","labels":["Test"],"properties":{}}}'
```

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
