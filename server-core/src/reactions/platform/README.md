# Platform Reaction

## Purpose

The Platform Reaction is a specialized reaction component in DrasiServerCore that publishes continuous query results to Redis Streams in CloudEvents format. It serves as the integration bridge between DrasiServerCore and the broader Drasi Platform ecosystem.

### What It Does

The Platform Reaction:
- Receives continuous query results from the DrasiServerCore query engine
- Transforms results into Drasi Platform's standardized format
- Wraps data in CloudEvents envelopes following the CloudEvents 1.0 specification
- Publishes events to Redis Streams with per-query topic isolation
- Provides sequence numbering for result ordering
- Optionally emits control events for lifecycle management
- Includes performance tracking metadata for distributed tracing

### Drasi Platform Integration Architecture

The Platform Reaction is a critical component in the Drasi Platform's data flow:

```
┌─────────────┐      ┌──────────────┐      ┌──────────────┐      ┌─────────────┐
│   Sources   │─────▶│   Queries    │─────▶│   Platform   │─────▶│    Redis    │
│ (Data In)   │      │  (Transform) │      │   Reaction   │      │   Streams   │
└─────────────┘      └──────────────┘      └──────────────┘      └─────────────┘
                                                   │
                                                   ▼
                                            CloudEvents Format
                                            {query-id}-results topic
```

In Drasi Platform deployments:
1. **Source components** detect data changes and feed them into queries
2. **Query components** evaluate continuous queries and produce results
3. **Platform Reaction** publishes results to Redis Streams as CloudEvents
4. **Dapr pubsub** subscribes to these streams and routes to downstream reactions
5. **Downstream reactions** (SignalR, Webhook, etc.) consume and process events

The Platform Reaction uses Redis Streams (not Redis Pub/Sub) because:
- Streams provide persistence and message durability
- Consumer groups enable exactly-once delivery semantics
- Messages remain available until acknowledged
- Multiple consumers can process in parallel
- Stream length management prevents unbounded growth

### Role in the Broader Drasi Platform

The Platform Reaction serves multiple roles:

1. **Format Translation**: Converts DrasiServerCore's internal query result format to the Drasi Platform SDK's standardized CloudEvent format
2. **Persistence Layer**: Ensures query results are durably stored in Redis Streams before delivery
3. **Topic Isolation**: Each query publishes to its own stream (`{query-id}-results`), enabling selective subscription
4. **Observability**: Embeds profiling metadata for end-to-end performance tracking across the platform
5. **Lifecycle Management**: Emits control events (Running, Stopped) for monitoring and coordination
6. **Dapr Integration**: Produces CloudEvents that Dapr's pubsub component can route to multiple subscribers

### When to Use This Reaction

Use the Platform Reaction when:

- **Integrating with Drasi Platform**: You need to connect DrasiServerCore to Drasi Platform's reaction ecosystem
- **Multiple Consumers**: Multiple downstream components need to consume the same query results
- **Result Persistence**: You require durable storage of query results
- **CloudEvents Compliance**: You need standardized CloudEvents format for interoperability
- **Distributed Tracing**: You want to track performance across the entire data pipeline
- **Exactly-Once Delivery**: You need consumer groups with acknowledgment semantics
- **Lifecycle Monitoring**: You want to track query lifecycle state changes

Do NOT use this reaction when:

- **Direct Integration**: You're building a custom application that can consume results directly via ApplicationReaction
- **Simple Logging**: You just need to log results (use LogReaction instead)
- **HTTP/gRPC Output**: You need direct HTTP or gRPC delivery (use HttpReaction or GrpcReaction)
- **No Redis Available**: Your deployment doesn't have Redis infrastructure
- **Low Latency Required**: You need minimal latency without serialization overhead

## Configuration Properties

All configuration is provided through the `properties` map in the reaction configuration.

### redis_url (REQUIRED)

- **Property name**: `redis_url`
- **Data type**: String (URL format)
- **Default value**: None (must be provided)
- **Required**: Yes
- **Description**: Redis connection URL including protocol, host, port, and optional authentication

Supported URL formats:
```
redis://localhost:6379                    # Basic connection
redis://:password@localhost:6379          # Password authentication
redis://username:password@localhost:6379  # Username + password (Redis 6+)
rediss://localhost:6380                   # TLS/SSL connection
redis://localhost:6379/0                  # Specific database number
```

### pubsub_name (OPTIONAL)

- **Property name**: `pubsub_name`
- **Data type**: String
- **Default value**: `"drasi-pubsub"`
- **Required**: No
- **Description**: Dapr pubsub component name included in CloudEvent envelope. This identifies which Dapr pubsub configuration should handle the event.

### source_name (OPTIONAL)

- **Property name**: `source_name`
- **Data type**: String
- **Default value**: `"drasi-core"`
- **Required**: No
- **Description**: CloudEvent source identifier. Indicates the origin of events and appears in the `source` field of CloudEvents.

### max_stream_length (OPTIONAL)

- **Property name**: `max_stream_length`
- **Data type**: Unsigned integer (u64)
- **Default value**: `None` (unlimited)
- **Required**: No
- **Description**: Maximum number of entries to retain in each Redis Stream. When specified, Redis uses approximate trimming (`MAXLEN ~ N`) to keep streams near this size, preventing unbounded memory growth.

**Important**: This applies per stream (per query). If you have 10 queries, each will maintain its own stream with this limit.

### emit_control_events (OPTIONAL)

- **Property name**: `emit_control_events`
- **Data type**: Boolean
- **Default value**: `true`
- **Required**: No
- **Description**: Whether to emit control events (Running, Stopped) when the reaction starts and stops. Control events enable lifecycle monitoring and coordination.

## Configuration Examples

### YAML Configuration

#### Basic Redis Connection

```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries:
      - my-query
    auto_start: true
    properties:
      redis_url: "redis://localhost:6379"
```

#### With Stream Length Management

```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries:
      - my-query
      - another-query
    auto_start: true
    properties:
      redis_url: "redis://localhost:6379"
      max_stream_length: 10000  # Keep last 10,000 results per stream
```

#### With Custom Pubsub and Source Names

```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries:
      - my-query
    auto_start: true
    properties:
      redis_url: "redis://localhost:6379"
      pubsub_name: "custom-pubsub"
      source_name: "my-application"
      max_stream_length: 5000
```

#### Control Events Disabled

```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries:
      - my-query
    auto_start: true
    properties:
      redis_url: "redis://localhost:6379"
      emit_control_events: false  # No lifecycle events
```

#### With Redis Authentication

```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries:
      - my-query
    auto_start: true
    properties:
      redis_url: "redis://:mypassword@redis.example.com:6379"
      pubsub_name: "drasi-pubsub"
```

#### With TLS/SSL Connection

```yaml
reactions:
  - id: platform-output
    reaction_type: platform
    queries:
      - my-query
    auto_start: true
    properties:
      redis_url: "rediss://redis.example.com:6380"
```

### JSON Configuration

#### Basic Redis Connection

```json
{
  "reactions": [
    {
      "id": "platform-output",
      "reaction_type": "platform",
      "queries": ["my-query"],
      "auto_start": true,
      "properties": {
        "redis_url": "redis://localhost:6379"
      }
    }
  ]
}
```

#### With Stream Length Management

```json
{
  "reactions": [
    {
      "id": "platform-output",
      "reaction_type": "platform",
      "queries": ["my-query", "another-query"],
      "auto_start": true,
      "properties": {
        "redis_url": "redis://localhost:6379",
        "max_stream_length": 10000
      }
    }
  ]
}
```

#### With Custom Pubsub and Source Names

```json
{
  "reactions": [
    {
      "id": "platform-output",
      "reaction_type": "platform",
      "queries": ["my-query"],
      "auto_start": true,
      "properties": {
        "redis_url": "redis://localhost:6379",
        "pubsub_name": "custom-pubsub",
        "source_name": "my-application",
        "max_stream_length": 5000
      }
    }
  ]
}
```

#### Control Events Disabled

```json
{
  "reactions": [
    {
      "id": "platform-output",
      "reaction_type": "platform",
      "queries": ["my-query"],
      "auto_start": true,
      "properties": {
        "redis_url": "redis://localhost:6379",
        "emit_control_events": false
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Creating the Reaction Configuration

```rust
use drasi_server_core::config::ReactionConfig;
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc;

// Create properties map
let mut properties = HashMap::new();
properties.insert("redis_url".to_string(), json!("redis://localhost:6379"));
properties.insert("pubsub_name".to_string(), json!("drasi-pubsub"));
properties.insert("source_name".to_string(), json!("drasi-core"));
properties.insert("max_stream_length".to_string(), json!(10000));
properties.insert("emit_control_events".to_string(), json!(true));

// Create reaction configuration
let config = ReactionConfig {
    id: "platform-output".to_string(),
    reaction_type: "platform".to_string(),
    queries: vec!["my-query".to_string()],
    auto_start: true,
    properties,
};

// Create event channel
let (event_tx, event_rx) = mpsc::channel(100);

// Create the reaction
let reaction = PlatformReaction::new(config, event_tx)?;
```

### Redis URL Formats

```rust
// Local development
let redis_url = "redis://localhost:6379";

// Password authentication
let redis_url = "redis://:mypassword@localhost:6379";

// Username + password (Redis 6+)
let redis_url = "redis://admin:secretpass@localhost:6379";

// TLS/SSL connection
let redis_url = "rediss://redis.example.com:6380";

// Specific database
let redis_url = "redis://localhost:6379/2";

// Production example
let redis_url = "redis://:prod_password@redis-service:6379";
```

### Setting Stream Parameters

```rust
let mut properties = HashMap::new();
properties.insert("redis_url".to_string(), json!("redis://localhost:6379"));

// Unlimited stream length (default)
// No max_stream_length property

// OR limited stream length
properties.insert("max_stream_length".to_string(), json!(50000));
```

### Starting the Reaction

```rust
use drasi_server_core::reactions::Reaction;
use tokio::sync::mpsc;

// Create query result channel
let (result_tx, result_rx) = mpsc::channel(1000);

// Start the reaction (spawns background task)
reaction.start(result_rx).await?;

// Reaction is now processing results in the background

// Later, stop the reaction
reaction.stop().await?;
```

### Complete Example

```rust
use drasi_server_core::{
    config::ReactionConfig,
    reactions::{Reaction, PlatformReaction},
    channels::QueryResult,
};
use serde_json::json;
use std::collections::HashMap;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configure the reaction
    let mut properties = HashMap::new();
    properties.insert("redis_url".to_string(), json!("redis://localhost:6379"));
    properties.insert("max_stream_length".to_string(), json!(10000));

    let config = ReactionConfig {
        id: "platform-reaction".to_string(),
        reaction_type: "platform".to_string(),
        queries: vec!["sensor-query".to_string()],
        auto_start: true,
        properties,
    };

    // Create channels
    let (event_tx, mut event_rx) = mpsc::channel(100);
    let (result_tx, result_rx) = mpsc::channel(1000);

    // Create and start reaction
    let reaction = PlatformReaction::new(config, event_tx)?;
    reaction.start(result_rx).await?;

    // Send query results
    let result = QueryResult {
        query_id: "sensor-query".to_string(),
        timestamp: chrono::Utc::now(),
        results: vec![
            json!({
                "type": "add",
                "data": {"sensor_id": "temp-01", "value": 23.5}
            })
        ],
        metadata: HashMap::new(),
        profiling: None,
    };

    result_tx.send(result).await?;

    // Process events...

    // Cleanup
    reaction.stop().await?;

    Ok(())
}
```

## Input Data Format

The Platform Reaction receives `QueryResult` structures from the query engine. Each `QueryResult` contains:

### QueryResult Structure

```rust
pub struct QueryResult {
    pub query_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<serde_json::Value>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub profiling: Option<ProfilingMetadata>,
}
```

**Fields:**

- `query_id`: Identifier of the query that produced these results
- `timestamp`: When the result was generated (UTC)
- `results`: Array of result items (adds, updates, deletes)
- `metadata`: Key-value metadata (filtered before output)
- `profiling`: Optional performance tracking data

### Result Item Formats

Each item in the `results` array has a `type` field indicating the operation:

#### Add Operation

```json
{
  "type": "add",
  "data": {
    "field1": "value1",
    "field2": 123,
    ...
  }
}
```

#### Update Operation

```json
{
  "type": "update",
  "before": {
    "field1": "old_value",
    "field2": 100
  },
  "after": {
    "field1": "new_value",
    "field2": 200
  },
  "grouping_keys": ["field1"]  // Optional, for aggregations
}
```

#### Delete Operation

```json
{
  "type": "delete",
  "data": {
    "field1": "value1",
    "field2": 123,
    ...
  }
}
```

### Complete Example

```json
{
  "query_id": "sensor-monitoring",
  "timestamp": "2025-01-15T10:30:45.123Z",
  "results": [
    {
      "type": "add",
      "data": {
        "sensor_id": "temp-01",
        "temperature": 23.5,
        "location": "warehouse-A"
      }
    },
    {
      "type": "update",
      "before": {
        "sensor_id": "temp-02",
        "temperature": 18.2
      },
      "after": {
        "sensor_id": "temp-02",
        "temperature": 19.1
      }
    },
    {
      "type": "delete",
      "data": {
        "sensor_id": "temp-03",
        "temperature": 15.0
      }
    }
  ],
  "metadata": {
    "source_id": "postgres-source",
    "custom_field": "value"
  },
  "profiling": {
    "source_ns": 1744055144490466971,
    "source_receive_ns": 1744055159124143047,
    "source_send_ns": 1744055173551481387,
    "query_receive_ns": 1744055178510629042,
    "query_core_call_ns": 1744055178510650750,
    "query_core_return_ns": 1744055178510848750
  }
}
```

## Output Data Format

The Platform Reaction publishes CloudEvents to Redis Streams. Each CloudEvent follows the CloudEvents 1.0 specification and contains query results in the data payload.

### CloudEvent Envelope Structure

The CloudEvent envelope wraps all published data:

```json
{
  "data": { ... },                           // Result payload (see below)
  "datacontenttype": "application/json",     // Always application/json
  "id": "550e8400-e29b-41d4-a716-446655440000",  // UUID v4
  "pubsubname": "drasi-pubsub",              // From configuration
  "source": "drasi-core",                    // From configuration
  "specversion": "1.0",                      // CloudEvents spec version
  "time": "2025-01-15T10:30:45.123Z",        // ISO 8601 timestamp
  "topic": "my-query-results",               // {query-id}-results
  "traceid": "",                             // Optional (empty if not set)
  "traceparent": "",                         // Optional (empty if not set)
  "tracestate": "",                          // Optional (empty if not set)
  "type": "com.dapr.event.sent"              // Always this value
}
```

**CloudEvent Fields:**

- `data`: The result payload (change event or control event)
- `datacontenttype`: Always `"application/json"`
- `id`: Unique UUID v4 identifier for this event
- `pubsubname`: Dapr pubsub component name (from config, default: `"drasi-pubsub"`)
- `source`: Event source identifier (from config, default: `"drasi-core"`)
- `specversion`: CloudEvents specification version (`"1.0"`)
- `time`: ISO 8601 timestamp when event was created
- `topic`: Stream name following pattern `{query-id}-results`
- `traceid`: Optional distributed tracing ID (currently unused)
- `traceparent`: Optional W3C trace parent (currently unused)
- `tracestate`: Optional W3C trace state (currently unused)
- `type`: Event type, always `"com.dapr.event.sent"` for Dapr compatibility

### Data Payload Structure

The `data` field contains either a **change event** or a **control event**, distinguished by the `kind` field.

#### Change Event (Query Results)

Change events contain actual query results with adds, updates, and deletes:

```json
{
  "kind": "change",
  "queryId": "sensor-monitoring",
  "sequence": 42,
  "sourceTimeMs": 1705318245123,
  "addedResults": [
    {
      "sensor_id": "temp-01",
      "temperature": 23.5,
      "location": "warehouse-A"
    }
  ],
  "updatedResults": [
    {
      "before": {
        "sensor_id": "temp-02",
        "temperature": 18.2
      },
      "after": {
        "sensor_id": "temp-02",
        "temperature": 19.1
      },
      "groupingKeys": ["sensor_id"]  // Optional, for aggregations
    }
  ],
  "deletedResults": [
    {
      "sensor_id": "temp-03",
      "temperature": 15.0
    }
  ],
  "metadata": {
    "tracking": {
      "source": {
        "seq": 42,
        "source_ns": 1744055144490466971,
        "changeRouterStart_ns": 1744055159124143047,
        "changeRouterEnd_ns": 1744055173551481387,
        "changeDispatcherStart_ns": 1744055173551481387,
        "changeDispatcherEnd_ns": 1744055173551481387,
        "reactivatorStart_ns": 1744055140000000000,
        "reactivatorEnd_ns": 1744055142000000000
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

**Change Event Fields:**

- `kind`: Always `"change"` for result events
- `queryId`: Query identifier (camelCase)
- `sequence`: Monotonically increasing sequence number (u64)
- `sourceTimeMs`: Source timestamp in milliseconds since Unix epoch
- `addedResults`: Array of objects representing added results (always present, may be empty)
- `updatedResults`: Array of update payloads with before/after states (always present, may be empty)
- `deletedResults`: Array of objects representing deleted results (always present, may be empty)
- `metadata`: Optional metadata object (omitted if empty)

**Update Payload Fields:**

- `before`: State before the update (object, optional for aggregations)
- `after`: State after the update (object, optional for aggregations)
- `groupingKeys`: Array of field names used for grouping (optional, only for aggregations)

**Metadata Tracking Structure:**

When profiling data is available, the `metadata.tracking` object contains performance measurements:

- `source.seq`: Sequence number
- `source.source_ns`: Original source timestamp (nanoseconds)
- `source.changeRouterStart_ns`: When change router started processing
- `source.changeRouterEnd_ns`: When change router finished processing
- `source.changeDispatcherStart_ns`: When change dispatcher started
- `source.changeDispatcherEnd_ns`: When change dispatcher finished
- `source.reactivatorStart_ns`: When reactivator started (if applicable)
- `source.reactivatorEnd_ns`: When reactivator finished (if applicable)
- `query.enqueue_ns`: When result was enqueued for query processing
- `query.dequeue_ns`: When result was dequeued for processing
- `query.queryStart_ns`: When query core processing started
- `query.queryEnd_ns`: When query core processing ended

All timestamps are in nanoseconds since Unix epoch.

#### Control Event (Lifecycle Signals)

Control events signal lifecycle state changes:

```json
{
  "kind": "control",
  "queryId": "sensor-monitoring",
  "sequence": 1,
  "sourceTimeMs": 1705318245123,
  "controlSignal": {
    "kind": "running"
  },
  "metadata": null
}
```

**Control Event Fields:**

- `kind`: Always `"control"` for control events
- `queryId`: Query identifier
- `sequence`: Sequence number for this control event
- `sourceTimeMs`: Timestamp when control event was generated
- `controlSignal`: Object with `kind` field indicating signal type
- `metadata`: Optional metadata (typically null for control events)

**Control Signal Types:**

- `"bootstrapStarted"`: Query bootstrap process has started
- `"bootstrapCompleted"`: Query bootstrap process has completed
- `"running"`: Query is running normally (emitted on reaction start)
- `"stopped"`: Query has been stopped (emitted on reaction stop)
- `"deleted"`: Query has been deleted

### Topic Naming Convention

All events are published to streams named using the pattern:

```
{query-id}-results
```

Examples:
- Query ID `sensor-monitoring` → Stream `sensor-monitoring-results`
- Query ID `inventory-updates` → Stream `inventory-updates-results`
- Query ID `user-activity` → Stream `user-activity-results`

This ensures:
- Each query has an isolated stream
- Consumers can subscribe to specific queries
- Multiple queries don't interfere with each other

### Complete CloudEvent Example

```json
{
  "data": {
    "kind": "change",
    "queryId": "sensor-monitoring",
    "sequence": 12345,
    "sourceTimeMs": 1705318245123,
    "addedResults": [
      {
        "sensor_id": "temp-01",
        "temperature": 23.5,
        "location": "warehouse-A",
        "timestamp": "2025-01-15T10:30:45Z"
      }
    ],
    "updatedResults": [
      {
        "before": {
          "sensor_id": "temp-02",
          "temperature": 18.2,
          "location": "warehouse-B"
        },
        "after": {
          "sensor_id": "temp-02",
          "temperature": 19.1,
          "location": "warehouse-B"
        }
      }
    ],
    "deletedResults": [],
    "metadata": {
      "tracking": {
        "source": {
          "seq": 12345,
          "source_ns": 1744055144490466971,
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
  },
  "datacontenttype": "application/json",
  "id": "550e8400-e29b-41d4-a716-446655440000",
  "pubsubname": "drasi-pubsub",
  "source": "drasi-core",
  "specversion": "1.0",
  "time": "2025-01-15T10:30:45.456Z",
  "topic": "sensor-monitoring-results",
  "traceid": "",
  "traceparent": "",
  "tracestate": "",
  "type": "com.dapr.event.sent"
}
```

## Redis Stream Integration

### Stream Naming Convention

The Platform Reaction publishes to Redis Streams with names following the pattern:

```
{query-id}-results
```

Each query gets its own isolated stream. Examples:

- Query `user-activity` → Stream `user-activity-results`
- Query `inventory-changes` → Stream `inventory-changes-results`
- Query `temperature-alerts` → Stream `temperature-alerts-results`

### How Results are Published to Redis

The Platform Reaction uses Redis Streams (XADD command) to publish messages:

1. **Serialization**: QueryResult is transformed to CloudEvent and serialized to JSON
2. **Stream Selection**: Stream name is derived from query ID (`{query-id}-results`)
3. **Publishing**: CloudEvent JSON is added to the stream with XADD
4. **Message ID**: Redis automatically generates a unique message ID (`{timestamp}-{sequence}`)
5. **Persistence**: Messages remain in the stream until trimmed or deleted

**Redis Command Equivalent:**

```redis
XADD sensor-monitoring-results * data "{...json...}"
```

Or with MAXLEN:

```redis
XADD sensor-monitoring-results MAXLEN ~ 10000 * data "{...json...}"
```

**Stream Structure:**

Each stream entry contains a single field:
- Field name: `data`
- Field value: Complete CloudEvent JSON string

### MAXLEN Parameter for Stream Length Management

The `max_stream_length` configuration controls stream growth using Redis's approximate trimming:

```rust
// Configuration
max_stream_length: Some(10000)
```

**How it works:**

1. **Approximate Trimming**: Uses `MAXLEN ~ N` (tilde for approximate)
2. **Performance**: Approximate trimming is much faster than exact trimming
3. **Behavior**: Stream may slightly exceed the limit before trimming occurs
4. **Per-Stream**: Each query's stream is trimmed independently
5. **Memory Management**: Prevents unbounded memory growth in long-running systems

**Example:**

With `max_stream_length: 10000`:
- Stream keeps approximately 10,000 most recent messages
- Older messages are automatically removed
- Trimming happens during XADD operations
- Actual size may vary by ±100 messages

**When to use:**

- ✅ Use `max_stream_length` for production deployments
- ✅ Set based on your retention requirements and available memory
- ✅ Consider query result rate when sizing
- ❌ Omit for development/testing with small data volumes
- ❌ Omit if you need indefinite retention (use external archival instead)

**Memory considerations:**

```
Stream Memory ≈ max_stream_length × average_message_size × number_of_queries
```

Example: 10 queries, 10,000 max length, 2 KB average message = ~200 MB

### Retry Logic

The Platform Reaction implements exponential backoff retry for both connection and publishing:

**Connection Retry:**
- Maximum attempts: 3
- Initial delay: 100ms
- Backoff: Exponential (100ms → 200ms → 400ms)
- Total max time: ~700ms

**Publishing Retry:**
- Maximum attempts: 3 (configurable via `publish_with_retry`)
- Initial delay: 100ms
- Backoff: Exponential (100ms → 200ms → 400ms)
- Behavior: Retries on any Redis error (network, timeout, etc.)

**Retry Flow:**

```
Attempt 1: Try publish
   ↓ (fail)
Wait 100ms
   ↓
Attempt 2: Try publish
   ↓ (fail)
Wait 200ms
   ↓
Attempt 3: Try publish
   ↓ (fail)
Log error and continue (message lost)
```

**Error Handling:**

- Failed publishes are logged but do not stop the reaction
- The reaction continues processing subsequent results
- No dead-letter queue or persistent retry queue
- Applications should monitor logs for publish failures

### Connection Handling

**Connection Model:**

The Platform Reaction uses Redis's multiplexed connection model:

- One `Client` is created at reaction initialization
- Multiplexed connections are obtained per-publish operation
- Connections are automatically returned to the pool after use
- Connection pool is managed by the Redis client library

**Connection Lifecycle:**

```
Reaction Start
   ↓
Create Redis Client (from redis_url)
   ↓
For each QueryResult:
   ↓
   Get multiplexed connection (with retry)
   ↓
   Publish to stream (with retry)
   ↓
   Connection returned to pool
   ↓
Reaction Stop
   ↓
Client dropped (connections closed)
```

**Connection Pooling:**

The underlying `redis` crate automatically manages connection pooling for multiplexed connections, providing:
- Automatic reconnection on connection loss
- Connection reuse for efficiency
- Thread-safe concurrent access
- Backpressure handling

**Configuration:**

Connection behavior is controlled by the `redis_url`:
- Timeouts: Controlled by Redis client defaults
- Keep-alive: Automatic
- Reconnection: Automatic with retry logic

## Control Events

Control events are lifecycle signals that track the state of query processing. They are published to the same stream as query results but have `kind: "control"`.

### Running Event

**When emitted:**
- Emitted when the Platform Reaction successfully starts
- Occurs after the reaction transitions to the Running state
- Sent once per reaction start

**Format:**

```json
{
  "kind": "control",
  "queryId": "my-query",
  "sequence": 1,
  "sourceTimeMs": 1705318245123,
  "controlSignal": {
    "kind": "running"
  },
  "metadata": null
}
```

**Purpose:**
- Signals to consumers that the query is actively processing
- Enables monitoring systems to track query availability
- Coordinates distributed systems waiting for query readiness

### Stopped Event

**When emitted:**
- Emitted when the Platform Reaction is explicitly stopped
- Occurs during the stop() method call
- Sent once per reaction stop

**Format:**

```json
{
  "kind": "control",
  "queryId": "my-query",
  "sequence": 9999,
  "sourceTimeMs": 1705318345123,
  "controlSignal": {
    "kind": "stopped"
  },
  "metadata": null
}
```

**Purpose:**
- Signals that the query is no longer processing results
- Allows consumers to stop waiting for new results
- Enables cleanup of downstream resources
- Supports graceful shutdown coordination

### Other Control Signals

While the Platform Reaction currently only emits `running` and `stopped` events, the control signal enum supports additional lifecycle events:

- `"bootstrapStarted"`: Query bootstrap process has started (not currently emitted by Platform Reaction)
- `"bootstrapCompleted"`: Query bootstrap process completed (not currently emitted by Platform Reaction)
- `"deleted"`: Query has been deleted (not currently emitted by Platform Reaction)

These signals are part of the Drasi Platform's broader event model and may be emitted by other components.

### How to Enable/Disable Control Events

Control events are controlled by the `emit_control_events` configuration property:

**Enable (default):**

```yaml
properties:
  redis_url: "redis://localhost:6379"
  emit_control_events: true  # or omit for default
```

**Disable:**

```yaml
properties:
  redis_url: "redis://localhost:6379"
  emit_control_events: false
```

**Programmatically:**

```rust
properties.insert("emit_control_events".to_string(), json!(false));
```

### Use Cases for Control Events

**Enable control events when:**

1. **Monitoring**: You need to track query health and availability
2. **Coordination**: Downstream systems need to know when queries start/stop
3. **Debugging**: You want visibility into query lifecycle for troubleshooting
4. **Orchestration**: You're coordinating multiple components that depend on query state
5. **Graceful Shutdown**: Consumers need to know when to stop waiting for results

**Disable control events when:**

1. **High Throughput**: Minimizing overhead is critical
2. **Simple Deployments**: Query lifecycle isn't monitored
3. **Testing**: Control events add noise to test outputs
4. **Cost Optimization**: Reducing Redis storage usage for streams

**Example monitoring workflow:**

```python
# Consumer watches for control events
for event in subscribe_to_stream("my-query-results"):
    if event["data"]["kind"] == "control":
        if event["data"]["controlSignal"]["kind"] == "running":
            print("Query started, ready to receive results")
            start_processing()
        elif event["data"]["controlSignal"]["kind"] == "stopped":
            print("Query stopped, no more results expected")
            stop_processing()
    else:
        process_result(event["data"])
```

## Sequence Numbering

### How Sequence Numbers are Generated

The Platform Reaction maintains a dedicated sequence counter for each reaction instance:

1. **Initialization**: Counter starts at 0 when reaction is created
2. **Increment**: Counter increments by 1 for each event (result or control)
3. **Atomicity**: Uses async RwLock to ensure thread-safe updates
4. **Scope**: Separate counter per reaction instance
5. **Persistence**: Counter is in-memory only (resets on restart)

**Code flow:**

```rust
// Initialize
sequence_counter: Arc::new(RwLock::new(0))

// Increment before each publish
let sequence = {
    let mut counter = sequence_counter.write().await;
    *counter += 1;
    *counter
};
```

**Sequence assignment:**
- Control event "Running" → sequence: 1 (or first available)
- Result event 1 → sequence: 2
- Result event 2 → sequence: 3
- Control event "Stopped" → sequence: N (last sequence)

### Monotonicity Guarantees

**Guarantees provided:**

✅ **Strictly Monotonic**: Sequence numbers always increase within a single reaction instance
✅ **No Gaps**: Sequence numbers increment by exactly 1 (no skipped values)
✅ **Order Preservation**: Sequence order matches processing order
✅ **Thread-Safe**: Concurrent result processing maintains monotonicity

**Guarantees NOT provided:**

❌ **Across Restarts**: Sequence resets to 0 if reaction restarts
❌ **Across Instances**: Multiple reaction instances have independent sequences
❌ **Persistence**: Sequences are not persisted to storage
❌ **Global Ordering**: Different queries have independent sequence spaces

**Implications:**

1. **Within Session**: Consumers can rely on sequence for ordering within a single reaction lifetime
2. **Restart Scenarios**: Sequence resets require consumers to handle duplicate sequence numbers
3. **Distributed Deployments**: Each reaction instance maintains its own sequence
4. **Gap Detection**: Any gap in sequences indicates message loss or processing issue

### Use Cases for Sequence Tracking

**1. Result Ordering**

Ensure results are processed in the correct order:

```python
last_sequence = 0
for event in consume_stream("my-query-results"):
    current_sequence = event["data"]["sequence"]

    if current_sequence <= last_sequence:
        print(f"Warning: Out-of-order or duplicate sequence: {current_sequence}")

    last_sequence = current_sequence
    process_event(event)
```

**2. Gap Detection**

Identify missing messages:

```python
expected_sequence = 1
for event in consume_stream("my-query-results"):
    current_sequence = event["data"]["sequence"]

    if current_sequence != expected_sequence:
        print(f"Gap detected: expected {expected_sequence}, got {current_sequence}")
        # Handle missing messages (log, alert, retry, etc.)

    expected_sequence = current_sequence + 1
```

**3. Duplicate Detection**

Prevent processing the same result multiple times:

```python
processed_sequences = set()

for event in consume_stream("my-query-results"):
    sequence = event["data"]["sequence"]

    if sequence in processed_sequences:
        print(f"Duplicate sequence {sequence}, skipping")
        continue

    processed_sequences.add(sequence)
    process_event(event)
```

**4. Progress Tracking**

Monitor processing progress:

```python
def get_processing_lag(stream_name):
    latest_in_stream = redis.xrevrange(stream_name, count=1)[0]
    latest_sequence = json.loads(latest_in_stream["data"])["sequence"]

    last_processed_sequence = get_last_processed()

    lag = latest_sequence - last_processed_sequence
    print(f"Processing lag: {lag} messages behind")
    return lag
```

**5. State Reconstruction**

Rebuild state from sequence history:

```python
def rebuild_state_from_sequence(start_sequence):
    state = {}

    for event in consume_stream_from_sequence("my-query-results", start_sequence):
        sequence = event["data"]["sequence"]

        # Apply changes in sequence order
        for item in event["data"]["addedResults"]:
            state[item["id"]] = item

        for item in event["data"]["deletedResults"]:
            state.pop(item["id"], None)

    return state
```

**6. Checkpoint/Resume**

Save progress and resume from checkpoint:

```python
# Save checkpoint
checkpoint = {
    "query_id": "my-query",
    "last_sequence": 12345,
    "timestamp": datetime.now()
}
save_checkpoint(checkpoint)

# Resume from checkpoint
checkpoint = load_checkpoint()
for event in consume_stream("my-query-results", after_sequence=checkpoint["last_sequence"]):
    process_event(event)
    update_checkpoint(event["data"]["sequence"])
```

**Important considerations:**

- Sequence numbers are scoped to a single reaction instance
- Use Redis Stream message IDs for globally unique ordering
- Combine sequence numbers with timestamps for temporal analysis
- Handle sequence resets gracefully (reaction restarts)
- Consider using sequence + query_id as composite key for multi-query tracking

## Troubleshooting

### Redis Connection Errors

**Symptoms:**
- Error message: "Failed to create Redis client"
- Error message: "Failed to connect to Redis after retries"
- Reaction fails to start

**Common causes and solutions:**

1. **Invalid Redis URL format**
   ```
   Error: Failed to create Redis client

   Solution: Check URL format
   ✅ Correct: redis://localhost:6379
   ❌ Wrong: localhost:6379
   ❌ Wrong: redis:6379
   ```

2. **Redis not running**
   ```
   Error: Failed to connect to Redis after retries

   Solution: Verify Redis is running
   $ redis-cli ping
   Should return: PONG

   Or check service status:
   $ systemctl status redis  # Linux
   $ brew services list      # macOS
   ```

3. **Wrong host/port**
   ```
   Error: Connection refused

   Solution: Verify host and port
   $ redis-cli -h localhost -p 6379 ping

   Check Redis configuration:
   $ redis-cli CONFIG GET port
   $ redis-cli CONFIG GET bind
   ```

4. **Network connectivity**
   ```
   Error: No route to host / Timeout

   Solution: Check network connectivity
   $ ping redis-host
   $ telnet redis-host 6379
   $ nc -zv redis-host 6379

   Check firewall rules allow port 6379
   ```

### Authentication Issues

**Symptoms:**
- Error message: "NOAUTH Authentication required"
- Error message: "ERR invalid password"
- Error message: "WRONGPASS invalid username-password pair"

**Common causes and solutions:**

1. **Missing password in URL**
   ```
   Error: NOAUTH Authentication required

   Solution: Add password to URL
   ❌ Wrong: redis://localhost:6379
   ✅ Correct: redis://:mypassword@localhost:6379
   ```

2. **Incorrect password**
   ```
   Error: ERR invalid password

   Solution: Verify password
   Test with redis-cli:
   $ redis-cli -h localhost -a mypassword ping

   Check Redis AUTH requirement:
   $ redis-cli CONFIG GET requirepass
   ```

3. **Wrong username (Redis 6+)**
   ```
   Error: WRONGPASS invalid username-password pair

   Solution: Check username and password
   ✅ Correct: redis://username:password@localhost:6379

   Test with redis-cli:
   $ redis-cli --user username --pass password ping
   ```

4. **TLS/SSL misconfiguration**
   ```
   Error: SSL handshake failed

   Solution: Verify TLS configuration
   ✅ Use rediss:// (not redis://) for TLS
   ✅ Correct: rediss://localhost:6380

   Test TLS connection:
   $ redis-cli --tls -h localhost -p 6380 ping
   ```

### Stream Length Management

**Symptoms:**
- Redis memory usage growing unbounded
- Warning: "OOM command not allowed when used memory > 'maxmemory'"
- Slow Redis performance

**Common causes and solutions:**

1. **No max_stream_length configured**
   ```
   Problem: Streams grow indefinitely

   Solution: Set max_stream_length
   properties:
     max_stream_length: 10000  # Keep last 10k results

   Monitor current stream length:
   $ redis-cli XLEN my-query-results
   ```

2. **max_stream_length too large**
   ```
   Problem: High memory usage despite limit

   Solution: Reduce limit based on:
   - Available memory
   - Result rate (messages/second)
   - Retention requirements

   Calculate memory usage:
   Stream memory = max_stream_length × avg_message_size × num_queries

   Example: 100k limit × 2KB × 10 queries = ~2GB
   ```

3. **Too many queries**
   ```
   Problem: Total memory = per-stream × num-queries

   Solution:
   - Reduce max_stream_length
   - Increase Redis memory
   - Archive old results externally

   Check memory usage per stream:
   $ redis-cli MEMORY USAGE my-query-results
   ```

4. **Redis maxmemory policy**
   ```
   Problem: Redis refuses writes when memory full

   Solution: Configure eviction policy
   $ redis-cli CONFIG SET maxmemory-policy allkeys-lru

   Or in redis.conf:
   maxmemory 2gb
   maxmemory-policy allkeys-lru

   Note: LRU may evict stream data unexpectedly
   Better: Set appropriate max_stream_length
   ```

### Publishing Failures and Retries

**Symptoms:**
- Log message: "Failed to publish CloudEvent (attempt 1/3)"
- Log message: "Failed to publish query result for 'my-query'"
- Results not appearing in Redis Streams

**Common causes and solutions:**

1. **Transient network issues**
   ```
   Log: Failed to publish CloudEvent (attempt 1/3): Connection reset

   This is normal - retry logic will handle it

   Monitor retry patterns:
   - Occasional retries: Normal transient issues
   - Frequent retries: Network instability
   - All retries failing: Check network/Redis health
   ```

2. **Redis server overloaded**
   ```
   Error: BUSY Redis is busy running a script

   Solution:
   - Check Redis CPU usage
   - Review slow queries: redis-cli SLOWLOG GET 10
   - Increase Redis resources
   - Reduce publishing rate
   ```

3. **Message too large**
   ```
   Error: Protocol error: too large mbulk count string

   Solution:
   - Check message size in logs
   - Review query results (may be too many fields)
   - Consider splitting large results
   - Increase Redis proto-max-bulk-len if appropriate
   ```

4. **All retries exhausted**
   ```
   Log: Failed to publish query result for 'my-query': <error>

   Impact: Result is lost (not persisted)

   Solutions:
   - Monitor logs for systematic failures
   - Check Redis health and connectivity
   - Review network stability
   - Consider increasing retry count in code
   - Implement external monitoring/alerting
   ```

### Network Connectivity

**Symptoms:**
- Connection timeouts
- Intermittent failures
- Slow publishing

**Diagnostic steps:**

1. **Basic connectivity**
   ```bash
   # Test TCP connection
   $ telnet redis-host 6379
   $ nc -zv redis-host 6379

   # Test Redis protocol
   $ redis-cli -h redis-host ping
   ```

2. **Network latency**
   ```bash
   # Measure round-trip time
   $ redis-cli -h redis-host --latency

   # Continuous monitoring
   $ redis-cli -h redis-host --latency-history

   High latency (>100ms) may cause:
   - Retry exhaustion
   - Slow result publishing
   - Backpressure in query processing
   ```

3. **DNS resolution**
   ```bash
   # Check DNS
   $ nslookup redis-host
   $ dig redis-host

   # Test with IP directly
   redis_url: redis://192.168.1.10:6379
   ```

4. **Firewall/Security Groups**
   ```bash
   # Check port accessibility
   $ nmap -p 6379 redis-host

   # Check security group rules (cloud deployments)
   - Ensure port 6379 (or 6380 for TLS) is open
   - Verify source IP is allowed
   ```

### Dapr Pubsub Integration Issues

**Symptoms:**
- Events published to Redis but not consumed by Dapr
- Dapr subscription errors
- Messages not routed to reactions

**Common causes and solutions:**

1. **Pubsub name mismatch**
   ```yaml
   Problem: Platform Reaction and Dapr component use different names

   Platform Reaction config:
   properties:
     pubsub_name: "drasi-pubsub"

   Must match Dapr component:
   apiVersion: dapr.io/v1alpha1
   kind: Component
   metadata:
     name: drasi-pubsub  # Must match!
   ```

2. **Topic subscription mismatch**
   ```yaml
   Problem: Dapr subscribes to wrong topic

   Platform Reaction publishes to: {query-id}-results
   Example: sensor-monitoring-results

   Dapr subscription must match:
   apiVersion: dapr.io/v1alpha1
   kind: Subscription
   metadata:
     name: sensor-sub
   spec:
     topic: sensor-monitoring-results  # Must match!
     route: /events
     pubsubname: drasi-pubsub
   ```

3. **Redis Streams vs Pub/Sub confusion**
   ```yaml
   Problem: Dapr configured for Redis Pub/Sub instead of Streams

   ❌ Wrong Dapr component:
   spec:
     type: pubsub.redis

   ✅ Correct Dapr component:
   spec:
     type: pubsub.redis.streams
     metadata:
       - name: redisHost
         value: localhost:6379
       - name: consumerID
         value: drasi-reaction
   ```

4. **Consumer group configuration**
   ```yaml
   Problem: Dapr not reading from stream

   Solution: Verify Dapr component has consumerID

   metadata:
     - name: consumerID
       value: my-consumer  # Required for streams
     - name: enableTLS
       value: false

   Check consumer group in Redis:
   $ redis-cli XINFO GROUPS sensor-monitoring-results
   ```

5. **CloudEvent format issues**
   ```
   Problem: Dapr rejects events due to format

   Platform Reaction uses CloudEvents 1.0 with:
   - specversion: "1.0"
   - type: "com.dapr.event.sent"
   - datacontenttype: "application/json"

   Ensure Dapr is configured to handle CloudEvents:
   metadata:
     - name: enableCloudEvent
       value: true
   ```

**Debugging Dapr integration:**

```bash
# Check Dapr logs
$ kubectl logs -l app=dapr-sidecar -c daprd

# List Dapr subscriptions
$ dapr list

# Test Dapr pubsub
$ dapr publish --publish-app-id myapp --pubsub drasi-pubsub --topic test-results --data '{}'

# Check Redis Streams directly
$ redis-cli XREAD STREAMS sensor-monitoring-results 0
```

## Limitations

### Redis Availability Requirements

**Dependency on Redis:**

The Platform Reaction requires Redis to be available and healthy for operation:

- **Critical Dependency**: Redis unavailable = results are lost
- **No Offline Mode**: Reaction cannot buffer results without Redis
- **No Fallback**: No automatic failover to alternative storage
- **Startup Requirement**: Reaction fails to start if initial Redis connection fails

**High Availability Recommendations:**

1. **Redis Clustering**: Use Redis Cluster or Sentinel for automatic failover
2. **Monitoring**: Implement Redis health checks and alerting
3. **Redundancy**: Deploy Redis with replication (master-replica)
4. **Backup**: Regular Redis backups (though streams may be transient)

**Mitigation Strategies:**

```yaml
# Use Redis Sentinel for HA
redis_url: redis-sentinel://sentinel1:26379,sentinel2:26379/mymaster

# Or Redis Cluster
redis_url: redis://node1:6379,node2:6379,node3:6379
```

### Stream Size and Memory Considerations

**Memory Characteristics:**

1. **Linear Growth**: Without `max_stream_length`, streams grow indefinitely
2. **Per-Query Isolation**: Each query maintains its own stream
3. **No Automatic Cleanup**: Old messages remain until explicitly trimmed
4. **Redis Memory Model**: All data in RAM

**Memory Calculation:**

```
Total Memory = num_queries × max_stream_length × avg_message_size

Example:
- 50 queries
- max_stream_length: 10,000
- avg_message_size: 3 KB
= 50 × 10,000 × 3 KB = 1.5 GB
```

**Considerations:**

- **Message Size**: Varies by query complexity and result count
  - Simple queries: 500 bytes - 2 KB
  - Complex queries with large results: 5 KB - 50 KB
  - Aggregations with tracking metadata: 10 KB+

- **Stream Overhead**: Redis metadata per message (~50-100 bytes)

- **Production Sizing**: Plan for 2-3x calculated memory for safety margin

**Best Practices:**

1. ✅ Always set `max_stream_length` in production
2. ✅ Monitor Redis memory usage: `INFO memory`
3. ✅ Set Redis `maxmemory` with appropriate eviction policy
4. ✅ Archive important results to external storage
5. ❌ Don't rely on streams for long-term storage

### CloudEvent Payload Size Limits

**Size Constraints:**

1. **Redis String Limit**: 512 MB (theoretical max)
2. **Practical Limit**: 1-10 MB recommended
3. **Network Considerations**: Large payloads increase network transfer time
4. **Deserialization Cost**: Large JSON parsing overhead

**Typical Payload Sizes:**

- **Minimal change event**: ~500 bytes (CloudEvent wrapper + small result)
- **Average change event**: 2-5 KB (moderate query results with metadata)
- **Large change event**: 50-100 KB (many results or large objects)
- **Very large change event**: 1+ MB (unusual, likely problematic)

**Problems with Large Payloads:**

1. **Memory Pressure**: Large messages consume more Redis memory
2. **Slow Publishing**: Serialization and network transfer delays
3. **Consumer Issues**: Downstream systems may struggle with large messages
4. **Retry Overhead**: Failed publishes waste bandwidth

**Mitigation Strategies:**

1. **Query Design**: Limit result set size in queries
   ```cypher
   // Instead of returning all fields:
   MATCH (n:Sensor) RETURN n

   // Return only needed fields:
   MATCH (n:Sensor) RETURN n.id, n.temperature, n.status
   ```

2. **Result Pagination**: Break large result sets into batches (requires application logic)

3. **External Storage**: Store large payloads elsewhere, publish references
   ```json
   {
     "data": {
       "kind": "change",
       "queryId": "large-query",
       "resultsRef": "s3://bucket/results/12345.json",
       "resultCount": 10000
     }
   }
   ```

4. **Compression**: Implement payload compression (requires custom consumer support)

**Warning Signs:**

- Message size >100 KB: Review query design
- Message size >1 MB: Likely architectural issue
- Frequent publish timeouts: May indicate oversized payloads

### Network Latency Impact

**Latency Sources:**

1. **Redis Round-Trip**: Query result → Redis → acknowledged
2. **Serialization**: JSON encoding overhead
3. **Network Transit**: Physical network latency
4. **Connection Pool**: Wait time for available connection

**Impact on Performance:**

```
Total publish time = serialize + network_latency + redis_processing + deserialize

Example with 50ms network latency:
- Serialize: 1ms
- Network: 50ms
- Redis XADD: 1ms
- Network return: 50ms
= 102ms per result

At 10 results/sec = ~1 second of latency overhead
At 100 results/sec = ~10 seconds of latency overhead (backlog grows)
```

**Latency Tolerance:**

- **Low latency (<10ms)**: No impact, reaction keeps up easily
- **Moderate latency (10-50ms)**: Minimal impact for typical workloads
- **High latency (50-200ms)**: May cause backlog under high throughput
- **Very high latency (>200ms)**: Significant backpressure, results queue up

**Symptoms of Latency Issues:**

- Increasing memory usage (query result channel buffering)
- Log messages about slow publishing
- Growing lag between source timestamp and publish time
- Retry exhaustion during network congestion

**Mitigation:**

1. **Colocation**: Deploy DrasiServerCore and Redis in same datacenter/region
2. **Network Quality**: Use reliable, low-latency network paths
3. **Connection Pooling**: Multiplexed connections reduce overhead (already implemented)
4. **Monitoring**: Track publish latency metrics
5. **Buffering**: Query result channel can buffer (default: 1000 results)

**Measurement:**

```bash
# Test Redis latency
$ redis-cli -h redis-host --latency

# Continuous monitoring
$ redis-cli -h redis-host --latency-history

# Intrinsic latency (Redis baseline)
$ redis-cli --intrinsic-latency 100
```

### Sequence Number Overflow

**Theoretical Issue:**

Sequence numbers are `u64` (unsigned 64-bit integers):
- Range: 0 to 18,446,744,073,709,551,615
- Max value: ~18 quintillion

**When Overflow Could Occur:**

```
Rate: 1 million results/second
Overflow time: 18,446,744,073,709,551,615 / 1,000,000 / 86,400 / 365
            ≈ 584,542 years
```

**Practical Reality:**

- **No Real Risk**: Overflow is theoretically impossible in practice
- **Restart Resets**: Reactions restart long before overflow (sequence resets to 0)
- **Not Persisted**: Sequence is in-memory only

**Behavior at Overflow (Hypothetical):**

Rust `u64` overflow behavior with default settings:
- **Debug mode**: Panic on overflow
- **Release mode**: Wraps to 0 (with overflow warning if enabled)

**If Overflow Were Possible:**

```rust
// At max value
sequence = 18,446,744,073,709,551,615

// Next increment wraps to 0
sequence += 1  // Now 0

// Consumer sees:
sequence 18446744073709551615
sequence 0  // Appears to jump backward
```

**Recommendation:**

- **Don't Worry**: Overflow is not a practical concern
- **If Paranoid**: Monitor sequence values (will never approach max in real scenarios)
- **Better Focus**: Handle sequence resets from reaction restarts

**Related Considerations:**

- Sequence resets on restart are a much more realistic concern
- Use Redis Stream message IDs for globally unique ordering
- Combine sequence with timestamps for comprehensive tracking
