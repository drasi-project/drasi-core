# HTTP Adaptive Reaction

An intelligent HTTP webhook reaction for Drasi that automatically batches query results based on real-time throughput patterns, optimizing both latency and network efficiency.

## Overview

The HTTP Adaptive Reaction extends the standard HTTP reaction with intelligent batching capabilities. It monitors data flow patterns and dynamically adjusts batch size and timing to optimize performance:

- **Low traffic**: Sends results immediately for minimal latency
- **Medium traffic**: Uses moderate batching for balanced performance
- **High traffic**: Maximizes batch sizes for network efficiency
- **Burst traffic**: Handles spikes gracefully with large batches

### Key Capabilities

- **Adaptive Batching**: Automatically adjusts batch size (1 to 1000+ events) based on throughput
- **Batch Endpoint**: Sends batches to `{base_url}/batch` for efficient processing
- **HTTP/2 Connection Pooling**: Maintains persistent connections with configurable pool size
- **Individual Fallback**: Falls back to query-specific routes for single results
- **Template Support**: Uses Handlebars for flexible URL and body templating
- **Bearer Token Auth**: Optional authentication via Authorization header

### Use Cases

- **Real-time Analytics Pipelines**: Efficiently deliver aggregated data to analytics systems
- **Event Processing**: Batch events to downstream processors without overwhelming them
- **Webhook Notifications**: Optimize webhook delivery for varying traffic patterns
- **Data Synchronization**: Keep external systems in sync with minimal HTTP overhead
- **Monitoring & Alerting**: Batch monitoring events while maintaining low latency

## Configuration

The HTTP Adaptive Reaction supports two configuration approaches: a fluent builder pattern (recommended for programmatic use) and a config struct approach (for YAML/serialization).

### Builder Pattern (Recommended)

```rust
use drasi_reaction_http_adaptive::AdaptiveHttpReaction;
use drasi_reaction_http::QueryConfig;

let reaction = AdaptiveHttpReaction::builder("analytics-webhook")
    .with_base_url("https://api.example.com")
    .with_token("your-api-token")
    .with_timeout_ms(10000)
    .with_queries(vec!["user-activity".to_string()])
    .with_min_batch_size(20)
    .with_max_batch_size(500)
    .with_window_size(50)  // 5 seconds
    .with_batch_timeout_ms(1000)
    .build()?;
```

### Config Struct Approach

```rust
use drasi_reaction_http_adaptive::HttpAdaptiveReactionConfig;
use drasi_lib::reactions::common::AdaptiveBatchConfig;
use std::collections::HashMap;

let config = HttpAdaptiveReactionConfig {
    base_url: "https://api.example.com".to_string(),
    token: Some("your-api-token".to_string()),
    timeout_ms: 10000,
    routes: HashMap::new(),
    adaptive: AdaptiveBatchConfig {
        adaptive_min_batch_size: 20,
        adaptive_max_batch_size: 500,
        adaptive_window_size: 50,  // 5 seconds (50 × 100ms)
        adaptive_batch_timeout_ms: 1000,
    },
};

let reaction = AdaptiveHttpReaction::new(
    "analytics-webhook",
    vec!["user-activity".to_string()],
    config
);
```

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `base_url` | Base URL for HTTP requests. Batch requests sent to `{base_url}/batch` | String | Valid HTTP/HTTPS URL | `"http://localhost"` |
| `token` | Optional bearer token for authentication | Option<String> | Any string | `None` |
| `timeout_ms` | Request timeout in milliseconds | u64 | 1 - 300000 (5 min) | `5000` |
| `routes` | Query-specific route configurations for individual requests | HashMap<String, QueryConfig> | See QueryConfig docs | Empty map |
| `adaptive_min_batch_size` | Minimum batch size used during idle/low traffic | usize | 1 - 10000 | `1` |
| `adaptive_max_batch_size` | Maximum batch size used during burst traffic | usize | 1 - 10000 | `100` |
| `adaptive_window_size` | Window size for throughput monitoring in 100ms units (10 = 1 sec, 50 = 5 sec, 100 = 10 sec) | usize | 1 - 255 | `10` (1 second) |
| `adaptive_batch_timeout_ms` | Maximum time to wait before flushing a partial batch | u64 | 1 - 60000 | `1000` |

### Adaptive Algorithm Behavior

The reaction monitors throughput over the configured window and adjusts parameters:

| Throughput Level | Messages/Sec | Batch Size | Wait Time |
|-----------------|--------------|------------|-----------|
| Idle | < 1 | min_batch_size | 1ms |
| Low | 1-100 | 2 × min | 1ms |
| Medium | 100-1K | 25% of max | 10ms |
| High | 1K-10K | 50% of max | 25ms |
| Burst | > 10K | max_batch_size | 50ms |

## Output Schema

### Batch Endpoint Format

When multiple results are available, the reaction sends a POST request to `{base_url}/batch` with the following JSON structure:

```json
[
  {
    "query_id": "user-changes",
    "results": [
      {
        "type": "ADD",
        "data": {"id": "user_123", "name": "John Doe"},
        "after": {"id": "user_123", "name": "John Doe"}
      },
      {
        "type": "UPDATE",
        "data": {"id": "user_456", "name": "Jane Smith"},
        "before": {"id": "user_456", "name": "Jane Doe"},
        "after": {"id": "user_456", "name": "Jane Smith"}
      },
      {
        "type": "DELETE",
        "data": {"id": "user_789", "name": "Bob Wilson"},
        "before": {"id": "user_789", "name": "Bob Wilson"}
      }
    ],
    "timestamp": "2025-10-19T12:34:56.789Z",
    "count": 3
  }
]
```

#### BatchResult Schema

Each batch result object contains:

- **`query_id`** (string): The ID of the query that produced these results
- **`results`** (array): Array of result objects from the query
  - **`type`** (string): Operation type - "ADD", "UPDATE", or "DELETE"
  - **`data`** (object): The result data from the query
  - **`before`** (object, optional): Previous state for UPDATE/DELETE operations
  - **`after`** (object, optional): New state for ADD/UPDATE operations
- **`timestamp`** (string): ISO 8601 timestamp when the batch was created
- **`count`** (number): Number of results in this batch (matches results.length)

### Individual Request Format

When only a single result is available or batch endpoints are disabled, the reaction uses query-specific routes (if configured) and sends individual POST requests with the raw result data.

### HTTP Headers

All requests include:
- `Content-Type: application/json`
- `Authorization: Bearer {token}` (if token is configured)

## Usage Examples

### Example 1: High-Throughput Analytics

```rust
use drasi_reaction_http_adaptive::AdaptiveHttpReaction;

// Configure for high-throughput analytics with large batches
let reaction = AdaptiveHttpReaction::builder("analytics-webhook")
    .with_base_url("https://analytics.example.com")
    .with_token("analytics-api-key")
    .with_queries(vec!["user-events".to_string()])
    .with_min_batch_size(50)
    .with_max_batch_size(2000)
    .with_window_size(100)  // 10 seconds
    .with_batch_timeout_ms(500)
    .with_timeout_ms(30000)  // 30 second timeout for large batches
    .build()?;
```

### Example 2: Low-Latency Monitoring

```rust
use drasi_reaction_http_adaptive::AdaptiveHttpReaction;

// Configure for low-latency monitoring with immediate delivery
let reaction = AdaptiveHttpReaction::builder("alert-webhook")
    .with_base_url("https://alerts.example.com")
    .with_queries(vec!["critical-events".to_string()])
    .with_min_batch_size(1)
    .with_max_batch_size(50)
    .with_window_size(30)  // 3 seconds
    .with_batch_timeout_ms(10)  // Send quickly
    .with_timeout_ms(5000)
    .build()?;
```

### Example 3: Multiple Queries with Routes

```rust
use drasi_reaction_http_adaptive::AdaptiveHttpReaction;
use drasi_reaction_http::{QueryConfig, CallSpec};
use std::collections::HashMap;

// Configure routes for individual fallback handling
let mut routes = HashMap::new();
routes.insert("users".to_string(), QueryConfig {
    added: Some(CallSpec {
        url: "/users".to_string(),
        method: "POST".to_string(),
        body: "{{data.after}}".to_string(),
        headers: HashMap::new(),
    }),
    updated: Some(CallSpec {
        url: "/users/{{data.after.id}}".to_string(),
        method: "PUT".to_string(),
        body: "{{data.after}}".to_string(),
        headers: HashMap::new(),
    }),
    deleted: Some(CallSpec {
        url: "/users/{{data.before.id}}".to_string(),
        method: "DELETE".to_string(),
        body: String::new(),
        headers: HashMap::new(),
    }),
});

let reaction = AdaptiveHttpReaction::builder("user-sync")
    .with_base_url("https://api.example.com")
    .with_queries(vec!["users".to_string(), "orders".to_string()])
    .with_route("users".to_string(), routes["users"].clone())
    .with_min_batch_size(10)
    .with_max_batch_size(100)
    .build()?;
```

### Example 4: Using Config Struct with YAML

```yaml
# config.yaml
reactions:
  - id: adaptive-webhook
    reaction_type: http_adaptive
    queries:
      - user-activity
      - order-updates
    auto_start: true
    config:
      base_url: https://webhook.example.com
      token: your-secret-token
      timeout_ms: 10000
      adaptive_min_batch_size: 20
      adaptive_max_batch_size: 500
      adaptive_window_size: 50
      adaptive_batch_timeout_ms: 1000
```

## Performance Characteristics

### Memory Usage

The reaction uses an internal channel for batching with capacity automatically scaled to:
```
channel_capacity = max_batch_size × 5
```

This provides sufficient buffering for pipeline parallelism and burst handling:

| max_batch_size | Channel Capacity | Memory (1KB/event) |
|----------------|------------------|---------------------|
| 100            | 500              | ~500 KB            |
| 1,000          | 5,000            | ~5 MB              |
| 5,000          | 25,000           | ~25 MB             |

### HTTP/2 Connection Pooling

The reaction maintains a connection pool with:
- **Idle timeout**: 90 seconds
- **Max idle per host**: 10 connections
- **HTTP/2 prior knowledge**: Enabled for compatible servers

This reduces connection overhead and improves throughput for high-frequency webhooks.

### Throughput Optimization

The adaptive algorithm automatically balances latency and throughput:

- **Idle periods**: ~1ms latency per event (immediate delivery)
- **Low traffic**: ~1-10ms latency, small batches
- **Medium traffic**: ~10-25ms latency, moderate batches
- **High traffic**: ~25-50ms latency, large batches
- **Burst traffic**: ~50-100ms latency, maximum batches

## Comparison with Standard HTTP Reaction

| Feature | HTTP Reaction | HTTP Adaptive Reaction |
|---------|--------------|------------------------|
| Batching | No (one request per result) | Yes (adaptive batching) |
| Throughput | Low-Medium | High |
| Latency | Lowest | Low (adaptive) |
| Network Efficiency | Low | High |
| Memory Usage | Low | Medium |
| Use Case | Low-volume events | Variable or high-volume events |

## Implementation Details

### Component Architecture

The reaction consists of two async tasks:

1. **Main Task**: Receives query results from the priority queue and forwards to batcher
2. **Batcher Task**: Collects results into adaptive batches and sends HTTP requests

### Error Handling

- Failed HTTP requests are logged but don't stop processing
- Non-2xx responses are logged with status code and body
- Network errors trigger automatic retry via HTTP client
- Timeouts are configurable per-request

### Backpressure

The reaction supports backpressure through the priority queue mechanism:
- Queue fills when HTTP requests are slower than result production
- Backpressure flows upstream to queries and sources
- Configurable via `priority_queue_capacity` parameter

## Testing

The component includes comprehensive tests:

```bash
# Run all tests
cargo test -p drasi-reaction-http-adaptive

# Run with logging
RUST_LOG=debug cargo test -p drasi-reaction-http-adaptive -- --nocapture
```

## Dependencies

- **drasi-lib**: Core library for reactions and channels
- **drasi-reaction-http**: Base HTTP reaction for route configurations
- **reqwest**: HTTP client with connection pooling
- **handlebars**: Template rendering for URLs and bodies
- **tokio**: Async runtime
- **serde/serde_json**: Serialization
- **chrono**: Timestamp generation

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
