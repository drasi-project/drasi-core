# HTTP Source

A Drasi source plugin that exposes HTTP endpoints for receiving data change events. It provides both single-event and batch submission modes with adaptive batching for optimized throughput.

## Overview

The HTTP Source is a plugin for the Drasi continuous query system that allows applications to submit graph data changes (nodes and relations) via REST API endpoints. It features:

- **REST API Endpoints**: Simple HTTP POST interface for submitting events
- **Adaptive Batching**: Automatically adjusts batch size and timing based on throughput patterns
- **Dual Submission Modes**: Single-event and batch endpoints for different use cases
- **Universal Bootstrap**: Supports any bootstrap provider (PostgreSQL, ScriptFile, Platform, etc.)
- **Graph Data Model**: Native support for nodes and relations with labels and properties
- **Flexible Configuration**: Builder pattern or configuration struct approaches

### Key Capabilities

- Submit graph data changes via HTTP POST requests
- Automatic batch optimization based on traffic patterns
- Label-based query subscriptions and filtering
- Bootstrap initial data from external sources while streaming continues
- Health check endpoint for monitoring
- Configurable timeouts and dispatch modes

### Use Cases

- **Real-time Event Streaming**: External systems push change events to Drasi
- **Hybrid Data Loading**: Bootstrap from database, then stream changes via HTTP
- **Webhook Integration**: Receive webhook notifications as graph events
- **Manual Testing**: Submit test data via curl during development
- **API Integration**: Connect third-party services to Drasi continuous queries

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent, type-safe API for constructing HTTP sources:

```rust
use drasi_source_http::HttpSource;

// Basic HTTP source
let source = HttpSource::builder("my-source")
    .with_host("0.0.0.0")
    .with_port(8080)
    .with_auto_start(true)
    .build()?;

// With adaptive batching tuning
let source = HttpSource::builder("high-throughput-source")
    .with_host("0.0.0.0")
    .with_port(9000)
    .with_adaptive_max_batch_size(2000)
    .with_adaptive_min_batch_size(50)
    .with_adaptive_max_wait_ms(200)
    .with_adaptive_enabled(true)
    .build()?;

// With custom dispatch settings
let source = HttpSource::builder("custom-source")
    .with_host("localhost")
    .with_port(8080)
    .with_dispatch_mode(DispatchMode::Channel)
    .with_dispatch_buffer_capacity(5000)
    .build()?;

// With bootstrap provider
let source = HttpSource::builder("bootstrapped-source")
    .with_host("0.0.0.0")
    .with_port(8080)
    .with_bootstrap_provider(postgres_provider)
    .build()?;
```

### Configuration Struct Approach

Alternatively, use `HttpSourceConfig` directly:

```rust
use drasi_source_http::{HttpSource, HttpSourceConfig};

let config = HttpSourceConfig {
    host: "0.0.0.0".to_string(),
    port: 8080,
    endpoint: None,
    timeout_ms: 10000,
    adaptive_max_batch_size: Some(1000),
    adaptive_min_batch_size: Some(10),
    adaptive_max_wait_ms: Some(100),
    adaptive_min_wait_ms: Some(1),
    adaptive_window_secs: Some(5),
    adaptive_enabled: Some(true),
};

let source = HttpSource::new("my-source", config)?;
```

### YAML Configuration (DrasiServer)

When using DrasiServer, configure HTTP sources via YAML:

```yaml
sources:
  - id: "my-http-source"
    source_type: "http"
    auto_start: true
    host: "0.0.0.0"
    port: 8080
    endpoint: "/events"
    timeout_ms: 30000
    adaptive_enabled: true
    adaptive_max_batch_size: 1000
    adaptive_min_batch_size: 10
    adaptive_max_wait_ms: 100
    adaptive_min_wait_ms: 1
    adaptive_window_secs: 5
```

## Configuration Options

### Core Settings

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `host` | HTTP server host address to bind to | String | Any valid hostname or IP | **Required** |
| `port` | HTTP server port number | u16 | 1-65535 | 8080 |
| `endpoint` | Optional custom endpoint path | Option<String> | Any valid path | None |
| `timeout_ms` | Request timeout in milliseconds | u64 | Any positive integer | 10000 |
| `auto_start` | Whether to start automatically when added to DrasiLib | bool | `true`, `false` | `true` |

### Adaptive Batching Settings

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `adaptive_enabled` | Enable/disable adaptive batching | Option<bool> | true, false | true |
| `adaptive_max_batch_size` | Maximum events per batch | Option<usize> | Any positive integer | 1000 |
| `adaptive_min_batch_size` | Minimum events per batch | Option<usize> | Any positive integer | 10 |
| `adaptive_max_wait_ms` | Maximum wait time before dispatching (ms) | Option<u64> | Any positive integer | 100 |
| `adaptive_min_wait_ms` | Minimum wait time between batches (ms) | Option<u64> | Any positive integer | 1 |
| `adaptive_window_secs` | Throughput measurement window (seconds) | Option<u64> | Any positive integer | 5 |

### Dispatch Settings (Builder Only)

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `dispatch_mode` | Event routing mode | DispatchMode | Channel, Broadcast | Channel |
| `dispatch_buffer_capacity` | Buffer size for dispatch channel | usize | Any positive integer | 1000 |

### Bootstrap Settings (Builder Only)

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `bootstrap_provider` | Bootstrap provider for initial data | Box<dyn BootstrapProvider> | Any provider implementation | None |

## Input Schema

The HTTP source accepts JSON data in the `HttpSourceChange` format. All events use a tagged union structure with an `operation` field.

### Insert Operation (Node)

```json
{
  "operation": "insert",
  "element": {
    "type": "node",
    "id": "user-123",
    "labels": ["User", "Customer"],
    "properties": {
      "name": "Alice",
      "email": "alice@example.com",
      "age": 30,
      "active": true
    }
  },
  "timestamp": 1699900000000000000
}
```

### Insert Operation (Relation)

```json
{
  "operation": "insert",
  "element": {
    "type": "relation",
    "id": "follows-1",
    "labels": ["FOLLOWS"],
    "from": "user-123",
    "to": "user-456",
    "properties": {
      "since": "2024-01-01",
      "weight": 1.0
    }
  }
}
```

### Update Operation

```json
{
  "operation": "update",
  "element": {
    "type": "node",
    "id": "user-123",
    "labels": ["User", "Premium"],
    "properties": {
      "name": "Alice Updated",
      "membership": "premium"
    }
  },
  "timestamp": 1699900001000000000
}
```

### Delete Operation

```json
{
  "operation": "delete",
  "id": "user-123",
  "labels": ["User"],
  "timestamp": 1699900002000000000
}
```

### Batch Submission

```json
{
  "events": [
    {
      "operation": "insert",
      "element": {
        "type": "node",
        "id": "1",
        "labels": ["Test"],
        "properties": {}
      }
    },
    {
      "operation": "insert",
      "element": {
        "type": "node",
        "id": "2",
        "labels": ["Test"],
        "properties": {}
      }
    }
  ]
}
```

### Field Descriptions

- **operation**: Must be "insert", "update", or "delete"
- **element**: The graph element (node or relation)
  - **type**: Must be "node" or "relation"
  - **id**: Unique identifier for the element
  - **labels**: Array of label strings (e.g., ["User"], ["FOLLOWS"])
  - **properties**: JSON object with arbitrary key-value pairs
  - **from**: (Relations only) Source node ID
  - **to**: (Relations only) Target node ID
- **timestamp**: Optional nanoseconds since Unix epoch (auto-generated if omitted)

## Usage Examples

### Starting an HTTP Source

```rust
use drasi_source_http::HttpSource;
use drasi_lib::Source;

// Create and start the source
let source = HttpSource::builder("my-source")
    .with_host("0.0.0.0")
    .with_port(8080)
    .build()?;

source.start().await?;

// Check status
assert_eq!(source.status().await, ComponentStatus::Running);
```

### Submitting Events via curl

```bash
# Submit single node
curl -X POST http://localhost:8080/sources/my-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "insert",
    "element": {
      "type": "node",
      "id": "user-1",
      "labels": ["User"],
      "properties": {"name": "Alice"}
    }
  }'

# Submit batch of events
curl -X POST http://localhost:8080/sources/my-source/events/batch \
  -H "Content-Type: application/json" \
  -d '{
    "events": [
      {
        "operation": "insert",
        "element": {
          "type": "node",
          "id": "user-1",
          "labels": ["User"],
          "properties": {"name": "Alice"}
        }
      },
      {
        "operation": "insert",
        "element": {
          "type": "node",
          "id": "user-2",
          "labels": ["User"],
          "properties": {"name": "Bob"}
        }
      }
    ]
  }'

# Health check
curl http://localhost:8080/health
```

### Python Example

```python
import requests
import json

# Submit a node
event = {
    "operation": "insert",
    "element": {
        "type": "node",
        "id": "sensor-42",
        "labels": ["Sensor", "IoT"],
        "properties": {
            "temperature": 72.5,
            "location": "Building A",
            "active": True
        }
    }
}

response = requests.post(
    "http://localhost:8080/sources/my-source/events",
    json=event
)

print(response.json())
# {"success": true, "message": "All 1 events processed successfully"}
```

### JavaScript/Node.js Example

```javascript
const axios = require('axios');

async function submitEvent() {
  const event = {
    operation: 'insert',
    element: {
      type: 'node',
      id: 'product-123',
      labels: ['Product'],
      properties: {
        name: 'Widget',
        price: 29.99,
        inStock: true
      }
    }
  };

  const response = await axios.post(
    'http://localhost:8080/sources/my-source/events',
    event
  );

  console.log(response.data);
}
```

### Complete Integration Example

```rust
use drasi_source_http::HttpSource;
use drasi_lib::{DrasiLib, Query};

#[tokio::main]
async fn main() -> Result<()> {
    // Create HTTP source
    let http_source = HttpSource::builder("http-source")
        .with_host("0.0.0.0")
        .with_port(8080)
        .with_adaptive_enabled(true)
        .build()?;

    // Create continuous query
    let query = Query::cypher("active-users")
        .query("MATCH (u:User) WHERE u.active = true RETURN u.name")
        .from_source("http-source")
        .build();

    // Initialize Drasi
    let drasi = DrasiLib::new()
        .with_source(http_source)
        .with_query(query)
        .build()
        .await?;

    // Start processing
    drasi.start().await?;

    // Now submit events via HTTP POST to localhost:8080
    // Query results will update automatically as events arrive

    Ok(())
}
```

## Endpoints

The HTTP source exposes the following endpoints:

| Method | Path | Description |
|--------|------|-------------|
| POST | `/sources/{source_id}/events` | Submit a single event |
| POST | `/sources/{source_id}/events/batch` | Submit multiple events |
| GET | `/health` | Health check (returns service status and features) |

### Response Format

Success:
```json
{
  "success": true,
  "message": "All 2 events processed successfully",
  "error": null
}
```

Partial success (batch):
```json
{
  "success": true,
  "message": "Processed 8 events successfully, 2 failed",
  "error": "Invalid element type"
}
```

Error:
```json
{
  "success": false,
  "message": "All 1 events failed",
  "error": "Source name mismatch"
}
```

## Adaptive Batching

The HTTP source includes intelligent batching that automatically adjusts based on throughput:

### Throughput Levels

| Level | Messages/Second | Batch Size | Wait Time |
|-------|----------------|------------|-----------|
| Idle | < 1 | Minimum | Minimum (1ms) |
| Low | 1-100 | Small (2x min) | 1ms |
| Medium | 100-1,000 | Moderate (25% of max) | 10ms |
| High | 1,000-10,000 | Large (50% of max) | 25ms |
| Burst | > 10,000 | Maximum | 50ms |

### Tuning Guidelines

**For low latency** (real-time dashboards):
```rust
.with_adaptive_max_wait_ms(10)
.with_adaptive_min_batch_size(1)
```

**For high throughput** (bulk data ingestion):
```rust
.with_adaptive_max_batch_size(5000)
.with_adaptive_max_wait_ms(500)
```

**To disable adaptive batching**:
```rust
.with_adaptive_enabled(false)
```

## Bootstrap Providers

The HTTP source supports universal bootstrap - any bootstrap provider can be used to load initial data before streaming begins.

### Common Patterns

**Bootstrap from PostgreSQL, stream via HTTP**:
```rust
let postgres_provider = PostgresBootstrapProvider::new(config)?;

let source = HttpSource::builder("http-source")
    .with_host("0.0.0.0")
    .with_port(8080)
    .with_bootstrap_provider(postgres_provider)
    .build()?;
```

**Bootstrap from files**:
```rust
let file_provider = ScriptFileBootstrapProvider::new(file_paths)?;

let source = HttpSource::builder("http-source")
    .with_host("0.0.0.0")
    .with_port(8080)
    .with_bootstrap_provider(file_provider)
    .build()?;
```

### Bootstrap Behavior

- Bootstrap runs asynchronously in a separate task
- Streaming events are processed immediately
- Queries receive both bootstrap and streaming data
- Bootstrap provider properties are passed via the source's generic properties map

## Error Handling

### Common Errors

**Port already in use**:
```
Failed to bind HTTP server to 0.0.0.0:8080: Address already in use
```
Solution: Change port or stop conflicting service

**Invalid JSON**:
```json
{"success": false, "message": "Failed to parse JSON", "error": "..."}
```
Solution: Validate JSON structure against schema

**Source name mismatch**:
```json
{"success": false, "message": "Source name mismatch", "error": "Expected 'my-source', got 'wrong-source'"}
```
Solution: Ensure URL path matches source ID

**Validation errors**:
- Port cannot be 0
- Timeout cannot be 0
- Min batch size cannot exceed max batch size
- Min wait time cannot exceed max wait time

## Testing

### Unit Tests

```bash
# Run all HTTP source tests
cargo test -p drasi-source-http

# Run specific test module
cargo test -p drasi-source-http construction

# Run with logging
RUST_LOG=debug cargo test -p drasi-source-http -- --nocapture
```

### Integration Testing

```bash
# Start test server
cargo run --example http_source_example

# In another terminal, submit test events
curl -X POST http://localhost:8080/sources/test-source/events \
  -H "Content-Type: application/json" \
  -d '{"operation":"insert","element":{"type":"node","id":"1","labels":["Test"],"properties":{}}}'
```

## Performance Considerations

### Channel Capacity

The internal batch channel capacity is automatically calculated as `max_batch_size × 5`:

| Max Batch Size | Channel Capacity | Memory (1KB/event) |
|----------------|------------------|--------------------|
| 100 | 500 | ~500 KB |
| 1,000 | 5,000 | ~5 MB |
| 5,000 | 25,000 | ~25 MB |

### Dispatch Modes

- **Channel** (default): Isolated channels per subscriber with backpressure, zero message loss
- **Broadcast**: Shared channel, no backpressure, possible message loss under high load

### Best Practices

1. **Use batch endpoint** for bulk operations (reduces HTTP overhead)
2. **Enable adaptive batching** for variable traffic patterns
3. **Tune batch sizes** based on your throughput requirements
4. **Monitor health endpoint** for production deployments
5. **Use Channel dispatch mode** when message reliability is critical

## Architecture Notes

### Internal Structure

- **HttpSource**: Main plugin implementation (lib.rs)
- **HttpSourceConfig**: Configuration struct (config.rs)
- **HttpElement/HttpSourceChange**: Event models (models.rs)
- **AdaptiveBatcher**: Batching logic (adaptive_batcher.rs)
- **Time utilities**: Timestamp handling (time.rs)

### Data Flow

1. HTTP POST request arrives at Axum endpoint
2. JSON deserialized to `HttpSourceChange`
3. Converted to `drasi_core::models::SourceChange`
4. Sent to adaptive batcher via mpsc channel
5. Batcher accumulates events based on throughput
6. Batch forwarded to dispatchers
7. Dispatchers route to subscribed queries

### Thread Safety

- All operations are async using Tokio
- Shared state protected by Arc<RwLock<_>>
- Channel-based communication for event flow
- Component status tracked with atomic updates

## See Also

- [Drasi Core Documentation](../../README.md)
- [Source Plugin API](../../../lib/src/plugin_core/source.rs)
- [Bootstrap Providers](../../../lib/src/bootstrap/)
- [Continuous Queries](../../../query-cypher/)
