# SSE (Server-Sent Events) Reaction

## Purpose

The SSE (Server-Sent Events) Reaction exposes continuous query results to browser clients and web applications via a unidirectional HTTP streaming protocol. It provides a lightweight, standardized mechanism for pushing real-time data updates from the server to connected clients.

### What the SSE Reaction Does

The SSE reaction:
- Starts an Axum-based HTTP server that exposes an SSE endpoint
- Receives continuous query results from the DrasiServerCore query engine
- Broadcasts these results to all connected SSE clients in real-time
- Maintains client connections with periodic heartbeat messages
- Filters query results based on subscribed query IDs
- Supports multiple concurrent client connections via a broadcast channel

### Real-Time Web Application Scenarios

SSE is ideal for scenarios where you need to push server updates to web clients:

- **Live Dashboards**: Stream real-time metrics, KPIs, or monitoring data to web dashboards
- **Notification Systems**: Push notifications to browser-based applications
- **Live Data Feeds**: Stock tickers, sensor readings, event streams
- **Collaborative Tools**: Push updates when shared data changes
- **IoT Monitoring**: Stream device status and telemetry to web interfaces
- **Log Streaming**: Display real-time logs in web-based admin consoles

### When to Use SSE vs Other Protocols

**Use SSE when:**
- You need unidirectional (server-to-client) data push
- Clients are web browsers with native EventSource support
- You want a simple, HTTP-based streaming protocol
- Automatic reconnection is desired (built into EventSource)
- You're streaming text-based data (JSON events)
- You want to leverage existing HTTP infrastructure (proxies, load balancers)

**Use WebSockets instead when:**
- You need bidirectional communication (client can send data back)
- You need lower latency or higher throughput
- You're streaming binary data
- You need more control over connection lifecycle

**Use HTTP Reaction instead when:**
- You need to push to external services (webhooks)
- Clients should pull data on-demand rather than receive pushes
- You need request/response patterns rather than streaming

**Use gRPC Reaction instead when:**
- You need efficient binary serialization
- You're integrating with non-browser clients (backend services)
- You want strongly-typed schemas (Protocol Buffers)

## Configuration Properties

The SSE reaction is configured using the `ReactionConfig` structure with `reaction_type: "sse"`. Configuration properties are specified in the `properties` map.

| Property | Type | Default | Required | Description |
|----------|------|---------|----------|-------------|
| `host` | String | `"0.0.0.0"` | No | The network interface address to bind the HTTP server to. Use `"0.0.0.0"` to listen on all interfaces, or `"127.0.0.1"` for localhost only. |
| `port` | Number | `50051` | No | The TCP port number for the HTTP server. Must be available and not blocked by firewall. |
| `sse_path` | String | `"/events"` | No | The HTTP path where the SSE endpoint will be exposed. Clients connect to `http://host:port/sse_path`. |
| `heartbeat_interval_ms` | Number | `15000` | No | Interval in milliseconds between heartbeat events. Heartbeats keep connections alive and help detect disconnections. Minimum recommended: 5000ms. |

### Property Details

#### host
- **Type**: String
- **Default**: `"0.0.0.0"`
- **Description**: Network interface to bind to. `"0.0.0.0"` allows connections from any network interface. Use `"127.0.0.1"` to restrict to localhost-only connections for development/testing.

#### port
- **Type**: Number (u16)
- **Default**: `50051`
- **Description**: TCP port for the HTTP server. Ensure the port is not in use by another service. Common ports: 8080, 3000, 5000.

#### sse_path
- **Type**: String
- **Default**: `"/events"`
- **Description**: URL path where SSE endpoint is exposed. Can include path segments (e.g., `"/api/v1/events"`). Must start with `/`.

#### heartbeat_interval_ms
- **Type**: Number (u64)
- **Default**: `15000` (15 seconds)
- **Description**: Milliseconds between heartbeat events. Heartbeats prevent connection timeouts and help clients detect server unavailability. Too frequent causes unnecessary traffic; too infrequent may cause proxy timeouts.

## Configuration Examples

### YAML Configuration

```yaml
server_core:
  id: "my-drasi-instance"

sources:
  - id: "sensor-data"
    source_type: "mock"
    auto_start: true
    properties:
      update_interval_ms: 1000

queries:
  - id: "temperature-alerts"
    query: "MATCH (s:Sensor) WHERE s.temperature > 80 RETURN s.id, s.temperature, s.location"
    queryLanguage: Cypher
    sources:
      - "sensor-data"
    auto_start: true

reactions:
  - id: "web-dashboard"
    reaction_type: "sse"
    queries:
      - "temperature-alerts"
    auto_start: true
    properties:
      host: "0.0.0.0"
      port: 8080
      sse_path: "/events"
      heartbeat_interval_ms: 15000
```

### JSON Configuration

```json
{
  "server_core": {
    "id": "my-drasi-instance"
  },
  "sources": [
    {
      "id": "sensor-data",
      "source_type": "mock",
      "auto_start": true,
      "properties": {
        "update_interval_ms": 1000
      }
    }
  ],
  "queries": [
    {
      "id": "temperature-alerts",
      "query": "MATCH (s:Sensor) WHERE s.temperature > 80 RETURN s.id, s.temperature, s.location",
      "queryLanguage": "Cypher",
      "sources": ["sensor-data"],
      "auto_start": true
    }
  ],
  "reactions": [
    {
      "id": "web-dashboard",
      "reaction_type": "sse",
      "queries": ["temperature-alerts"],
      "auto_start": true,
      "properties": {
        "host": "0.0.0.0",
        "port": 8080,
        "sse_path": "/events",
        "heartbeat_interval_ms": 15000
      }
    }
  ]
}
```

### Minimal Configuration

```yaml
reactions:
  - id: "simple-sse"
    reaction_type: "sse"
    queries:
      - "my-query"
    # All properties optional - uses defaults
```

### Multiple SSE Endpoints

```yaml
reactions:
  # Public dashboard
  - id: "public-dashboard"
    reaction_type: "sse"
    queries:
      - "public-metrics"
    properties:
      host: "0.0.0.0"
      port: 8080
      sse_path: "/public/events"

  # Internal monitoring
  - id: "internal-monitoring"
    reaction_type: "sse"
    queries:
      - "internal-metrics"
      - "system-health"
    properties:
      host: "127.0.0.1"
      port: 8081
      sse_path: "/internal/events"
      heartbeat_interval_ms: 10000
```

## Programmatic Construction in Rust

### Creating the Reaction Configuration

```rust
use drasi_server_core::{
    DrasiServerCore, DrasiServerCoreConfig,
    ReactionConfig, QueryConfig, SourceConfig,
};
use std::collections::HashMap;
use serde_json::json;

// Create SSE reaction configuration
let mut sse_properties = HashMap::new();
sse_properties.insert("host".to_string(), json!("0.0.0.0"));
sse_properties.insert("port".to_string(), json!(8080));
sse_properties.insert("sse_path".to_string(), json!("/events"));
sse_properties.insert("heartbeat_interval_ms".to_string(), json!(15000));

let sse_reaction = ReactionConfig {
    id: "web-dashboard".to_string(),
    reaction_type: "sse".to_string(),
    queries: vec!["my-query".to_string()],
    auto_start: true,
    properties: sse_properties,
};

// Build complete configuration
let config = DrasiServerCoreConfig {
    server_core: Default::default(),
    sources: vec![/* source configs */],
    queries: vec![/* query configs */],
    reactions: vec![sse_reaction],
};
```

### Starting the SSE Server

```rust
use drasi_server_core::{DrasiServerCore, RuntimeConfig};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load or create configuration
    let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    let runtime_config = Arc::new(RuntimeConfig::from(config));

    // Create and initialize core
    let mut core = DrasiServerCore::new(runtime_config);
    core.initialize().await?;

    // Start all components (sources, queries, reactions)
    core.start().await?;

    // The SSE server is now running on the configured host:port
    println!("SSE server started on http://0.0.0.0:8080/events");

    // Keep running until shutdown signal
    tokio::signal::ctrl_c().await?;

    // Cleanup
    core.stop().await?;
    Ok(())
}
```

### Handling Server Lifecycle

```rust
use drasi_server_core::{DrasiServerCore, RuntimeConfig};
use std::sync::Arc;

async fn manage_sse_lifecycle() -> anyhow::Result<()> {
    let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    let runtime_config = Arc::new(RuntimeConfig::from(config));

    let mut core = DrasiServerCore::new(runtime_config);
    core.initialize().await?;

    // Start with auto_start enabled reactions
    core.start().await?;

    // Programmatically stop a specific reaction
    core.stop_reaction("web-dashboard").await?;

    // Restart the reaction
    core.start_reaction("web-dashboard").await?;

    // Check reaction status
    let status = core.get_reaction_status("web-dashboard").await?;
    println!("SSE Reaction Status: {:?}", status);

    // Shutdown everything
    core.stop().await?;

    Ok(())
}
```

### Dynamic Reaction Creation

```rust
use drasi_server_core::api::DrasiServerCoreHandle;

async fn create_sse_reaction_dynamically(
    core_handle: &DrasiServerCoreHandle
) -> anyhow::Result<()> {
    let mut properties = HashMap::new();
    properties.insert("host".to_string(), json!("0.0.0.0"));
    properties.insert("port".to_string(), json!(9090));
    properties.insert("sse_path".to_string(), json!("/streaming"));

    let reaction_config = ReactionConfig {
        id: "dynamic-sse".to_string(),
        reaction_type: "sse".to_string(),
        queries: vec!["my-query".to_string()],
        auto_start: true,
        properties,
    };

    // Add and start the reaction
    core_handle.add_reaction(reaction_config).await?;
    core_handle.start_reaction("dynamic-sse").await?;

    Ok(())
}
```

## Input Data Format

The SSE reaction receives continuous query results through the `QueryResult` structure from the DrasiServerCore query engine.

### QueryResult Structure

```rust
pub struct QueryResult {
    /// Unique identifier of the query that produced this result
    pub query_id: String,

    /// Timestamp when the result was generated
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Array of result records (JSON objects)
    pub results: Vec<serde_json::Value>,

    /// Additional metadata about the query execution
    pub metadata: HashMap<String, serde_json::Value>,

    /// Optional profiling information for performance tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiling: Option<ProfilingMetadata>,
}
```

### Field Descriptions

- **query_id**: The ID of the query that generated this result. The SSE reaction uses this to filter results based on its configured query subscriptions.

- **timestamp**: UTC timestamp indicating when the query produced this result set. This is the query engine's timestamp, not the SSE server's broadcast time.

- **results**: Array of query result records. Each record is a JSON object containing the fields returned by the Cypher query. For example, a query like `MATCH (s:Sensor) RETURN s.id, s.temperature` produces objects like `{"s.id": "sensor-1", "s.temperature": 78.5}`.

- **metadata**: Additional context about the query execution. May include performance tracking, sequence numbers, or custom metadata.

- **profiling**: Optional performance profiling data tracked by the system for monitoring query execution time and resource usage.

### Example Input

```rust
QueryResult {
    query_id: "temperature-alerts".to_string(),
    timestamp: chrono::Utc::now(),
    results: vec![
        json!({
            "id": "sensor-001",
            "temperature": 85.2,
            "location": "Building A"
        }),
        json!({
            "id": "sensor-042",
            "temperature": 92.7,
            "location": "Building C"
        })
    ],
    metadata: HashMap::from([
        ("sequence".to_string(), json!(42))
    ]),
    profiling: None,
}
```

## Output Data Format

The SSE reaction broadcasts events to connected clients using the Server-Sent Events protocol. Events are sent as plain text with the SSE event format.

### SSE Event Structure

Server-Sent Events follow this text format:

```
data: <JSON payload>

```

Each event consists of:
- A `data:` field containing the JSON payload
- A blank line terminating the event

Multiple `data:` lines can be used for multi-line payloads, but the SSE reaction uses single-line JSON.

### Query Result Event Payload

When the SSE reaction receives a `QueryResult`, it broadcasts a JSON event with this structure:

```json
{
  "queryId": "temperature-alerts",
  "results": [
    {
      "id": "sensor-001",
      "temperature": 85.2,
      "location": "Building A"
    },
    {
      "id": "sensor-042",
      "temperature": 92.7,
      "location": "Building C"
    }
  ],
  "timestamp": 1706742123456
}
```

**Field Descriptions:**

- **queryId** (string): The ID of the query that produced these results. Clients can filter events based on this field.

- **results** (array): Array of result records from the query. Each element is a JSON object containing the fields returned by the Cypher query.

- **timestamp** (number): Unix timestamp in milliseconds (UTC) when the event was broadcast by the SSE server.

### Heartbeat Event

The SSE reaction sends periodic heartbeat events to keep connections alive and help clients detect disconnections:

```json
{
  "type": "heartbeat",
  "ts": 1706742123456
}
```

**Field Descriptions:**

- **type** (string): Always `"heartbeat"` for heartbeat events.

- **ts** (number): Unix timestamp in milliseconds (UTC) when the heartbeat was sent.

Heartbeats are sent at the interval specified by `heartbeat_interval_ms` (default: 15 seconds).

### Keep-Alive Comments

In addition to heartbeat events, the Axum SSE implementation sends keep-alive comments every 30 seconds:

```
: keep-alive

```

These are SSE comment lines (prefixed with `:`) that are ignored by EventSource but keep the HTTP connection active through proxies and firewalls.

### Complete SSE Stream Example

```
data: {"queryId":"temperature-alerts","results":[{"id":"sensor-001","temperature":85.2}],"timestamp":1706742123456}

data: {"type":"heartbeat","ts":1706742138456}

: keep-alive

data: {"queryId":"temperature-alerts","results":[{"id":"sensor-001","temperature":87.1}],"timestamp":1706742153789}

data: {"type":"heartbeat","ts":1706742168456}

: keep-alive
```

### Event Ordering and Delivery

- **Broadcast to All Clients**: Events are broadcast to all connected clients simultaneously via a Tokio broadcast channel (capacity: 1024 events).

- **No Persistence**: Events are not persisted. If a client connects after an event is broadcast, it will not receive that event.

- **Filtering**: The SSE reaction only broadcasts results from queries in its configured `queries` list. Other query results are ignored.

- **Lagging Clients**: If a client cannot keep up with the event rate, the broadcast channel may drop events for that slow client. The client will not receive an error; events are simply lost.

- **No Ordering Guarantees**: Event ordering is preserved within a single query's results, but if multiple queries produce results concurrently, their events may interleave.

## Client Connection

### JavaScript/Browser Client Example

The browser's native `EventSource` API provides built-in SSE support with automatic reconnection.

#### Basic Connection

```javascript
// Connect to the SSE endpoint
const eventSource = new EventSource('http://localhost:8080/events');

// Listen for messages
eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  // Check event type
  if (data.type === 'heartbeat') {
    console.log('Heartbeat received:', new Date(data.ts));
    return;
  }

  // Handle query results
  console.log('Query ID:', data.queryId);
  console.log('Results:', data.results);
  console.log('Timestamp:', new Date(data.timestamp));

  // Process results
  data.results.forEach(result => {
    console.log('Result:', result);
  });
};

// Handle connection open
eventSource.onopen = () => {
  console.log('SSE connection established');
};

// Handle errors
eventSource.onerror = (error) => {
  console.error('SSE error:', error);
  if (eventSource.readyState === EventSource.CLOSED) {
    console.log('Connection closed by server');
  } else {
    console.log('Connection error, will retry...');
  }
};

// Close connection when done
// eventSource.close();
```

#### Filtering by Query ID

```javascript
const eventSource = new EventSource('http://localhost:8080/events');

const targetQueryId = 'temperature-alerts';

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  // Ignore heartbeats
  if (data.type === 'heartbeat') return;

  // Filter by query ID
  if (data.queryId !== targetQueryId) {
    console.log('Ignoring result from query:', data.queryId);
    return;
  }

  // Process only matching query results
  console.log('Temperature alerts:', data.results);
  updateDashboard(data.results);
};
```

#### React Component Example

```javascript
import React, { useEffect, useState } from 'react';

function LiveDashboard() {
  const [results, setResults] = useState([]);
  const [connected, setConnected] = useState(false);

  useEffect(() => {
    const eventSource = new EventSource('http://localhost:8080/events');

    eventSource.onopen = () => {
      setConnected(true);
      console.log('Connected to SSE stream');
    };

    eventSource.onmessage = (event) => {
      const data = JSON.parse(event.data);

      // Ignore heartbeats
      if (data.type === 'heartbeat') return;

      // Update state with new results
      setResults(data.results);
    };

    eventSource.onerror = () => {
      setConnected(false);
      console.error('SSE connection error');
    };

    // Cleanup on unmount
    return () => {
      eventSource.close();
    };
  }, []);

  return (
    <div>
      <h1>Live Dashboard</h1>
      <p>Status: {connected ? 'Connected' : 'Disconnected'}</p>
      <ul>
        {results.map((result, index) => (
          <li key={index}>{JSON.stringify(result)}</li>
        ))}
      </ul>
    </div>
  );
}

export default LiveDashboard;
```

#### Advanced: Custom Reconnection Logic

```javascript
class SSEClient {
  constructor(url, options = {}) {
    this.url = url;
    this.maxRetries = options.maxRetries || Infinity;
    this.retryDelay = options.retryDelay || 3000;
    this.retryCount = 0;
    this.eventSource = null;
    this.listeners = [];
  }

  connect() {
    this.eventSource = new EventSource(this.url);

    this.eventSource.onopen = () => {
      console.log('SSE connected');
      this.retryCount = 0;
    };

    this.eventSource.onmessage = (event) => {
      const data = JSON.parse(event.data);
      this.listeners.forEach(listener => listener(data));
    };

    this.eventSource.onerror = () => {
      console.error('SSE error');
      this.eventSource.close();
      this.reconnect();
    };
  }

  reconnect() {
    if (this.retryCount >= this.maxRetries) {
      console.error('Max retries reached');
      return;
    }

    this.retryCount++;
    console.log(`Reconnecting in ${this.retryDelay}ms (attempt ${this.retryCount})...`);

    setTimeout(() => {
      this.connect();
    }, this.retryDelay);
  }

  addListener(callback) {
    this.listeners.push(callback);
  }

  close() {
    if (this.eventSource) {
      this.eventSource.close();
    }
  }
}

// Usage
const client = new SSEClient('http://localhost:8080/events', {
  maxRetries: 10,
  retryDelay: 5000
});

client.addListener((data) => {
  if (data.type === 'heartbeat') return;
  console.log('Query results:', data.results);
});

client.connect();
```

### Node.js Client Example

For Node.js applications, use the `eventsource` package:

```bash
npm install eventsource
```

```javascript
const EventSource = require('eventsource');

const eventSource = new EventSource('http://localhost:8080/events');

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  if (data.type === 'heartbeat') {
    console.log('Heartbeat:', new Date(data.ts));
    return;
  }

  console.log('Query results:', data.results);
};

eventSource.onerror = (error) => {
  console.error('SSE error:', error);
};
```

### cURL Example

Test the SSE endpoint using cURL:

```bash
# Connect and stream events
curl -N http://localhost:8080/events

# With verbose output to see headers
curl -N -v http://localhost:8080/events
```

Expected output:

```
data: {"queryId":"temperature-alerts","results":[{"id":"sensor-001","temperature":85.2}],"timestamp":1706742123456}

data: {"type":"heartbeat","ts":1706742138456}

: keep-alive

data: {"queryId":"temperature-alerts","results":[{"id":"sensor-001","temperature":87.1}],"timestamp":1706742153789}
```

The `-N` flag disables buffering, allowing you to see events as they arrive.

## Server Behavior

### HTTP Server Setup (Axum)

The SSE reaction uses the [Axum web framework](https://github.com/tokio-rs/axum) to implement the HTTP server:

- **Async Runtime**: Built on Tokio for efficient async I/O
- **HTTP/1.1**: Standard HTTP protocol with streaming response
- **Single Endpoint**: Exposes one GET endpoint at the configured `sse_path`
- **Binding**: Binds to the configured `host:port` on startup
- **Graceful Shutdown**: Stops accepting new connections and aborts existing connections when the reaction is stopped

### CORS Configuration

The SSE server is configured with permissive CORS settings to enable browser-based clients from any origin:

```rust
let cors = CorsLayer::new()
    .allow_origin(Any)
    .allow_methods([Method::GET, Method::OPTIONS])
    .allow_headers(Any);
```

**CORS Headers:**
- `Access-Control-Allow-Origin: *` - Allows requests from any origin
- `Access-Control-Allow-Methods: GET, OPTIONS` - Supports GET requests and preflight OPTIONS
- `Access-Control-Allow-Headers: *` - Allows any request headers

This configuration enables browser clients on different domains to connect to the SSE endpoint without CORS errors.

**Security Note**: The wildcard CORS policy (`*`) allows any website to connect to the SSE endpoint. For production deployments, consider restricting `allow_origin` to specific domains:

```rust
// Example: restrict to specific origins
.allow_origin([
    "https://dashboard.example.com".parse().unwrap(),
    "https://app.example.com".parse().unwrap(),
])
```

### Broadcast Channel for Multiple Clients

The SSE reaction uses a Tokio `broadcast::channel` to efficiently distribute events to multiple concurrent clients:

```rust
let (tx, _rx) = broadcast::channel(1024);
```

**Channel Characteristics:**

- **Capacity**: 1024 events buffered in the channel
- **Multiple Receivers**: Each connected SSE client gets its own receiver from the broadcaster
- **Fanout**: Events are cloned and sent to all active receivers simultaneously
- **Lagging Receivers**: If a client cannot consume events fast enough and the channel buffer fills, that client will miss events (they are dropped)

**How It Works:**

1. Query results arrive via the `QueryResultReceiver`
2. Results are converted to JSON and sent to the broadcast channel
3. All connected SSE clients receive the event simultaneously
4. Each client's HTTP stream receives the event independently

### Heartbeat Mechanism

The SSE reaction maintains a separate Tokio task that sends periodic heartbeat events:

```rust
let mut ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
loop {
    ticker.tick().await;
    let beat = json!({"type":"heartbeat","ts": chrono::Utc::now().timestamp_millis()})
        .to_string();
    let _ = hb_tx.send(beat);
}
```

**Purpose:**
- Keeps connections alive through proxies and firewalls
- Helps clients detect server unavailability
- Prevents idle connection timeouts
- Provides a timing signal for monitoring

**Behavior:**
- Runs continuously while the reaction is active
- Broadcasts to all connected clients
- Uses UTC timestamps in milliseconds
- Independent of query result events

### Query Filtering

The SSE reaction filters incoming query results based on its configured query subscriptions:

```rust
if !queries.contains(&query_result.query_id) {
    debug!("Ignoring result from non-subscribed query '{}'", query_result.query_id);
    continue;
}
```

**Filtering Logic:**

- Only results from queries in the reaction's `queries` list are broadcast
- Results from other queries are silently ignored
- Filtering happens before broadcasting to avoid unnecessary network traffic
- Allows multiple SSE reactions to subscribe to different queries

**Example:**

```yaml
reactions:
  - id: "dashboard-sse"
    reaction_type: "sse"
    queries:
      - "query-a"
      - "query-b"
    # This reaction will only broadcast results from query-a and query-b
```

### Connection Limits

The SSE reaction does not impose explicit connection limits. The practical limits are:

- **System Resources**: Limited by available memory and file descriptors
- **Tokio Runtime**: Handles thousands of concurrent connections efficiently
- **Broadcast Channel**: Fixed capacity (1024 events) may cause event loss for slow clients
- **Network Bandwidth**: Each client consumes bandwidth for event delivery

**Scaling Considerations:**

- Each connected client holds an HTTP connection and a broadcast receiver
- Memory usage scales linearly with the number of concurrent clients
- High event rates with many clients may cause slower clients to lag and drop events
- Consider using a reverse proxy (nginx, HAProxy) for production deployments to handle connection pooling and load balancing

## Troubleshooting

### Connection Refused

**Symptom**: Client cannot connect, receives "Connection refused" error

**Possible Causes**:
1. SSE server not started or failed to start
2. Incorrect host/port in client URL
3. Firewall blocking the port
4. Port already in use by another service

**Solutions**:
```bash
# Check if port is in use
lsof -i :8080
netstat -an | grep 8080

# Verify server is listening
curl -v http://localhost:8080/events

# Check server logs for startup errors
# Look for messages like:
# "Starting SSE server on 0.0.0.0:8080 path /events with CORS enabled"
# "Failed to bind SSE server: ..."

# Try binding to different port
properties:
  port: 8081

# For local testing, use 127.0.0.1
properties:
  host: "127.0.0.1"
  port: 8080
```

### CORS Errors in Browser

**Symptom**: Browser console shows CORS policy error

```
Access to EventSource at 'http://localhost:8080/events' from origin 'http://example.com' has been blocked by CORS policy
```

**Cause**: While the SSE reaction allows all origins by default, issues may arise from:
- Reverse proxy stripping CORS headers
- HTTPS/HTTP mixed content (browser blocks HTTP EventSource from HTTPS page)
- Custom middleware interfering with CORS

**Solutions**:
```javascript
// If running locally, use same origin
const eventSource = new EventSource('/events');  // Relative URL

// For HTTPS sites, ensure SSE server uses HTTPS
// (requires reverse proxy like nginx with SSL)

// Verify CORS headers are present
curl -v http://localhost:8080/events
// Look for:
// Access-Control-Allow-Origin: *
```

**Mixed Content Issues**:
- Browsers block HTTP requests from HTTPS pages
- Must use HTTPS for SSE endpoint if client page is HTTPS
- Use reverse proxy with SSL termination:

```nginx
# nginx configuration
location /events {
    proxy_pass http://localhost:8080/events;
    proxy_set_header Connection '';
    proxy_http_version 1.1;
    chunked_transfer_encoding off;
    proxy_buffering off;
    proxy_cache off;
}
```

### Event Loss

**Symptom**: Client doesn't receive all events, events appear to be skipped

**Causes**:
1. **Slow Client**: Client processing too slow, broadcast channel overflows (1024 event buffer)
2. **Network Congestion**: TCP buffer full, events dropped
3. **Connection Interruption**: Client disconnected temporarily, missed events during disconnect
4. **Lagging Receiver**: Broadcast channel drops events for receivers that fall behind

**Solutions**:
```javascript
// Monitor event rate to detect losses
let lastSequence = 0;
eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);
  if (data.sequence && data.sequence > lastSequence + 1) {
    console.warn('Event loss detected: expected', lastSequence + 1, 'got', data.sequence);
  }
  lastSequence = data.sequence;
};

// Implement application-level sequence checking
// Query metadata can include sequence numbers:
// "metadata": {"sequence": 42}

// If event loss is frequent, consider:
// 1. Reduce query result frequency (adjust source update_interval_ms)
// 2. Use HTTP polling instead of SSE for high-volume scenarios
// 3. Implement client-side buffering and batching
// 4. Use gRPC reaction for more reliable delivery

// For critical events, use HTTP reaction with retry logic
```

**Note**: SSE provides at-most-once delivery. Lost events are not retransmitted.

### Heartbeat Timeouts

**Symptom**: Client connection drops after idle period, no events received

**Causes**:
- Proxy/firewall closes idle connections
- Heartbeat interval too long
- Network path drops idle TCP connections

**Solutions**:
```yaml
# Reduce heartbeat interval
properties:
  heartbeat_interval_ms: 5000  # 5 seconds instead of 15

# Adjust keep-alive interval (in code, currently hardcoded to 30s)
# If needed, modify SseReaction to make keep_alive_interval configurable
```

```javascript
// Client-side: implement heartbeat monitoring
let lastHeartbeat = Date.now();

eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);

  if (data.type === 'heartbeat') {
    lastHeartbeat = Date.now();
    return;
  }

  // Process normal events
};

// Monitor heartbeat timeout
setInterval(() => {
  const elapsed = Date.now() - lastHeartbeat;
  if (elapsed > 30000) {  // 30 seconds without heartbeat
    console.error('Heartbeat timeout, connection may be dead');
    eventSource.close();
    // Reconnect...
  }
}, 5000);
```

### Port Already in Use

**Symptom**: Server fails to start with error like:

```
Failed to bind SSE server: Address already in use (os error 48)
```

**Solutions**:
```bash
# Find process using the port
lsof -i :8080
# or
netstat -vanp tcp | grep 8080

# Kill the process
kill <PID>

# Or configure different port
properties:
  port: 8081
```

### Firewall Issues

**Symptom**: External clients cannot connect, but localhost works

**Causes**:
- Host firewall blocking port
- Network firewall blocking traffic
- Cloud provider security group blocking port

**Solutions**:
```bash
# Check host firewall (Linux)
sudo iptables -L -n | grep 8080
sudo ufw status

# Allow port through firewall
sudo ufw allow 8080/tcp

# macOS firewall
# System Preferences -> Security & Privacy -> Firewall

# Cloud providers (AWS, GCP, Azure)
# Add inbound rule to security group for port 8080

# Verify external connectivity
curl http://<external-ip>:8080/events
```

### High Memory Usage

**Symptom**: Memory usage grows over time with many clients

**Causes**:
- Large number of concurrent connections
- Broadcast channel buffering events
- Slow clients holding references to old events

**Solutions**:
```yaml
# Limit connection rate at reverse proxy
# Use nginx connection limiting

# Monitor memory usage
# Look for increasing resident set size (RSS)

# Consider horizontal scaling:
# Run multiple SSE reaction instances behind load balancer

# For high-volume scenarios:
# - Use gRPC reaction for backend clients
# - Implement client connection limits
# - Use shorter keepalive intervals to detect dead connections
```

## Limitations

### Browser Compatibility

The SSE reaction relies on the browser's native `EventSource` API, which has broad but not universal support:

**Supported Browsers**:
- Chrome/Chromium: All versions
- Firefox: All versions
- Safari: All versions (iOS 10+)
- Edge: All versions (Chromium-based)
- Opera: All versions

**Not Supported**:
- Internet Explorer: No SSE support (use polyfill or alternative)
- Old Android Browser: Limited or no support (use Chrome on Android)

**Polyfills**:
For older browsers, use polyfills like [event-source-polyfill](https://github.com/Yaffle/EventSource):

```javascript
import EventSource from 'event-source-polyfill';
const eventSource = new EventSource('http://localhost:8080/events');
```

**Feature Detection**:
```javascript
if (typeof EventSource !== 'undefined') {
  // Native SSE support
  const eventSource = new EventSource('http://localhost:8080/events');
} else {
  // Fallback to polling or WebSocket
  console.error('SSE not supported');
}
```

### Connection Limits

**Concurrent Connections**:
- No explicit limit in SSE reaction code
- Limited by system resources (file descriptors, memory)
- Tokio runtime handles thousands of connections efficiently

**Browser Limitations**:
- Browsers limit concurrent SSE connections per domain (typically 6)
- Opening multiple EventSource connections to same domain may block
- Use multiplexing or subdomain sharding if needed

**Practical Limits**:
- Memory: ~1-5 KB per connection for Tokio async task overhead
- File Descriptors: Default ulimit typically 1024-4096, can be increased
- Broadcast Channel: Fixed 1024 event buffer affects slow clients

### Event Ordering Guarantees

**Single Query**:
- Events from a single query are broadcast in the order they are generated
- Ordering preserved within one query's result stream

**Multiple Queries**:
- Events from different queries may interleave
- No global ordering across queries
- Client receives events in the order they are broadcast, but concurrent query results may arrive in any order

**Network Reordering**:
- TCP guarantees in-order delivery within a single connection
- But reconnection may cause gaps

**Example**:
```javascript
// Query A produces: A1, A2, A3
// Query B produces: B1, B2, B3
// Client may receive: A1, B1, A2, B2, A3, B3
// Or any other interleaving
```

**Recommendations**:
- Include sequence numbers in query result metadata if ordering is critical
- Use single query per SSE reaction if ordering matters
- Implement client-side reordering based on timestamps or sequence numbers

### Network Reliability Considerations

**Connection Drops**:
- SSE connections can drop due to network issues, proxy timeouts, or server restarts
- EventSource automatically reconnects, but events sent during disconnect are lost
- No persistent queue or replay mechanism

**Event Loss**:
- At-most-once delivery semantics
- Events broadcast to disconnected clients are lost
- Slow clients may drop events if broadcast buffer overflows

**No Acknowledgment**:
- Server doesn't know if client successfully received an event
- No delivery confirmation or error feedback

**Recommendations**:
- For critical events, use HTTP reaction with retry logic
- Implement application-level acknowledgment if needed
- Use gRPC reaction for more reliable delivery with streaming

### Buffering and Backpressure

**Broadcast Channel Buffer**:
- Fixed capacity: 1024 events
- If slow clients lag, events are dropped for those clients
- No backpressure mechanism to slow down query result generation

**TCP Buffers**:
- OS-level TCP send buffers may fill if client is very slow
- May block send operations briefly, but won't stop query processing

**No Rate Limiting**:
- SSE reaction broadcasts events as fast as queries produce them
- High-frequency queries can overwhelm clients
- No built-in throttling or batching

**Recommendations**:
```yaml
# Control event rate at source
sources:
  - id: "sensor-data"
    properties:
      update_interval_ms: 5000  # Reduce update frequency

# Or use batching in queries
# (Drasi doesn't support batching in queries directly)

# Client-side buffering
// Buffer events and process in batches
let buffer = [];
eventSource.onmessage = (event) => {
  buffer.push(JSON.parse(event.data));
  if (buffer.length >= 10) {
    processBatch(buffer);
    buffer = [];
  }
};
```

### One-Way Communication (SSE vs WebSocket)

**SSE Limitations**:
- **Unidirectional**: Server-to-client only
- **No Client Messages**: Client cannot send data back through SSE connection
- **Text Only**: SSE events are text-based (JSON over text)

**When SSE Falls Short**:
- Need bidirectional communication (chat, gaming)
- Client needs to send control messages or acknowledgments
- Need binary data streaming (video, audio)
- Need lower latency (WebSocket has less overhead)

**SSE Advantages**:
- Simpler protocol (just HTTP)
- Automatic reconnection built into EventSource
- Works over HTTP/1.1 without protocol upgrade
- Better compatibility with proxies and firewalls
- CORS support built-in

**Workarounds for Client-to-Server Communication**:
```javascript
// Use separate HTTP POST for client-to-server messages
const eventSource = new EventSource('http://localhost:8080/events');

// Listen to server events
eventSource.onmessage = (event) => {
  const data = JSON.parse(event.data);
  handleServerEvent(data);
};

// Send control messages via HTTP POST
async function sendCommand(command) {
  await fetch('http://localhost:8080/api/command', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(command)
  });
}
```

**When to Use WebSocket Instead**:
- Real-time chat or collaboration
- Gaming or interactive applications
- Need low-latency bidirectional communication
- Binary data streaming required
- Client needs to send frequent updates

**WebSocket Alternative**:
- Drasi does not currently provide a WebSocket reaction
- For bidirectional needs, combine SSE reaction (server-to-client) with HTTP/gRPC source (client-to-server)
- Or implement custom WebSocket reaction if needed
