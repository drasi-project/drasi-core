# Platform Source

A Redis Streams-based source plugin for Drasi that consumes CloudEvent-wrapped change events from the Drasi platform infrastructure.

## Overview

The Platform Source provides integration between external Drasi platform sources and drasi-lib's continuous query engine. It consumes events from Redis Streams using consumer groups, transforming platform SDK event formats into drasi-core's `SourceChange` format for processing by continuous queries.

### Key Capabilities

- **Redis Streams Integration**: Consumes events from Redis Streams using consumer groups for reliable delivery
- **CloudEvent Support**: Parses CloudEvent-wrapped messages containing node and relation changes
- **At-Least-Once Delivery**: Leverages Redis consumer groups with acknowledgments to ensure reliable event processing
- **Horizontal Scaling**: Multiple consumers can share the workload within the same consumer group
- **Automatic Reconnection**: Handles Redis connection failures with exponential backoff and retry logic
- **Control Events**: Supports control messages for query subscription coordination
- **Profiling Support**: Captures and propagates timing metadata for end-to-end performance analysis

### Use Cases

1. **Platform Integration**: Connect external Drasi platform sources (PostgreSQL, MongoDB, etc.) to drasi-lib queries
2. **Distributed Processing**: Scale event processing across multiple consumer instances
3. **Event Replay**: Replay historical events from Redis Streams for query initialization
4. **Cross-Service Communication**: Enable event-driven communication between Drasi components

## Architecture

```
External Drasi Platform Source
        ↓
  Redis Stream (CloudEvents)
        ↓
Platform Source (Consumer Group)
        ↓
  Event Transformation
        ↓
drasi-lib Queries
```

The platform source acts as a bridge between external Drasi platform sources and drasi-lib queries:

1. Reads CloudEvent-wrapped messages from a Redis Stream
2. Extracts and transforms events to `SourceChange` format
3. Publishes changes to subscribed queries via internal channels
4. Acknowledges processed events for reliable delivery

### Consumer Groups

Consumer groups enable coordinated consumption across multiple instances:

- **Load Balancing**: Messages distributed among consumers in the group
- **Exactly-Once Per Group**: Each message delivered to only one consumer
- **Failure Recovery**: Unacknowledged messages can be claimed by other consumers
- **Position Tracking**: Group maintains last processed position in the stream

## Configuration

### Builder Pattern (Preferred)

The builder pattern provides a type-safe, fluent API for constructing platform sources:

```rust
use drasi_source_platform::PlatformSource;

let source = PlatformSource::builder("my-platform-source")
    .with_redis_url("redis://localhost:6379")
    .with_stream_key("sensor-changes")
    .with_consumer_group("drasi-consumers")
    .with_consumer_name("consumer-1")
    .with_batch_size(50)
    .with_block_ms(10000)
    .with_dispatch_mode(drasi_lib::channels::DispatchMode::Channel)
    .with_dispatch_buffer_capacity(1500)
    .with_auto_start(true)
    .build()?;

drasi_lib.add_source(source).await?;
```

### Config Struct Approach

For programmatic configuration or deserialization from files:

```rust
use drasi_source_platform::{PlatformSource, PlatformSourceConfig};

let config = PlatformSourceConfig {
    redis_url: "redis://localhost:6379".to_string(),
    stream_key: "sensor-changes".to_string(),
    consumer_group: "drasi-consumers".to_string(),
    consumer_name: Some("consumer-1".to_string()),
    batch_size: 50,
    block_ms: 10000,
};

let source = PlatformSource::new("my-platform-source", config)?;
drasi_lib.add_source(source).await?;
```

### YAML Configuration

For declarative configuration in YAML files:

```yaml
sources:
  - id: platform_source
    source_type: platform
    auto_start: true
    dispatch_mode: channel  # or "broadcast"
    dispatch_buffer_capacity: 1500
    properties:
      redis_url: "redis://localhost:6379"
      stream_key: "sensor-changes"
      consumer_group: "drasi-consumers"
      consumer_name: "consumer-1"
      batch_size: 50
      block_ms: 10000
```

## Configuration Options

| Name | Type | Description | Valid Values | Default |
|------|------|-------------|--------------|---------|
| `redis_url` | String | Redis connection URL (standard redis:// format) | Valid Redis URL | **Required** |
| `stream_key` | String | Redis stream key to consume events from | Any valid stream key | **Required** |
| `consumer_group` | String | Consumer group name for coordinated consumption | Any identifier | `"drasi-core"` |
| `consumer_name` | Option\<String\> | Unique consumer name within the group | Any unique ID | Auto-generated from source ID |
| `batch_size` | usize | Number of events to read per XREADGROUP call | 1-10000 (recommended) | `100` |
| `block_ms` | u64 | Milliseconds to block waiting for new events | 100-60000 (recommended) | `5000` |
| `dispatch_mode` | DispatchMode | Event dispatch strategy | `Channel`, `Broadcast` | `Channel` |
| `dispatch_buffer_capacity` | usize | Buffer size for dispatch channels | Any positive integer | `1000` |
| `auto_start` | bool | Whether to start automatically when added to DrasiLib | `true`, `false` | `true` |

### Configuration Details

#### Redis URL Formats

Standard Redis connection string formats are supported:

- `redis://localhost:6379` - Local Redis without authentication
- `redis://:password@host:6379` - Redis with password authentication
- `redis://user:password@host:6379` - Redis with username and password
- `rediss://host:6379` - Redis with TLS encryption

#### Consumer Name

The consumer name should be unique within a consumer group:

- **Kubernetes**: Use `${HOSTNAME}` or pod name for automatic uniqueness
- **Docker**: Use container ID or hostname
- **Local Development**: Can be omitted (auto-generated from source ID)

#### Batch Size

Controls throughput vs. latency tradeoff:

- **Higher values** (100-500): Better throughput, higher memory usage
- **Lower values** (10-50): Lower latency, more frequent Redis calls
- **Recommended**: Start with 100, tune based on event rate

#### Block Timeout

Controls responsiveness vs. CPU usage:

- **Higher values** (10000-30000ms): Lower CPU, higher shutdown latency
- **Lower values** (1000-5000ms): More responsive, higher CPU usage
- **Recommended**: 5000ms for balanced performance

#### Dispatch Mode

- **Channel** (default): Each subscriber gets isolated channel with backpressure and zero message loss
- **Broadcast**: Shared channel with no backpressure, possible message loss under heavy load

## Input Schema

### CloudEvent Wrapper

All events are wrapped in CloudEvent format with the following structure:

```json
{
  "id": "5095316c-f4b6-43db-9887-f2730cf1dc2b",
  "source": "hello-world-reactivator",
  "type": "com.dapr.event.sent",
  "specversion": "1.0",
  "datacontenttype": "application/json",
  "time": "2025-10-03T14:58:12Z",
  "pubsubname": "drasi-pubsub",
  "topic": "hello-world-change",
  "data": [ /* array of change events */ ]
}
```

### Data Change Events

The `data` array contains one or more change events. Each event has:

- **op**: Operation type (`"i"` = insert, `"u"` = update, `"d"` = delete)
- **payload**: Event payload with element data and metadata
- **reactivatorStart_ns** (optional): Upstream processing start timestamp
- **reactivatorEnd_ns** (optional): Upstream processing end timestamp

### Node Insert

```json
{
  "op": "i",
  "payload": {
    "after": {
      "id": "user-123",
      "labels": ["User"],
      "properties": {
        "name": "Alice",
        "email": "alice@example.com",
        "age": 30
      }
    },
    "source": {
      "db": "mydb",
      "table": "node",
      "ts_ns": 1699900000000000000
    }
  }
}
```

### Node Update

```json
{
  "op": "u",
  "payload": {
    "after": {
      "id": "user-123",
      "labels": ["User", "Premium"],
      "properties": {
        "name": "Alice Updated",
        "age": 31,
        "premium": true
      }
    },
    "source": {
      "db": "mydb",
      "table": "node",
      "ts_ns": 1699900001000000000
    }
  }
}
```

### Node Delete

```json
{
  "op": "d",
  "payload": {
    "before": {
      "id": "user-123",
      "labels": ["User"],
      "properties": {}
    },
    "source": {
      "db": "mydb",
      "table": "node",
      "ts_ns": 1699900002000000000
    }
  }
}
```

### Relation Insert

```json
{
  "op": "i",
  "payload": {
    "after": {
      "id": "follows-1",
      "labels": ["FOLLOWS"],
      "startId": "user-123",
      "endId": "user-456",
      "properties": {
        "since": "2024-01-01"
      }
    },
    "source": {
      "db": "mydb",
      "table": "rel",
      "ts_ns": 1699900003000000000
    }
  }
}
```

### Relation Update

```json
{
  "op": "u",
  "payload": {
    "after": {
      "id": "follows-1",
      "labels": ["FOLLOWS"],
      "startId": "user-123",
      "endId": "user-456",
      "properties": {
        "since": "2024-01-01",
        "strength": 0.8
      }
    },
    "source": {
      "db": "mydb",
      "table": "rel",
      "ts_ns": 1699900004000000000
    }
  }
}
```

### Relation Delete

```json
{
  "op": "d",
  "payload": {
    "before": {
      "id": "follows-1",
      "labels": ["FOLLOWS"],
      "startId": "user-123",
      "endId": "user-456",
      "properties": {}
    },
    "source": {
      "db": "mydb",
      "table": "rel",
      "ts_ns": 1699900005000000000
    }
  }
}
```

### Field Descriptions

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `op` | String | Yes | Operation: `"i"` (insert), `"u"` (update), `"d"` (delete) |
| `payload.after` | Object | Yes (for i/u) | Element state after change |
| `payload.before` | Object | Yes (for d) | Element state before deletion |
| `payload.source.db` | String | Yes | Database name (use `"Drasi"` for control events) |
| `payload.source.table` | String | Yes | Element type: `"node"`, `"rel"`, or `"relation"` |
| `payload.source.ts_ns` | u64 | Yes | Timestamp in nanoseconds (used as `effective_from`) |
| `id` | String | Yes | Unique element identifier |
| `labels` | Array\<String\> | Yes | Element labels (at least one required) |
| `properties` | Object | Yes | Element properties (can be empty) |
| `startId` | String | Yes (relations) | Outgoing node ID for relations |
| `endId` | String | Yes (relations) | Incoming node ID for relations |

### Control Events

Control events coordinate query subscriptions and are identified by `payload.source.db = "Drasi"` (case-insensitive).

#### SourceSubscription Control Event

```json
{
  "op": "i",
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
}
```

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `queryId` | String | Yes | Unique query identifier |
| `queryNodeId` | String | Yes | Query node identifier |
| `nodeLabels` | Array\<String\> | No | Node labels query is interested in (defaults to empty) |
| `relLabels` | Array\<String\> | No | Relation labels query is interested in (defaults to empty) |

**Operations**:
- `"i"`: Insert subscription (query subscribes to source)
- `"u"`: Update subscription (query changes label filters)
- `"d"`: Delete subscription (query unsubscribes from source)

**Behavior**:
- Unknown control types are silently skipped with info log
- Missing required fields cause event to be skipped with warning
- Missing optional fields default to empty arrays

### Event Transformation Mapping

| Platform Event | drasi-core SourceChange |
|----------------|-------------------------|
| `op: "i"` | `SourceChange::Insert` |
| `op: "u"` | `SourceChange::Update` |
| `op: "d"` | `SourceChange::Delete` |
| `payload.source.table: "node"` | `Element::Node` |
| `payload.source.table: "rel"` or `"relation"` | `Element::Relation` |
| `startId` | `out_node` (ElementReference) |
| `endId` | `in_node` (ElementReference) |
| `payload.source.ts_ns` | `effective_from` (nanoseconds) |

### Property Types

All JSON property types are supported and converted to `ElementValue`:

```json
{
  "properties": {
    "string_prop": "hello",
    "int_prop": 42,
    "float_prop": 3.14,
    "bool_prop": true,
    "null_prop": null,
    "array_prop": [1, 2, 3],
    "object_prop": { "nested": "value" }
  }
}
```

## Usage Examples

### Basic Usage with Builder

```rust
use drasi_source_platform::PlatformSource;
use std::sync::Arc;

// Create platform source
let source = PlatformSource::builder("sensor-source")
    .with_redis_url("redis://localhost:6379")
    .with_stream_key("sensor-changes")
    .with_consumer_group("drasi-core")
    .with_batch_size(50)
    .build()?;

// Add to drasi-lib
drasi_lib.add_source(Arc::new(source)).await?;
```

### Kubernetes Deployment

```rust
use drasi_source_platform::PlatformSource;
use std::env;

// Use hostname for unique consumer name
let consumer_name = env::var("HOSTNAME")
    .unwrap_or_else(|_| "consumer-1".to_string());

let source = PlatformSource::builder("platform-source")
    .with_redis_url("redis://redis.default.svc.cluster.local:6379")
    .with_stream_key("events:changes")
    .with_consumer_group("drasi-core-group")
    .with_consumer_name(consumer_name)
    .with_batch_size(100)
    .build()?;

drasi_lib.add_source(Arc::new(source)).await?;
```

### With Bootstrap Provider

```rust
use drasi_source_platform::PlatformSource;
use drasi_lib::bootstrap::InMemoryBootstrapProvider;

let bootstrap_provider = InMemoryBootstrapProvider::new();

let source = PlatformSource::builder("platform-source")
    .with_redis_url("redis://localhost:6379")
    .with_stream_key("sensor-changes")
    .with_bootstrap_provider(bootstrap_provider)
    .build()?;

drasi_lib.add_source(Arc::new(source)).await?;
```

### Custom Dispatch Settings

```rust
use drasi_source_platform::PlatformSource;
use drasi_lib::channels::DispatchMode;

let source = PlatformSource::builder("platform-source")
    .with_redis_url("redis://localhost:6379")
    .with_stream_key("sensor-changes")
    .with_dispatch_mode(DispatchMode::Broadcast)
    .with_dispatch_buffer_capacity(2000)
    .build()?;

drasi_lib.add_source(Arc::new(source)).await?;
```

### Publishing Events to Redis

From external sources, publish events using XADD:

```bash
redis-cli XADD sensor-changes * \
  data '{
    "data": [{
      "op": "i",
      "payload": {
        "after": {
          "id": "sensor-1",
          "labels": ["Sensor"],
          "properties": {
            "temperature": 75.5,
            "location": "Building A"
          }
        },
        "source": {
          "db": "sensors",
          "table": "node",
          "ts_ns": 1699900000000000000
        }
      }
    }],
    "id": "event-1",
    "source": "sensor-source",
    "type": "com.dapr.event.sent"
  }'
```

### Batch Processing Multiple Events

The `data` array can contain multiple events for batch processing:

```bash
redis-cli XADD sensor-changes * \
  data '{
    "data": [
      {
        "op": "i",
        "payload": {
          "after": {
            "id": "sensor-1",
            "labels": ["Sensor"],
            "properties": {"temperature": 75.5}
          },
          "source": {"db": "sensors", "table": "node", "ts_ns": 1000000000}
        }
      },
      {
        "op": "i",
        "payload": {
          "after": {
            "id": "sensor-2",
            "labels": ["Sensor"],
            "properties": {"temperature": 72.0}
          },
          "source": {"db": "sensors", "table": "node", "ts_ns": 1000000001}
        }
      }
    ],
    "id": "batch-1",
    "source": "sensor-source",
    "type": "com.dapr.event.sent"
  }'
```

### Testing with Test Subscription

```rust
use drasi_source_platform::PlatformSource;

#[tokio::test]
async fn test_event_consumption() {
    let source = PlatformSource::builder("test-source")
        .with_redis_url("redis://localhost:6379")
        .with_stream_key("test-stream")
        .build()?;

    // Create test subscription
    let mut receiver = source.test_subscribe_async().await;

    // Start source
    source.start().await?;

    // Publish test event to Redis
    // ... (use redis-cli or Redis client)

    // Receive and verify event
    let event = receiver.recv().await?;
    // ... assertions
}
```

## Lifecycle Management

### Start

When `start()` is called:

1. Connect to Redis with exponential backoff retry (default: 5 attempts)
2. Create consumer group if it doesn't exist (using XGROUP CREATE with MKSTREAM)
3. Spawn async task to consume stream
4. Update status to `Running`
5. Begin reading events using XREADGROUP

### Running

During normal operation:

1. Read events in batches using XREADGROUP with `>` (new messages)
2. Extract and parse CloudEvent wrapper
3. Detect message type (data vs. control)
4. Transform events to `SourceChange` format
5. Dispatch to subscribed queries via channels
6. Batch acknowledge all processed messages with XACK
7. Handle errors and connection failures
8. Continue loop until stopped

### Stop

When `stop()` is called:

1. Cancel consumer task (abort tokio task)
2. Close Redis connection
3. Update status to `Stopped`

**Note**: Consumer group position is preserved in Redis. Restarting will resume from last acknowledged position.

## Error Handling

### Connection Errors

- **Initial connection failure**: Retry with exponential backoff (1s, 2s, 4s, 8s, 16s)
- **Connection lost during operation**: Auto-reconnect with same retry logic
- **Redis unavailable**: Emit ComponentEvent, keep retrying in background
- **Network issues**: Automatic recovery when connection restored

### Event Processing Errors

- **Malformed JSON**: Log warning, acknowledge event to skip, continue processing
- **Invalid event format**: Log error with details, acknowledge to avoid reprocessing
- **Missing required fields**: Log error, acknowledge event, continue
- **Transformation errors**: Send ComponentEvent, acknowledge event, continue

### Stream Errors

- **Consumer group already exists**: Ignore BUSYGROUP error, continue normally
- **Stream doesn't exist**: Created automatically via MKSTREAM flag
- **Read timeout**: Normal operation when no events available, continue loop
- **Acknowledgment failure**: Fallback to individual acknowledgments

### Error Recovery Strategy

1. **Transient errors**: Retry with exponential backoff
2. **Event errors**: Skip event with logging to prevent blocking
3. **Connection errors**: Reconnect and resume from last position
4. **Fatal errors**: Stop source and emit failure status

## Performance Considerations

### Throughput

- **Target**: >10,000 events/second per consumer
- **Batch size**: Higher values (100-500) improve throughput
- **Horizontal scaling**: Add more consumers in same group
- **Network latency**: Co-locate with Redis for best performance

### Latency

- **Target**: p99 < 10ms for transformation
- **Block timeout**: Lower values reduce latency but increase CPU
- **Batch processing**: All events in batch processed before acknowledgment
- **Channel dispatch**: Async channels provide non-blocking delivery

### Memory Usage

- **Event batching**: Controlled by `batch_size` parameter
- **Dispatch buffers**: Controlled by `dispatch_buffer_capacity`
- **Stable under load**: Minimal allocations in hot path
- **Acknowledgment batching**: All stream IDs acknowledged in single XACK

### Tuning Recommendations

**High Throughput**:
```rust
.with_batch_size(500)
.with_block_ms(10000)
.with_dispatch_buffer_capacity(5000)
```

**Low Latency**:
```rust
.with_batch_size(10)
.with_block_ms(1000)
.with_dispatch_buffer_capacity(500)
```

**Balanced**:
```rust
.with_batch_size(100)
.with_block_ms(5000)
.with_dispatch_buffer_capacity(1000)
```

## Troubleshooting

### Connection Issues

**Symptom**: "Failed to connect to Redis" errors

**Solutions**:
- Verify `redis_url` is correct and accessible
- Check network connectivity and firewall rules
- Ensure Redis is running: `redis-cli ping`
- Check Redis logs for connection errors

### Consumer Group Conflicts

**Symptom**: BUSYGROUP error or duplicate processing

**Solutions**:
- Ensure `consumer_name` is unique per instance
- In Kubernetes: Use `${HOSTNAME}` or pod name
- Check existing consumers: `redis-cli XINFO CONSUMERS stream_key group_name`
- Remove zombie consumers: `redis-cli XGROUP DELCONSUMER stream_key group_name consumer_name`

### Missing Events

**Symptom**: Events not appearing in queries

**Solutions**:
- Verify `stream_key` matches external source's stream
- Check consumer group position: `redis-cli XINFO GROUPS stream_key`
- Ensure events are being written: `redis-cli XLEN stream_key`
- Review logs for transformation errors
- Verify query subscriptions are active

### Event Replay

**Symptom**: Need to reprocess all events from stream beginning

**Solutions**:
- Delete consumer group: `redis-cli XGROUP DESTROY stream_key group_name`
- Recreate with start position: `redis-cli XGROUP CREATE stream_key group_name 0 MKSTREAM`
- Or use configuration (advanced): Set `always_create_consumer_group: true` and `start_id: "0"` in internal config

**Warning**: Deleting consumer group affects all consumers in the group.

### Performance Issues

**Symptom**: High latency or low throughput

**Solutions**:
- Increase `batch_size` for better throughput (100-500)
- Reduce `block_ms` for lower latency (1000-3000)
- Add more consumers in same group for horizontal scaling
- Check Redis performance: `redis-cli INFO stats`
- Monitor consumer lag: `redis-cli XINFO GROUPS stream_key`
- Check network latency between source and Redis

### Event Format Errors

**Symptom**: "Transformation error" or "Failed to parse JSON" in logs

**Solutions**:
- Verify external source produces correct CloudEvent format
- Check required fields: `op`, `payload`, `source`, `table`, `ts_ns`
- For nodes: Ensure `id`, `labels`, `properties` are present
- For relations: Ensure `startId` and `endId` are present
- Review event in Redis: `redis-cli XRANGE stream_key - + COUNT 1`
- Validate JSON format with external tool

### Memory Issues

**Symptom**: High memory usage or OOM errors

**Solutions**:
- Reduce `batch_size` to process fewer events at once
- Reduce `dispatch_buffer_capacity` to limit buffering
- Check for query backpressure (slow query processing)
- Monitor memory usage: `ps aux | grep drasi`
- Review Redis memory usage: `redis-cli INFO memory`

## Monitoring

### Key Metrics

**Consumer Group Metrics**:
```bash
redis-cli XINFO GROUPS stream_key
# Check 'lag' field - number of unprocessed messages
# Check 'pending' field - messages delivered but not acknowledged
```

**Consumer Metrics**:
```bash
redis-cli XINFO CONSUMERS stream_key group_name
# Check 'pending' field per consumer
# Check 'idle' field - time since last activity
```

**Stream Metrics**:
```bash
redis-cli XLEN stream_key  # Total messages in stream
redis-cli XINFO STREAM stream_key  # Stream details
```

### Health Checks

1. **Source Status**: Check `status()` returns `ComponentStatus::Running`
2. **Redis Connectivity**: Verify no connection errors in recent logs
3. **Event Flow**: Confirm events flowing to queries via ComponentEvents
4. **Consumer Lag**: Monitor consumer group lag stays within acceptable bounds
5. **Error Rate**: Track transformation errors and failed acknowledgments

### Logging

The platform source uses structured logging:

- **info**: Normal operations (start, stop, connection, consumer group creation)
- **warn**: Non-fatal issues (transformation errors, unknown control types)
- **error**: Fatal issues (connection failures, acknowledgment failures)
- **debug**: Detailed event processing (individual events, acknowledgments)

Enable debug logging for troubleshooting:
```bash
RUST_LOG=drasi_source_platform=debug cargo run
```

## Redis Streams Reference

### Consumer Group Commands

**Create consumer group**:
```bash
redis-cli XGROUP CREATE stream_key group_name 0 MKSTREAM
# 0 = start from beginning
# $ = start from end
# > = only new messages (used internally)
```

**Read events**:
```bash
redis-cli XREADGROUP GROUP group_name consumer_name COUNT 10 BLOCK 5000 STREAMS stream_key >
```

**Acknowledge events**:
```bash
redis-cli XACK stream_key group_name event_id1 event_id2 ...
```

**View pending messages**:
```bash
redis-cli XPENDING stream_key group_name
```

**Delete consumer**:
```bash
redis-cli XGROUP DELCONSUMER stream_key group_name consumer_name
```

**Delete consumer group**:
```bash
redis-cli XGROUP DESTROY stream_key group_name
```

### Stream IDs

Redis stream IDs have format `{timestamp_ms}-{sequence}`:

- **0**: Start of stream (all historical events)
- **$**: Current latest position (skip existing events)
- **>**: Only new undelivered messages (default for XREADGROUP)
- **Specific ID**: Resume from specific position (e.g., `1699900000000-0`)

## Integration Examples

### With DrasiLib

```rust
use drasi_lib::{DrasiLib, Query};
use drasi_source_platform::PlatformSource;
use std::sync::Arc;

// Create drasi-lib instance
let mut drasi = DrasiLib::new();

// Add platform source
let source = PlatformSource::builder("platform-source")
    .with_redis_url("redis://localhost:6379")
    .with_stream_key("events:changes")
    .build()?;

drasi.add_source(Arc::new(source)).await?;

// Add query
let query = Query::cypher("monitor-users")
    .query("MATCH (u:User) WHERE u.age > 18 RETURN u")
    .from_source("platform-source")
    .build();

drasi.add_query(query).await?;

// Start all components
drasi.start().await?;
```

### With DrasiServer

```rust
use drasi_server::DrasiServerBuilder;
use drasi_source_platform::PlatformSource;
use std::sync::Arc;

let source = PlatformSource::builder("platform-source")
    .with_redis_url("redis://localhost:6379")
    .with_stream_key("events:changes")
    .build()?;

let server = DrasiServerBuilder::new()
    .with_id("my-server")
    .with_host_port("0.0.0.0", 8080)
    .with_source(source)
    .build()
    .await?;

server.run().await?;
```

## Advanced Features

### Profiling Support

The platform source captures and propagates timing metadata:

- **source_ns**: Original event timestamp from `payload.source.ts_ns`
- **reactivator_start_ns**: Upstream reactivator start time (from event)
- **reactivator_end_ns**: Upstream reactivator end time (from event)
- **source_send_ns**: Platform source dispatch timestamp

This enables end-to-end latency analysis across the entire pipeline.

### Multiple Events Per CloudEvent

The platform source efficiently processes batches of events:

```json
{
  "data": [
    {"op": "i", "payload": {...}},
    {"op": "u", "payload": {...}},
    {"op": "i", "payload": {...}}
  ]
}
```

All events in the `data` array are transformed and dispatched sequentially.

### Consumer Group Persistence

Consumer group position is persisted in Redis:

- Position maintained across restarts
- Enables exactly-once semantics per group
- Supports consumer failover and recovery
- No data loss on graceful shutdown

### Dispatch Modes

**Channel Mode** (default):
- Each subscriber gets isolated channel
- Backpressure when subscriber is slow
- Zero message loss
- Higher memory usage

**Broadcast Mode**:
- Shared channel for all subscribers
- No backpressure (slow subscribers drop messages)
- Lower memory usage
- Possible message loss under load

## See Also

- [drasi-lib Documentation](../../lib/README.md) - Core library documentation
- [Drasi Platform SDK](https://github.com/drasi-project/drasi-platform) - External platform sources
- [Redis Streams Documentation](https://redis.io/docs/data-types/streams/) - Redis Streams reference
- [CloudEvents Specification](https://cloudevents.io/) - CloudEvent format details
