# SSE Reaction

Server-Sent Events (SSE) reaction plugin for Drasi that streams continuous query results to browser clients in real-time.

## Overview

The SSE Reaction component exposes Drasi continuous query results to web clients via Server-Sent Events, enabling real-time data streaming over HTTP. SSE provides a simple, unidirectional push-based communication channel from server to clients, making it ideal for scenarios where clients need to receive continuous updates without polling.

### Key Capabilities

- **Real-time streaming**: Automatically pushes query result changes to connected clients
- **Browser-native**: Uses standard SSE protocol supported by all modern browsers via EventSource API
- **Multi-client broadcast**: Efficiently broadcasts events to multiple concurrent clients
- **Automatic heartbeats**: Keeps connections alive with configurable heartbeat messages
- **CORS enabled**: Configured to allow cross-origin requests from any domain
- **Timestamp tracking**: All events include millisecond-precision timestamps
- **Priority queue processing**: Ensures events are processed in timestamp order

### Use Cases

- Real-time dashboards and monitoring applications
- Live data visualization and analytics
- Event-driven notifications in web applications
- Streaming sensor data to browser clients
- Live query result updates for continuous queries
- Push-based notifications for data changes

## Configuration

The SSE Reaction can be configured using either the builder pattern (recommended) or the config struct approach.

### Builder Pattern (Recommended)

```rust
use drasi_reaction_sse::SseReaction;

let reaction = SseReaction::builder("my-sse-reaction")
    .with_host("0.0.0.0")
    .with_port(8080)
    .with_sse_path("/events")
    .with_heartbeat_interval_ms(30000)
    .with_queries(vec!["sensor-data".to_string(), "alerts".to_string()])
    .with_priority_queue_capacity(1000)
    .with_auto_start(true)
    .build()?;
```

### Config Struct Approach

```rust
use drasi_reaction_sse::{SseReaction, SseReactionConfig};

let config = SseReactionConfig {
    host: "0.0.0.0".to_string(),
    port: 8080,
    sse_path: "/events".to_string(),
    heartbeat_interval_ms: 30000,
};

let reaction = SseReaction::new(
    "my-sse-reaction",
    vec!["sensor-data".to_string()],
    config
);
```

### Configuration Options

| Option | Description | Type | Valid Values | Default |
|--------|-------------|------|--------------|---------|
| `id` | Unique identifier for the reaction | String | Any valid string | Required |
| `host` | Host address to bind the SSE server | String | Valid IP address or hostname | `"0.0.0.0"` |
| `port` | Port number to bind the SSE server | u16 | 1-65535 | `8080` |
| `sse_path` | HTTP path for SSE endpoint | String | Valid URL path | `"/events"` |
| `heartbeat_interval_ms` | Interval between heartbeat messages in milliseconds | u64 | > 0 | `30000` (30 seconds) |
| `queries` | List of query IDs to subscribe to | Vec&lt;String&gt; | Valid query identifiers | `[]` |
| `priority_queue_capacity` | Custom capacity for priority queue (optional) | usize | > 0 | Auto-configured |
| `auto_start` | Whether to start automatically when added | bool | true/false | `true` |

## Output Schema

The SSE Reaction emits two types of events:

### Query Result Event

Sent whenever subscribed queries produce new results:

```json
{
  "queryId": "sensor-data",
  "results": [
    {
      "id": "sensor-1",
      "temperature": 85.2,
      "timestamp": "2025-12-05T10:30:00Z"
    },
    {
      "id": "sensor-2",
      "temperature": 92.7,
      "timestamp": "2025-12-05T10:30:01Z"
    }
  ],
  "timestamp": 1706742123456
}
```

**Fields:**
- `queryId` (string): The ID of the query that produced the results
- `results` (array): Array of result objects from the query
- `timestamp` (number): Unix timestamp in milliseconds when the event was generated

### Heartbeat Event

Sent at regular intervals to keep connections alive:

```json
{
  "type": "heartbeat",
  "ts": 1706742123456
}
```

**Fields:**
- `type` (string): Always `"heartbeat"` for heartbeat events
- `ts` (number): Unix timestamp in milliseconds when the heartbeat was sent

## Usage Examples

### Basic Usage with Single Query

```rust
use drasi_reaction_sse::SseReaction;

// Create SSE reaction for a single query
let reaction = SseReaction::builder("temperature-monitor")
    .with_query("high-temp-sensors")
    .with_port(8080)
    .build()?;

// The reaction will be available at http://0.0.0.0:8080/events
```

### Multiple Queries with Custom Configuration

```rust
use drasi_reaction_sse::SseReaction;

let reaction = SseReaction::builder("multi-query-sse")
    .with_queries(vec![
        "sensor-data".to_string(),
        "alert-events".to_string(),
        "system-metrics".to_string(),
    ])
    .with_host("localhost")
    .with_port(9090)
    .with_sse_path("/api/stream")
    .with_heartbeat_interval_ms(15000)  // 15 second heartbeats
    .build()?;

// Available at http://localhost:9090/api/stream
```

### Custom Priority Queue Capacity

```rust
use drasi_reaction_sse::SseReaction;

// For high-volume scenarios, configure a larger priority queue
let reaction = SseReaction::builder("high-volume-sse")
    .with_query("rapid-events")
    .with_priority_queue_capacity(10000)
    .build()?;
```

### Integration with DrasiLib

```rust
use drasi_lib::DrasiLib;
use drasi_reaction_sse::SseReaction;

let drasi = DrasiLib::new()
    .add_query(my_query)
    .add_reaction(
        SseReaction::builder("web-dashboard")
            .with_query("dashboard-data")
            .with_port(8080)
            .build()?
    )
    .build()
    .await?;
```

### Client-Side JavaScript Example

```javascript
// Connect to SSE endpoint
const eventSource = new EventSource('http://localhost:8080/events');

// Handle query result events
eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  if (data.type === 'heartbeat') {
    console.log('Heartbeat received at', data.ts);
  } else {
    console.log('Query results from', data.queryId);
    console.log('Results:', data.results);
    console.log('Timestamp:', data.timestamp);

    // Update your UI with the new data
    updateDashboard(data.results);
  }
};

// Handle connection events
eventSource.onerror = (error) => {
  console.error('SSE connection error:', error);
};

eventSource.onopen = () => {
  console.log('SSE connection opened');
};
```

### Python Client Example

```python
import sseclient
import json
import requests

def stream_events():
    response = requests.get('http://localhost:8080/events', stream=True)
    client = sseclient.SSEClient(response)

    for event in client.events():
        data = json.loads(event.data)

        if data.get('type') == 'heartbeat':
            print(f"Heartbeat at {data['ts']}")
        else:
            print(f"Query: {data['queryId']}")
            print(f"Results: {data['results']}")
            print(f"Timestamp: {data['timestamp']}")

if __name__ == '__main__':
    stream_events()
```

## Architecture Details

### Event Processing Flow

1. **Query Subscription**: Upon start, the reaction subscribes to all configured queries
2. **Priority Queue**: Query results are queued in timestamp order to maintain event ordering
3. **Broadcasting**: Results are broadcast to all connected SSE clients via a Tokio broadcast channel
4. **Heartbeats**: A separate task sends periodic heartbeat messages to keep connections alive
5. **HTTP Server**: Axum HTTP server handles SSE connections with CORS enabled

### Multi-Client Broadcast

The SSE Reaction uses Tokio's broadcast channel to efficiently distribute events to multiple clients:

- Channel capacity: 1024 messages
- Late subscribers receive new events only (no replay)
- Slow clients may experience message lag if they fall too far behind
- Client disconnections are handled automatically

### CORS Configuration

The SSE server is configured with permissive CORS to allow browser connections from any origin:

- Allowed origins: Any (`*`)
- Allowed methods: GET, OPTIONS
- Allowed headers: Any

### Connection Keep-Alive

Two mechanisms ensure connection stability:

1. **Heartbeat messages**: Sent at configured intervals (default 30 seconds)
2. **SSE keep-alive**: Axum's built-in keep-alive with 30-second intervals

## Error Handling

The SSE Reaction handles various error conditions gracefully:

- **Port binding failures**: Logged but don't prevent startup
- **Client disconnections**: Automatically cleaned up
- **Broadcast channel full**: Old messages are dropped (lagging clients)
- **No connected clients**: Messages are dropped without error
- **Slow clients**: May receive lag errors if they fall too far behind

## Performance Considerations

### Scalability

- **Client count**: Tested with dozens of concurrent clients
- **Message throughput**: Handles high-frequency query results via priority queue
- **Memory usage**: Broadcast channel has fixed 1024 message capacity
- **CPU usage**: Minimal overhead for broadcasting

### Best Practices

1. **Heartbeat interval**: Balance between connection stability and bandwidth
   - Too short: Unnecessary bandwidth usage
   - Too long: Connections may timeout

2. **Priority queue capacity**: Size based on expected query result frequency
   - High-frequency queries: Increase capacity (e.g., 10000)
   - Low-frequency queries: Default is sufficient

3. **Number of queries**: Multiple queries share the same SSE connection
   - Clients receive all results from all subscribed queries
   - Filter on client side if needed

4. **Network considerations**: SSE uses HTTP/1.1 with chunked encoding
   - Works through most firewalls and proxies
   - Browser limits: ~6 concurrent SSE connections per domain

## Troubleshooting

### Common Issues

**SSE connection immediately closes:**
- Check that the server is running and the port is accessible
- Verify firewall rules allow inbound connections
- Check browser console for CORS errors

**No events received:**
- Verify queries are producing results
- Check query subscriptions are correct
- Review server logs for processing errors

**Heartbeat messages but no query results:**
- Confirm queries are configured and running
- Check query IDs match between reaction and actual queries
- Verify queries are producing output

**Client shows lag errors:**
- Increase broadcast channel capacity
- Reduce query result frequency
- Consider multiple SSE reactions for different query groups

## Dependencies

- `drasi-lib`: Core Drasi library for reaction framework
- `axum`: HTTP server for SSE endpoints
- `tower-http`: CORS middleware
- `tokio`: Async runtime and broadcast channels
- `tokio-stream`: Stream utilities for SSE
- `serde_json`: JSON serialization
- `chrono`: Timestamp generation
- `log`: Logging framework

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0. See LICENSE file for details.
