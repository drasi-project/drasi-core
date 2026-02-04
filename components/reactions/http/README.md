# HTTP Reaction

HTTP Reaction is a plugin component for Drasi that sends HTTP webhooks when query results change. It enables real-time notifications to external HTTP endpoints whenever continuous queries detect additions, updates, or deletions in the result set.

## Overview

The HTTP Reaction component monitors continuous query results and automatically sends HTTP requests to configured endpoints when data changes occur. It supports:

- **Multiple HTTP Methods**: GET, POST, PUT, DELETE, PATCH
- **Flexible Routing**: Different endpoints for different queries and operation types
- **Template-Based Requests**: Handlebars templates for dynamic URL, body, and header generation
- **Authentication**: Built-in Bearer token authentication support
- **Operation-Specific Handling**: Separate configurations for ADD, UPDATE, and DELETE operations
- **Priority Queue Processing**: Processes changes in timestamp order to ensure correct sequencing
- **Adaptive Batching**: Intelligent batching based on throughput patterns for high-volume scenarios

## Use Cases

- **Real-time Notifications**: Alert external systems when data changes
- **Workflow Automation**: Trigger automated workflows based on query result changes
- **Data Synchronization**: Keep external systems in sync with Drasi query results
- **Event-Driven Architecture**: Integrate Drasi with event-driven applications
- **Monitoring and Alerting**: Send alerts when specific conditions are detected

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides a fluent API for programmatic configuration:

```rust
use drasi_reaction_http::HttpReaction;

let reaction = HttpReaction::builder("my-http-reaction")
    .with_base_url("https://api.example.com")
    .with_token("your-secret-token")
    .with_timeout_ms(10000)
    .with_query("temperature-alerts")
    .with_query("pressure-alerts")
    .build()?;
```

### Config Struct Approach

For more complex configurations with custom routing:

```rust
use drasi_reaction_http::{HttpReaction, HttpReactionConfig, QueryConfig, CallSpec};
use std::collections::HashMap;

let mut routes = HashMap::new();
routes.insert("temperature-alerts".to_string(), QueryConfig {
    added: Some(CallSpec {
        url: "/alerts/temperature/new".to_string(),
        method: "POST".to_string(),
        body: r#"{"alert": "High temperature", "data": {{json after}}}"#.to_string(),
        headers: HashMap::new(),
    }),
    updated: Some(CallSpec {
        url: "/alerts/temperature/update".to_string(),
        method: "PUT".to_string(),
        body: r#"{"before": {{json before}}, "after": {{json after}}}"#.to_string(),
        headers: HashMap::new(),
    }),
    deleted: Some(CallSpec {
        url: "/alerts/temperature/resolved".to_string(),
        method: "DELETE".to_string(),
        body: String::new(),
        headers: HashMap::new(),
    }),
});

let config = HttpReactionConfig {
    base_url: "https://api.example.com".to_string(),
    token: Some("your-secret-token".to_string()),
    timeout_ms: 5000,
    routes,
};

let reaction = HttpReaction::new(
    "my-http-reaction",
    vec!["temperature-alerts".to_string()],
    config,
);
```

## Configuration Options

### HttpReactionConfig

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|----------|
| `base_url` | Base URL for all HTTP requests. Can be overridden with absolute URLs in CallSpec. | String | `"http://localhost"` | No |
| `token` | Bearer token for authentication. Automatically adds `Authorization: Bearer <token>` header. | Option\<String\> | None | No |
| `timeout_ms` | Request timeout in milliseconds. | u64 | 5000 | No |
| `routes` | Query-specific routing configurations. Keys are query IDs. | HashMap\<String, QueryConfig\> | Empty | No |
| `adaptive` | Optional adaptive batching configuration. When set, enables intelligent batching. | Option\<AdaptiveBatchConfig\> | None | No |

### QueryConfig

Defines HTTP call specifications for each operation type within a query.

| Name | Description | Type | Required |
|------|-------------|------|----------|
| `added` | HTTP call specification for ADD operations (new rows). | Option\<CallSpec\> | No |
| `updated` | HTTP call specification for UPDATE operations (modified rows). | Option\<CallSpec\> | No |
| `deleted` | HTTP call specification for DELETE operations (removed rows). | Option\<CallSpec\> | No |

### CallSpec

Specification for an individual HTTP call.

| Name | Description | Type | Default | Required |
|------|-------------|------|---------|----------|
| `url` | URL path (appended to base_url) or absolute URL. Supports Handlebars templates. | String | - | Yes |
| `method` | HTTP method: GET, POST, PUT, DELETE, or PATCH (case-insensitive). | String | - | Yes |
| `body` | Request body as a Handlebars template. If empty, sends raw JSON data. | String | Empty | No |
| `headers` | Additional HTTP headers. Values support Handlebars templates. | HashMap\<String, String\> | Empty | No |

### Builder Methods

| Method | Description | Parameters |
|--------|-------------|------------|
| `builder(id)` | Create a new builder | `id: impl Into<String>` |
| `with_base_url(url)` | Set the base URL | `url: impl Into<String>` |
| `with_token(token)` | Set authentication token | `token: impl Into<String>` |
| `with_timeout_ms(ms)` | Set request timeout | `ms: u64` |
| `with_query(id)` | Add a query to subscribe to | `id: impl Into<String>` |
| `with_queries(ids)` | Set all queries to subscribe to | `ids: Vec<String>` |
| `with_route(id, config)` | Add a route configuration | `id: impl Into<String>`, `config: QueryConfig` |
| `with_priority_queue_capacity(capacity)` | Set priority queue capacity | `capacity: usize` |
| `with_auto_start(auto_start)` | Enable/disable auto-start | `auto_start: bool` |
| `with_adaptive_enabled(enabled)` | Enable/disable adaptive batching | `enabled: bool` |
| `with_min_batch_size(size)` | Set minimum batch size (default: 1) | `size: usize` |
| `with_max_batch_size(size)` | Set maximum batch size (default: 100) | `size: usize` |
| `with_window_size(size)` | Set throughput window size in 100ms units (default: 10 = 1 sec) | `size: usize` |
| `with_batch_timeout_ms(timeout)` | Set batch timeout in milliseconds (default: 1000) | `timeout: u64` |
| `build()` | Build the HttpReaction instance | Returns `anyhow::Result<HttpReaction>` |

## Output Schema

The HTTP Reaction sends JSON payloads to configured endpoints. The exact format depends on whether a custom body template is specified.

### Default Payload Format

When no custom body template is provided, the reaction sends the raw data:

**ADD Operation:**
```json
{
  "type": "ADD",
  "data": {
    "field1": "value1",
    "field2": "value2"
  }
}
```

**UPDATE Operation:**
```json
{
  "type": "UPDATE",
  "before": {
    "field1": "old_value1",
    "field2": "old_value2"
  },
  "after": {
    "field1": "new_value1",
    "field2": "new_value2"
  },
  "data": {
    "field1": "new_value1",
    "field2": "new_value2"
  }
}
```

**DELETE Operation:**
```json
{
  "type": "DELETE",
  "data": {
    "field1": "value1",
    "field2": "value2"
  }
}
```

### Template Variables

When using Handlebars templates in the body, the following variables are available:

| Variable | Description | Available Operations |
|----------|-------------|---------------------|
| `after` | The new/current state of the data | ADD, UPDATE |
| `before` | The previous state of the data | UPDATE, DELETE |
| `data` | The data field (equivalent to `after` for ADD/UPDATE) | UPDATE |
| `query_name` | The ID of the query that triggered the change | ALL |
| `operation` | The operation type: "ADD", "UPDATE", or "DELETE" | ALL |

### HTTP Request Details

**Headers:**
- `Content-Type: application/json` (always set)
- `Authorization: Bearer <token>` (if token is configured)
- Any custom headers defined in CallSpec

**URL Construction:**
- If CallSpec.url starts with `http://` or `https://`, it's used as-is
- Otherwise, it's appended to the base_url: `{base_url}{CallSpec.url}`

## Usage Examples

### Example 1: Basic Webhook

Simple webhook that POSTs all changes to a single endpoint:

```rust
use drasi_reaction_http::HttpReaction;

let reaction = HttpReaction::builder("webhook-reaction")
    .with_base_url("https://webhook.site/your-unique-id")
    .with_query("my-query")
    .build()?;
```

This uses the default configuration which sends all ADD, UPDATE, and DELETE operations to `/changes/my-query`.

### Example 2: Authenticated API Integration

Send changes to an authenticated API with custom timeout:

```rust
use drasi_reaction_http::HttpReaction;

let reaction = HttpReaction::builder("api-integration")
    .with_base_url("https://api.myservice.com")
    .with_token("sk_live_abc123def456")
    .with_timeout_ms(30000)  // 30 second timeout
    .with_queries(vec![
        "user-registrations".to_string(),
        "order-updates".to_string(),
    ])
    .build()?;
```

### Example 3: Custom Routes with Templates

Different endpoints for different operations with custom payloads:

```rust
use drasi_reaction_http::{HttpReaction, HttpReactionConfig, QueryConfig, CallSpec};
use std::collections::HashMap;

let mut routes = HashMap::new();
routes.insert("sensor-alerts".to_string(), QueryConfig {
    added: Some(CallSpec {
        url: "/alerts".to_string(),
        method: "POST".to_string(),
        body: r#"{
            "type": "alert",
            "severity": "high",
            "sensor_id": "{{after.sensor_id}}",
            "temperature": {{after.temperature}},
            "timestamp": "{{after.timestamp}}"
        }"#.to_string(),
        headers: HashMap::new(),
    }),
    updated: Some(CallSpec {
        url: "/alerts/{{after.alert_id}}".to_string(),
        method: "PUT".to_string(),
        body: r#"{
            "sensor_id": "{{after.sensor_id}}",
            "temperature": {{after.temperature}},
            "previous_temperature": {{before.temperature}}
        }"#.to_string(),
        headers: HashMap::new(),
    }),
    deleted: Some(CallSpec {
        url: "/alerts/{{before.alert_id}}".to_string(),
        method: "DELETE".to_string(),
        body: String::new(),
        headers: HashMap::new(),
    }),
});

let config = HttpReactionConfig {
    base_url: "https://monitoring.example.com".to_string(),
    token: Some("api-key-xyz".to_string()),
    timeout_ms: 10000,
    routes,
};

let reaction = HttpReaction::new("sensor-monitor", vec!["sensor-alerts".to_string()], config);
```

### Example 4: Using the json Helper

The `json` Handlebars helper serializes complex objects:

```rust
use drasi_reaction_http::{QueryConfig, CallSpec};
use std::collections::HashMap;

let call_spec = CallSpec {
    url: "/webhook".to_string(),
    method: "POST".to_string(),
    body: r#"{
        "event": "{{operation}}",
        "query": "{{query_name}}",
        "data": {{json after}}
    }"#.to_string(),
    headers: HashMap::new(),
};
```

This ensures the entire `after` object is properly JSON-serialized.

### Example 5: Custom Headers

Add custom headers with templating:

```rust
use drasi_reaction_http::{CallSpec};
use std::collections::HashMap;

let mut headers = HashMap::new();
headers.insert("X-Event-Type".to_string(), "{{operation}}".to_string());
headers.insert("X-Query-ID".to_string(), "{{query_name}}".to_string());
headers.insert("X-Custom-Header".to_string(), "static-value".to_string());

let call_spec = CallSpec {
    url: "/events".to_string(),
    method: "POST".to_string(),
    body: r#"{{json after}}"#.to_string(),
    headers,
};
```

### Example 6: Integration with DrasiLib

Full example showing integration with DrasiLib:

```rust
use drasi_lib::DrasiLib;
use drasi_reaction_http::HttpReaction;

// Create Drasi instance
let drasi = DrasiLib::new("my-app").await?;

// Create and add HTTP reaction
let reaction = HttpReaction::builder("webhook")
    .with_base_url("https://api.example.com")
    .with_token("secret-token")
    .with_query("my-continuous-query")
    .build()?;

drasi.add_reaction(reaction).await?;

// Start the reaction
drasi.start_reaction("webhook").await?;
```

## Adaptive Batching

For high-throughput scenarios, the HTTP Reaction supports adaptive batching. When enabled, the reaction intelligently groups results into batches based on current throughput patterns, reducing network overhead and improving efficiency.

### Enabling Adaptive Batching

```rust
use drasi_reaction_http::HttpReaction;

let reaction = HttpReaction::builder("high-throughput-webhook")
    .with_base_url("https://api.example.com")
    .with_query("high-volume-query")
    .with_adaptive_enabled(true)
    .with_min_batch_size(10)
    .with_max_batch_size(500)
    .with_window_size(50)  // 5 seconds
    .with_batch_timeout_ms(1000)
    .build()?;
```

### Adaptive Batching Configuration

| Parameter | Description | Default |
|-----------|-------------|---------|
| `adaptive_min_batch_size` | Minimum batch size used during idle/low traffic | 1 |
| `adaptive_max_batch_size` | Maximum batch size used during burst traffic | 100 |
| `adaptive_window_size` | Throughput monitoring window (in 100ms units) | 10 (1 second) |
| `adaptive_batch_timeout_ms` | Maximum time to wait before flushing a partial batch | 1000 (1 second) |

### Throughput Levels

The adaptive batcher adjusts batch size and timing based on detected throughput:

| Level | Messages/Sec | Batch Size | Wait Time |
|-------|--------------|------------|-----------|
| Idle | < 1 | min_batch_size | 1ms |
| Low | 1-100 | 2 × min | 1ms |
| Medium | 100-1K | 25% of max | 10ms |
| High | 1K-10K | 50% of max | 25ms |
| Burst | > 10K | max_batch_size | 50ms |

### Batch Endpoint

When adaptive batching is enabled, batches are sent to `{base_url}/batch` as a JSON array of `BatchResult` objects:

```json
[
  {
    "query_id": "my-query",
    "results": [
      { "type": "add", "data": {...} },
      { "type": "update", "data": {...}, "before": {...}, "after": {...} }
    ],
    "timestamp": "2025-01-24T12:34:56.789Z",
    "count": 2
  }
]
```

### BatchResult Schema

| Field | Type | Description |
|-------|------|-------------|
| `query_id` | String | The ID of the query that produced these results |
| `results` | Array\<ResultDiff\> | Array of result diffs (add, update, delete) |
| `timestamp` | String | ISO 8601 timestamp when the batch was created |
| `count` | Number | Number of results in this batch |

### When to Use Adaptive Batching

Adaptive batching is recommended when:
- You have high-volume data changes (>100 events/sec)
- Your receiving endpoint can handle batch processing
- Network latency is a concern
- You want to optimize for throughput over latency

Standard mode (no batching) is better when:
- You need real-time, per-event delivery
- Your event volume is low to moderate
- Your endpoint doesn't support batch processing
- Latency is critical

### HTTP/2 Connection Pooling

When adaptive batching is enabled, the HTTP client automatically uses HTTP/2 with connection pooling:
- Pool idle timeout: 90 seconds
- Max idle connections per host: 10

This provides better performance for high-throughput scenarios.

## Advanced Features

### Priority Queue Processing

The HTTP Reaction uses a priority queue to process changes in timestamp order. This ensures that changes are sent to webhooks in the correct sequence, even if they arrive out of order.

You can customize the priority queue capacity:

```rust
let reaction = HttpReaction::builder("my-reaction")
    .with_base_url("https://api.example.com")
    .with_priority_queue_capacity(1000)  // Custom capacity
    .build()?;
```

### Default Routing Behavior

If no route configuration is provided for a query, the HTTP Reaction uses a default configuration that:
- POSTs all ADD operations to `/changes/{query_id}`
- POSTs all UPDATE operations to `/changes/{query_id}`
- POSTs all DELETE operations to `/changes/{query_id}`

This allows you to quickly set up basic webhooks without detailed configuration.

### Query ID Matching

The HTTP Reaction supports flexible query ID matching:
- Exact match: `"my-query"` matches query ID `"my-query"`
- Suffix match: If query ID is `"source.my-query"`, it will match routes for `"my-query"`

This is useful when queries are namespaced by source.

### Error Handling

The HTTP Reaction logs errors but continues processing:
- Failed HTTP requests are logged as warnings with status code and response body
- Processing errors are logged as errors but don't stop the reaction
- The reaction continues processing subsequent changes even if individual requests fail

## Performance Considerations

- **Timeout Configuration**: Set appropriate timeouts based on your endpoint's expected response time
- **Priority Queue Capacity**: Adjust based on expected change volume and processing speed
- **Concurrent Processing**: The reaction processes changes sequentially within each query to maintain order
- **HTTP Client**: Uses a shared reqwest client with connection pooling for efficiency

## Dependencies

The HTTP Reaction requires the following dependencies:

- `drasi-lib` - Core Drasi library for plugin integration
- `reqwest` - HTTP client library
- `handlebars` - Template engine for dynamic content
- `serde` / `serde_json` - JSON serialization
- `tokio` - Async runtime
- `anyhow` - Error handling

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
