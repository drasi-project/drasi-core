# SSE (Server-Sent Events) Reaction

## Purpose

The SSE reaction exposes continuous query results to browser clients and web applications via Server-Sent Events (SSE), a unidirectional HTTP streaming protocol. It provides real-time data updates from the server to connected clients using standard HTTP.

**Use Cases:**
- Live dashboards and monitoring interfaces
- Real-time notifications in web applications
- Streaming data feeds (stock tickers, sensor readings)
- Log streaming and system monitoring

## Protocol Details

SSE is a W3C standard for server-to-client streaming over HTTP:
- **Protocol**: HTTP/1.1 with `text/event-stream` content type
- **Direction**: Unidirectional (server → client only)
- **Transport**: Standard HTTP GET request with streaming response
- **Format**: Text-based events with `data:` prefix
- **Browser Support**: Native `EventSource` API in all modern browsers

### SSE vs Alternatives

| Feature | SSE | WebSocket | HTTP Polling |
|---------|-----|-----------|--------------|
| Direction | Server → Client | Bidirectional | Client → Server |
| Protocol | HTTP | WebSocket | HTTP |
| Reconnection | Automatic | Manual | N/A |
| Browser API | `EventSource` | `WebSocket` | `fetch` |
| Proxy-friendly | Yes | Sometimes | Yes |

## Configuration

Configure via `ReactionConfig` with `reaction_type: "sse"`:

```yaml
reactions:
  - id: "web-dashboard"
    reaction_type: "sse"
    queries:
      - "temperature-alerts"
    auto_start: true
    host: "0.0.0.0"              # Network interface (default: "0.0.0.0")
    port: 8080                    # HTTP port (default: 50051)
    sse_path: "/events"           # SSE endpoint path (default: "/events")
    heartbeat_interval_ms: 15000  # Heartbeat interval (default: 15000)
```

### Configuration Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `host` | String | `"0.0.0.0"` | Network interface to bind (`"0.0.0.0"` = all, `"127.0.0.1"` = localhost) |
| `port` | Number | `50051` | TCP port for HTTP server |
| `sse_path` | String | `"/events"` | HTTP path for SSE endpoint |
| `heartbeat_interval_ms` | Number | `15000` | Interval between heartbeat events (milliseconds) |

### Rust API Example

```rust
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig, RuntimeConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration
    let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    let runtime_config = Arc::new(RuntimeConfig::from(config));

    // Create and start server
    let mut core = DrasiServerCore::new(runtime_config);
    core.initialize().await?;
    core.start().await?;

    println!("SSE server running on http://0.0.0.0:8080/events");

    // Wait for shutdown
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
```

## Server Setup and Lifecycle

### Server Architecture
- **Framework**: Axum async HTTP server
- **Runtime**: Tokio async runtime
- **Broadcast**: Tokio broadcast channel (capacity: 1024 events)
- **CORS**: Permissive CORS enabled (allows all origins)
- **Heartbeat**: Separate task for periodic heartbeats
- **Keep-alive**: HTTP-level keep-alive comments every 30 seconds

### Lifecycle
1. **Create**: Configure via `ReactionConfig`
2. **Start**: HTTP server binds to `host:port`, subscribes to queries
3. **Running**: Broadcasts query results and heartbeats to all clients
4. **Stop**: Closes HTTP server, aborts background tasks

### Multi-Client Broadcast
- All connected clients receive the same events simultaneously
- Broadcast channel capacity: 1024 events
- Slow clients may drop events if buffer overflows (no backpressure)
- No event persistence or replay mechanism

## Event Format

### Query Result Event
```json
{
  "queryId": "temperature-alerts",
  "results": [
    {"id": "sensor-1", "temperature": 85.2, "location": "Building A"},
    {"id": "sensor-2", "temperature": 92.7, "location": "Building C"}
  ],
  "timestamp": 1706742123456
}
```

**Fields:**
- `queryId`: ID of the query that produced the results
- `results`: Array of query result objects (structure depends on query)
- `timestamp`: Unix timestamp in milliseconds (UTC) when event was broadcast

### Heartbeat Event
```json
{
  "type": "heartbeat",
  "ts": 1706742123456
}
```

**Fields:**
- `type`: Always `"heartbeat"`
- `ts`: Unix timestamp in milliseconds (UTC)

**Purpose:** Keep connections alive, detect server unavailability

### SSE Wire Format
Events are sent using SSE protocol:
```
data: {"queryId":"temp-alerts","results":[{"temp":85}],"timestamp":1706742123456}

data: {"type":"heartbeat","ts":1706742138456}

: keep-alive

```

Format: `data: <JSON>\n\n` (blank line terminates each event)

## Client Implementation

### JavaScript (Browser)

**Basic Connection:**
```javascript
const eventSource = new EventSource('http://localhost:8080/events');

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  if (data.type === 'heartbeat') {
    console.log('Heartbeat:', new Date(data.ts));
    return;
  }

  console.log('Query results:', data.queryId, data.results);
};

eventSource.onerror = (error) => {
  console.error('SSE error:', error);
};

// Close when done
// eventSource.close();
```

**React Component:**
```javascript
import React, { useEffect, useState } from 'react';

function LiveDashboard() {
  const [results, setResults] = useState([]);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    const eventSource = new EventSource('http://localhost:8080/events');

    eventSource.onopen = () => setConnected(true);
    eventSource.onerror = () => setConnected(false);

    eventSource.onmessage = (event) => {
      const data = JSON.parse(event.data);
      if (data.type !== 'heartbeat') {
        setResults(data.results);
      }
    };

    return () => eventSource.close();
  }, []);

  return (
    <div>
      <h1>Live Dashboard</h1>
      <p>Status: {connected ? 'Connected' : 'Disconnected'}</p>
      <ul>
        {results.map((result, i) => (
          <li key={i}>{JSON.stringify(result)}</li>
        ))}
      </ul>
    </div>
  );
}
```

**Filter by Query ID:**
```javascript
const eventSource = new EventSource('http://localhost:8080/events');

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  if (data.type === 'heartbeat') return;

  // Only process specific query
  if (data.queryId === 'temperature-alerts') {
    console.log('Temperature alerts:', data.results);
  }
};
```

### Node.js Client

```bash
npm install eventsource
```

```javascript
const EventSource = require('eventsource');

const eventSource = new EventSource('http://localhost:8080/events');

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);
  if (data.type !== 'heartbeat') {
    console.log('Results:', data.results);
  }
};

eventSource.onerror = (error) => {
  console.error('Error:', error);
};
```

### cURL (Testing)

```bash
# Stream events
curl -N http://localhost:8080/events

# With verbose headers
curl -N -v http://localhost:8080/events
```

Expected output:
```
data: {"queryId":"temperature-alerts","results":[...],"timestamp":1706742123456}

data: {"type":"heartbeat","ts":1706742138456}

: keep-alive
```

The `-N` flag disables buffering for real-time streaming.

## CORS Configuration

The SSE server is configured with permissive CORS by default:
- **Access-Control-Allow-Origin**: `*` (all origins)
- **Access-Control-Allow-Methods**: `GET`, `OPTIONS`
- **Access-Control-Allow-Headers**: `*` (all headers)

This allows browser clients from any origin to connect. For production, consider restricting origins:

```rust
// Example: Modify in mod.rs for specific origins
.allow_origin([
    "https://dashboard.example.com".parse().unwrap(),
    "https://app.example.com".parse().unwrap(),
])
```

## Troubleshooting

### Connection Refused
**Issue**: Client cannot connect

**Solutions**:
```bash
# Check if port is in use
lsof -i :8080

# Verify server is listening
curl -v http://localhost:8080/events

# Check server logs for bind errors
```

### CORS Errors
**Issue**: Browser blocks connection

**Solutions**:
- Use same origin (relative URL): `new EventSource('/events')`
- Ensure HTTPS if client page uses HTTPS (use reverse proxy)
- Check browser console for specific CORS error

### Event Loss
**Issue**: Client missing events

**Causes**:
- Slow client processing (broadcast buffer overflow)
- Network congestion
- Temporary disconnection

**Recommendations**:
- SSE provides at-most-once delivery (no guarantees)
- Use HTTP reaction with retry logic for critical events
- Reduce query result frequency at source level

### Heartbeat Timeouts
**Issue**: Connection drops during idle periods

**Solutions**:
```yaml
reactions:
  - id: "web-dashboard"
    reaction_type: "sse"
    heartbeat_interval_ms: 5000  # Reduce from 15s to 5s
```

```javascript
// Client-side monitoring
let lastHeartbeat = Date.now();

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);
  if (data.type === 'heartbeat') {
    lastHeartbeat = Date.now();
  }
};

setInterval(() => {
  if (Date.now() - lastHeartbeat > 30000) {
    console.error('Heartbeat timeout');
    eventSource.close();
    // Reconnect...
  }
}, 5000);
```

### Port Already in Use
**Issue**: Server fails to start

**Solutions**:
```bash
# Find process using port
lsof -i :8080

# Kill process or use different port in config
reactions:
  - id: "web-dashboard"
    reaction_type: "sse"
    port: 8081
```

## Limitations

### Browser Compatibility
- **Supported**: Chrome, Firefox, Safari, Edge (Chromium)
- **Not Supported**: Internet Explorer
- **Workaround**: Use [event-source-polyfill](https://github.com/Yaffle/EventSource)

```javascript
import EventSource from 'event-source-polyfill';
const eventSource = new EventSource('http://localhost:8080/events');
```

### Connection Limits
- **System**: Limited by file descriptors and memory
- **Broadcast Channel**: 1024 event buffer (slow clients drop events)
- **Browser**: ~6 concurrent SSE connections per domain
- **Recommendation**: Use reverse proxy for production scaling

### Event Ordering
- **Single Query**: Events ordered by generation time
- **Multiple Queries**: Events may interleave
- **Recommendation**: Include sequence numbers in query metadata if ordering is critical

### Network Reliability
- **Delivery**: At-most-once (no retransmission)
- **Reconnection**: Automatic via EventSource, but events sent during disconnect are lost
- **No Acknowledgment**: Server doesn't know if client received event
- **Recommendation**: Use HTTP reaction for guaranteed delivery

### Backpressure
- **No Rate Limiting**: Broadcasts events as fast as queries produce them
- **Buffer**: Fixed 1024 event capacity, slow clients drop events
- **No Backpressure**: Query processing not affected by slow clients
- **Recommendation**: Control rate at source level (`update_interval_ms`)

### One-Way Communication
- **SSE**: Server → Client only
- **Workaround**: Combine with HTTP POST for client → server messages
- **Alternative**: Use WebSocket for bidirectional communication

## References

- [W3C SSE Specification](https://html.spec.whatwg.org/multipage/server-sent-events.html)
- [MDN EventSource API](https://developer.mozilla.org/en-US/docs/Web/API/EventSource)
- [Axum SSE Documentation](https://docs.rs/axum/latest/axum/response/sse/index.html)
- [TypeSpec Schema](./sse-events.tsp) - Event payload definitions
