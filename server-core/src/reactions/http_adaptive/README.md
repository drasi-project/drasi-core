# Adaptive HTTP Reaction

The Adaptive HTTP reaction extends the standard HTTP reaction with intelligent batching capabilities and HTTP/2 connection pooling. It automatically adjusts batch size and timing based on throughput patterns to optimize for either low latency or high throughput scenarios.

## Purpose

The Adaptive HTTP reaction provides the same webhook integration capabilities as the standard HTTP reaction, but adds:

- **Adaptive Batching**: Automatically adjusts batch size and wait time based on message throughput
- **Batch Endpoint Support**: Sends multiple results in a single HTTP request when enabled
- **HTTP/2 Connection Pooling**: Reuses HTTP/2 connections for improved performance
- **Throughput-Based Optimization**: Switches between latency-optimized (small batches) and throughput-optimized (large batches) modes

### Difference from Standard HTTP Reaction

The standard HTTP reaction sends one HTTP request per query result. The Adaptive HTTP reaction adds:

1. **Intelligent batching** - Groups multiple results into batches based on traffic patterns
2. **Batch endpoint option** - Can send batches to a dedicated `/batch` endpoint
3. **Adaptive algorithm** - Adjusts batch size dynamically (10-1000 items) and wait time (1-100ms) based on observed throughput
4. **Connection pooling** - Maintains persistent HTTP/2 connections for better performance

### Batch Endpoint Support

When `batch_endpoints_enabled` is `true` (default), the reaction sends batches to a `/batch` endpoint with an array of results. When `false` or when only single results are available, it falls back to individual requests using the same format as the standard HTTP reaction.

### When to Use Adaptive vs Standard HTTP

**Use Adaptive HTTP when**:
- You expect high-volume or bursty traffic patterns
- Your target service supports batch endpoints (`POST /batch`)
- You want to reduce HTTP request overhead
- You need HTTP/2 connection pooling for performance
- You want automatic optimization for varying load patterns

**Use Standard HTTP when**:
- You need exactly one HTTP request per result (no batching)
- Your target service doesn't support batch endpoints
- You have consistent, low-volume traffic
- You prefer simpler, more predictable behavior

## Configuration Properties

### Standard HTTP Properties

The following properties work identically to the standard HTTP reaction. See the [HTTP Reaction README](../http/README.md) for detailed documentation:

| Property | Type | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `base_url` | string | No | `"http://localhost"` | Base URL for HTTP requests |
| `token` | string | No | `null` | Bearer token for authentication |
| `timeout_ms` | number | No | `10000` | Request timeout in milliseconds |
| `queries` | object | No | `{}` | Query-specific configurations for individual requests (see HTTP Reaction README) |

### Adaptive-Specific Properties

| Property | Type | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `batch_endpoints_enabled` | boolean | No | `true` | Enable batch endpoint (`POST /batch`). When `false`, sends individual requests only |
| `adaptive_max_batch_size` | number | No | `1000` | Maximum number of results in a single batch |
| `adaptive_min_batch_size` | number | No | `10` | Minimum batch size for efficient batching |
| `adaptive_max_wait_ms` | number | No | `100` | Maximum time to wait for batch to fill (milliseconds) |
| `adaptive_min_wait_ms` | number | No | `1` | Minimum wait time to allow message coalescing (milliseconds) |
| `adaptive_window_secs` | number | No | `5` | Window size for throughput calculation (seconds) |
| `adaptive_enabled` | boolean | No | `true` | Enable adaptive parameter adjustment. When `false`, uses fixed min values |
| `pool_idle_timeout_secs` | number | No | `90` | HTTP connection pool idle timeout (seconds). Hard-coded, not configurable |
| `pool_max_idle_per_host` | number | No | `10` | Maximum idle connections per host. Hard-coded, not configurable |

**Note**: The `pool_idle_timeout_secs` and `pool_max_idle_per_host` properties are currently hard-coded in the implementation and cannot be overridden via configuration.

## Configuration Examples

### YAML Configuration

#### Basic Adaptive Configuration with Batch Endpoint

```yaml
reactions:
  - id: "adaptive-webhook"
    reaction_type: "http_adaptive"
    queries:
      - "user-changes"
    auto_start: true
    properties:
      base_url: "https://api.example.com"
      token: "your-api-token"
      timeout_ms: 10000
      batch_endpoints_enabled: true
      adaptive_max_batch_size: 500
      adaptive_min_batch_size: 20
      adaptive_max_wait_ms: 50
      adaptive_min_wait_ms: 2
```

#### Adaptive Disabled (Individual Requests with Fixed Batching)

```yaml
reactions:
  - id: "fixed-batch-webhook"
    reaction_type: "http_adaptive"
    queries:
      - "sensor-data"
    auto_start: true
    properties:
      base_url: "https://api.example.com"
      token: "your-api-token"
      adaptive_enabled: false
      adaptive_max_batch_size: 100
      adaptive_min_batch_size: 10
      batch_endpoints_enabled: false
      queries:
        sensor-data:
          added:
            url: "/sensors/data"
            method: "POST"
            body: '{{json after}}'
```

#### High-Throughput Batch Configuration

```yaml
reactions:
  - id: "high-volume-sync"
    reaction_type: "http_adaptive"
    queries:
      - "inventory-updates"
      - "order-events"
    auto_start: true
    properties:
      base_url: "https://data-ingestion.example.com"
      token: "${API_TOKEN}"
      timeout_ms: 30000
      batch_endpoints_enabled: true
      adaptive_enabled: true
      adaptive_max_batch_size: 1000
      adaptive_min_batch_size: 50
      adaptive_max_wait_ms: 100
      adaptive_min_wait_ms: 5
      adaptive_window_secs: 10
```

#### HTTP/2 Connection Pooling Settings

Note: These settings are hard-coded and shown here for documentation purposes only.

```yaml
reactions:
  - id: "pooled-connection-example"
    reaction_type: "http_adaptive"
    queries:
      - "real-time-events"
    auto_start: true
    properties:
      base_url: "https://api.example.com"
      token: "your-api-token"
      batch_endpoints_enabled: true
      # Connection pooling is automatic with:
      # - pool_idle_timeout: 90 seconds (hard-coded)
      # - pool_max_idle_per_host: 10 connections (hard-coded)
      # - HTTP/2 enabled with prior knowledge
```

### JSON Configuration

#### Basic Adaptive Configuration

```json
{
  "reactions": [
    {
      "id": "adaptive-webhook",
      "reaction_type": "http_adaptive",
      "queries": ["user-changes"],
      "auto_start": true,
      "properties": {
        "base_url": "https://api.example.com",
        "token": "your-api-token",
        "timeout_ms": 10000,
        "batch_endpoints_enabled": true,
        "adaptive_max_batch_size": 500,
        "adaptive_min_batch_size": 20,
        "adaptive_max_wait_ms": 50,
        "adaptive_min_wait_ms": 2
      }
    }
  ]
}
```

#### Adaptive Disabled (Individual Requests)

```json
{
  "reactions": [
    {
      "id": "fixed-batch-webhook",
      "reaction_type": "http_adaptive",
      "queries": ["sensor-data"],
      "auto_start": true,
      "properties": {
        "base_url": "https://api.example.com",
        "token": "your-api-token",
        "adaptive_enabled": false,
        "batch_endpoints_enabled": false,
        "queries": {
          "sensor-data": {
            "added": {
              "url": "/sensors/data",
              "method": "POST",
              "body": "{{json after}}"
            }
          }
        }
      }
    }
  ]
}
```

#### High-Throughput Batch Configuration

```json
{
  "reactions": [
    {
      "id": "high-volume-sync",
      "reaction_type": "http_adaptive",
      "queries": ["inventory-updates", "order-events"],
      "auto_start": true,
      "properties": {
        "base_url": "https://data-ingestion.example.com",
        "token": "${API_TOKEN}",
        "timeout_ms": 30000,
        "batch_endpoints_enabled": true,
        "adaptive_enabled": true,
        "adaptive_max_batch_size": 1000,
        "adaptive_min_batch_size": 50,
        "adaptive_max_wait_ms": 100,
        "adaptive_min_wait_ms": 5,
        "adaptive_window_secs": 10
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Creating the Adaptive Reaction Configuration

```rust
use drasi_server_core::config::{ReactionConfig, ComponentEventSender};
use drasi_server_core::reactions::http_adaptive::AdaptiveHttpReaction;
use serde_json::json;
use std::collections::HashMap;

// Create basic adaptive reaction configuration
let mut properties = HashMap::new();
properties.insert("base_url".to_string(), json!("https://api.example.com"));
properties.insert("token".to_string(), json!("your-api-token"));
properties.insert("timeout_ms".to_string(), json!(10000));
properties.insert("batch_endpoints_enabled".to_string(), json!(true));

let config = ReactionConfig {
    id: "adaptive-webhook".to_string(),
    reaction_type: "http_adaptive".to_string(),
    queries: vec!["user-changes".to_string()],
    properties,
};

// Create the reaction instance
let reaction = AdaptiveHttpReaction::new(config, event_tx);
```

### Setting Batch Endpoint Options

```rust
use serde_json::json;
use std::collections::HashMap;

let mut properties = HashMap::new();
properties.insert("base_url".to_string(), json!("https://api.example.com"));
properties.insert("token".to_string(), json!("your-api-token"));

// Configure batch endpoint behavior
properties.insert("batch_endpoints_enabled".to_string(), json!(true));

// Disable batch endpoints (fall back to individual requests)
// properties.insert("batch_endpoints_enabled".to_string(), json!(false));

let config = ReactionConfig {
    id: "batch-config-example".to_string(),
    reaction_type: "http_adaptive".to_string(),
    queries: vec!["my-query".to_string()],
    properties,
};
```

### Setting Adaptive Parameters

```rust
use serde_json::json;
use std::collections::HashMap;

let mut properties = HashMap::new();
properties.insert("base_url".to_string(), json!("https://api.example.com"));

// Configure adaptive batching parameters
properties.insert("adaptive_enabled".to_string(), json!(true));
properties.insert("adaptive_max_batch_size".to_string(), json!(1000));
properties.insert("adaptive_min_batch_size".to_string(), json!(50));
properties.insert("adaptive_max_wait_ms".to_string(), json!(100));
properties.insert("adaptive_min_wait_ms".to_string(), json!(5));
properties.insert("adaptive_window_secs".to_string(), json!(10));

let config = ReactionConfig {
    id: "adaptive-params-example".to_string(),
    reaction_type: "http_adaptive".to_string(),
    queries: vec!["high-volume-query".to_string()],
    properties,
};
```

### Configuring Connection Pooling

```rust
use serde_json::json;
use std::collections::HashMap;

// Note: Connection pooling parameters are hard-coded in the implementation
// This example shows the configuration that would be used if they were configurable

let mut properties = HashMap::new();
properties.insert("base_url".to_string(), json!("https://api.example.com"));
properties.insert("token".to_string(), json!("your-api-token"));
properties.insert("batch_endpoints_enabled".to_string(), json!(true));

// The following are hard-coded and cannot be overridden:
// - pool_idle_timeout: 90 seconds
// - pool_max_idle_per_host: 10 connections
// - http2_prior_knowledge: enabled

let config = ReactionConfig {
    id: "pooling-example".to_string(),
    reaction_type: "http_adaptive".to_string(),
    queries: vec!["my-query".to_string()],
    properties,
};
```

### Starting the Reaction

```rust
use drasi_server_core::DrasiServerCore;
use drasi_server_core::reactions::Reaction;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create event channel
    let (event_tx, event_rx) = mpsc::channel(100);

    // Create reaction configuration
    let mut properties = HashMap::new();
    properties.insert("base_url".to_string(), json!("https://api.example.com"));
    properties.insert("batch_endpoints_enabled".to_string(), json!(true));

    let config = ReactionConfig {
        id: "my-adaptive-reaction".to_string(),
        reaction_type: "http_adaptive".to_string(),
        queries: vec!["my-query".to_string()],
        properties,
    };

    // Create reaction instance
    let reaction = AdaptiveHttpReaction::new(config, event_tx);

    // Create result channel for query results
    let (result_tx, result_rx) = mpsc::channel(1000);

    // Start the reaction
    reaction.start(result_rx).await?;

    Ok(())
}
```

## Input Data Format

The Adaptive HTTP reaction receives query results in the same format as the standard HTTP reaction. See the [HTTP Reaction README](../http/README.md#input-data-format) for the complete `QueryResult` structure and result types (ADD, UPDATE, DELETE operations).

## Output Data Format

The Adaptive HTTP reaction supports two output modes:

1. **Batch Endpoint Format** - When `batch_endpoints_enabled` is `true` and multiple results are batched
2. **Individual Request Format** - When `batch_endpoints_enabled` is `false` or only single results are available

### Batch Endpoint Format

When batching is enabled and multiple results are grouped together, the reaction sends a POST request to the `/batch` endpoint with the following format:

**Endpoint**: `POST {base_url}/batch`

**Request Body**: Array of `BatchResult` objects

```json
[
  {
    "query_id": "user-changes",
    "results": [
      {
        "type": "ADD",
        "data": {
          "id": "user_123",
          "name": "John Doe",
          "email": "john@example.com"
        }
      },
      {
        "type": "UPDATE",
        "data": {
          "id": "user_456",
          "name": "Jane Smith",
          "email": "jane@example.com"
        },
        "before": {
          "id": "user_456",
          "name": "Jane Doe",
          "email": "jane@example.com"
        },
        "after": {
          "id": "user_456",
          "name": "Jane Smith",
          "email": "jane@example.com"
        }
      }
    ],
    "timestamp": "2025-10-19T12:34:56.789Z",
    "count": 2
  },
  {
    "query_id": "order-events",
    "results": [
      {
        "type": "ADD",
        "data": {
          "order_id": "order_789",
          "total": 125.50,
          "status": "pending"
        }
      }
    ],
    "timestamp": "2025-10-19T12:34:56.789Z",
    "count": 1
  }
]
```

**BatchResult Structure**:
- `query_id` (String): The ID of the query that produced these results
- `results` (Array): Array of query result objects (each with type, data, before/after fields)
- `timestamp` (String): ISO 8601 timestamp when the batch was created
- `count` (Number): Number of results in this batch (same as `results.length`)

**Headers**:
- `Content-Type: application/json` (always included)
- `Authorization: Bearer {token}` (if `token` is configured)

See [batch-format.tsp](batch-format.tsp) for the TypeSpec definition.

### Individual Request Format

When `batch_endpoints_enabled` is `false`, or when the adaptive batcher produces batches with only single results, the reaction falls back to individual HTTP requests using the same format as the standard HTTP reaction.

Individual requests use the `queries` configuration with `added`, `updated`, and `deleted` CallSpec definitions. See the [HTTP Reaction README](../http/README.md#output-data-format) for:
- Template context variables
- CallSpec structure
- Example HTTP requests for ADD, UPDATE, DELETE operations
- Handlebars template usage

## Adaptive Batching Behavior

The Adaptive HTTP reaction uses a sophisticated algorithm to optimize batch size and timing based on observed message throughput.

### How the Adaptive Algorithm Works

The adaptive batcher monitors throughput over a configurable window (default: 5 seconds) and classifies traffic into five levels:

| Throughput Level | Messages/Second | Batch Size | Wait Time |
|-----------------|-----------------|------------|-----------|
| **Idle** | < 1 | min_batch_size (10) | min_wait_time (1ms) |
| **Low** | 1-100 | 2 × min_batch_size (20) | 1ms |
| **Medium** | 100-1,000 | 25% of max_batch_size (250) | 10ms |
| **High** | 1,000-10,000 | 50% of max_batch_size (500) | 25ms |
| **Burst** | > 10,000 | max_batch_size (1000) | 50ms |

**Adaptive Behavior**:
1. **Throughput Monitoring**: Tracks messages per second in a rolling window
2. **Parameter Adjustment**: Adjusts batch size and wait time based on current throughput level
3. **Burst Detection**: Detects when many messages are pending and fills batches completely
4. **Latency Optimization**: Uses small batches and minimal wait time during idle/low traffic for low latency
5. **Throughput Optimization**: Uses large batches and longer wait time during burst traffic for high throughput

### When Batch Endpoint is Used vs Individual Requests

The reaction decides whether to use the batch endpoint or individual requests based on:

1. **`batch_endpoints_enabled` flag**:
   - `true` (default): Use batch endpoint when batches contain multiple results
   - `false`: Always use individual requests, regardless of batch size

2. **Batch content**:
   - If batch contains multiple results for any query: Use `POST /batch` with array of `BatchResult` objects
   - If batch contains only single results: Use individual requests with CallSpec configuration

### How the `batch_endpoints_enabled` Flag Affects Behavior

**When `batch_endpoints_enabled` is `true`** (default):
```
Multiple Results → POST /batch (batch format)
Single Result → Individual request (CallSpec format)
```

**When `batch_endpoints_enabled` is `false`**:
```
All Results → Individual requests (CallSpec format)
```

**Example Scenarios**:

1. **High-volume traffic with batch endpoint enabled**:
   - Adaptive batcher creates batches of 500-1000 results
   - Sends `POST /batch` with array of results
   - Reduces HTTP overhead, increases throughput

2. **Low-volume traffic with batch endpoint enabled**:
   - Adaptive batcher creates batches of 10-20 results
   - Sends `POST /batch` with small array
   - Minimizes latency while still benefiting from batching

3. **Any traffic with batch endpoint disabled**:
   - Each result sent individually using CallSpec
   - Allows per-operation customization (different URLs/methods for ADD/UPDATE/DELETE)
   - Compatible with APIs that don't support batch endpoints

### Adaptive Parameter Effects on Batch Size

**Key Parameters**:
- `adaptive_max_batch_size`: Maximum results per batch (default: 1000)
- `adaptive_min_batch_size`: Minimum batch size for efficient batching (default: 10)
- `adaptive_max_wait_ms`: Maximum wait time for batch to fill (default: 100ms)
- `adaptive_min_wait_ms`: Minimum wait time for message coalescing (default: 1ms)
- `adaptive_window_secs`: Throughput measurement window (default: 5 seconds)
- `adaptive_enabled`: Enable/disable adaptive adjustment (default: true)

**Parameter Impact**:
- Larger `max_batch_size`: Allows more results per batch during burst traffic
- Larger `min_batch_size`: Ensures minimum efficiency threshold for batching
- Longer `max_wait_ms`: Allows more time to collect results during medium/high traffic
- Shorter `min_wait_ms`: Reduces latency during idle/low traffic
- Longer `window_secs`: Smoother adaptation to traffic changes, less reactive
- `adaptive_enabled = false`: Uses fixed `min_batch_size` and `min_wait_time` values

**Tuning Recommendations**:
- For **low-latency scenarios**: Decrease `min_wait_ms`, `max_wait_ms`, and `min_batch_size`
- For **high-throughput scenarios**: Increase `max_batch_size` and `max_wait_ms`
- For **bursty traffic**: Increase `max_batch_size` and use default adaptive settings
- For **predictable traffic**: Set `adaptive_enabled = false` and tune fixed batch parameters

## HTTP/2 Connection Pooling

The Adaptive HTTP reaction uses HTTP/2 connection pooling to improve performance by reusing connections across multiple requests.

### Connection Pooling Behavior

The reaction creates a single `reqwest::Client` instance with the following configuration:

1. **HTTP/2 Prior Knowledge**: Attempts to use HTTP/2 protocol without negotiation
2. **Connection Reuse**: Maintains persistent connections to target hosts
3. **Automatic Pooling**: Automatically manages connection pool lifecycle
4. **Per-Reaction Pools**: Each reaction instance has its own connection pool (pools are not shared)

### Connection Pool Settings

**Hard-Coded Settings** (currently not configurable):

| Setting | Value | Description |
|---------|-------|-------------|
| `pool_idle_timeout` | 90 seconds | How long idle connections are kept alive |
| `pool_max_idle_per_host` | 10 connections | Maximum idle connections per host |
| `http2_prior_knowledge` | enabled | Use HTTP/2 without ALPN negotiation |

### Connection Reuse

Connections are automatically reused when:
- Multiple requests are sent to the same host
- Connections are within the idle timeout period
- Connection pool has not reached the maximum idle limit

**Connection Lifecycle**:
1. First request to a host creates a new connection
2. Connection remains open after request completes
3. Subsequent requests to the same host reuse the existing connection
4. Idle connections are closed after 90 seconds of inactivity
5. Pool maintains up to 10 idle connections per host

### Performance Benefits

**Reduced Latency**:
- Eliminates TCP handshake for subsequent requests
- Avoids TLS negotiation overhead
- Reduces connection establishment time

**Improved Throughput**:
- Allows request multiplexing over a single connection (HTTP/2 feature)
- Reduces connection overhead for high-volume scenarios
- Better utilization of network resources

**Resource Efficiency**:
- Fewer TCP connections reduce kernel overhead
- Lower memory usage compared to one-connection-per-request
- Better scaling for multiple concurrent reactions

## Performance Tuning

### When to Enable Batch Endpoints

**Enable batch endpoints (`batch_endpoints_enabled: true`)** when:
- Target service provides a `/batch` endpoint
- You expect high-volume or bursty traffic
- You want to minimize HTTP request overhead
- Latency requirements allow for batching delays (1-100ms)
- Target service can efficiently process batch requests

**Disable batch endpoints (`batch_endpoints_enabled: false`)** when:
- Target service doesn't support batch processing
- You need per-operation customization (different URLs/methods)
- You require immediate delivery of each result
- Target service performs better with individual requests
- You want predictable, one-request-per-result behavior

### Setting Adaptive Parameters

#### Low-Latency Scenarios

Optimize for minimal latency at the cost of throughput:

```yaml
properties:
  batch_endpoints_enabled: true  # Or false for individual requests
  adaptive_enabled: true
  adaptive_min_batch_size: 1     # Send immediately
  adaptive_max_batch_size: 50    # Keep batches small
  adaptive_min_wait_ms: 1        # Minimal delay
  adaptive_max_wait_ms: 10       # Short wait time
  adaptive_window_secs: 2        # Quick adaptation
```

**Use cases**: Real-time notifications, interactive applications, low-volume updates

#### High-Throughput Scenarios

Optimize for maximum throughput at the cost of latency:

```yaml
properties:
  batch_endpoints_enabled: true
  adaptive_enabled: true
  adaptive_min_batch_size: 100   # Larger minimum batch
  adaptive_max_batch_size: 2000  # Very large batches
  adaptive_min_wait_ms: 10       # Allow some coalescing
  adaptive_max_wait_ms: 200      # Longer wait for more results
  adaptive_window_secs: 10       # Stable adaptation
```

**Use cases**: Bulk data synchronization, analytics pipelines, high-volume event streams

#### Balanced Configuration

Balance latency and throughput for mixed workloads:

```yaml
properties:
  batch_endpoints_enabled: true
  adaptive_enabled: true
  adaptive_min_batch_size: 10    # Default
  adaptive_max_batch_size: 1000  # Default
  adaptive_min_wait_ms: 1        # Default
  adaptive_max_wait_ms: 100      # Default
  adaptive_window_secs: 5        # Default
```

**Use cases**: General-purpose integrations, variable traffic patterns, mixed priorities

### Connection Pool Tuning

**Current Limitations**: Connection pool parameters are hard-coded and cannot be tuned. If configurable in the future, consider:

**For High-Volume Scenarios**:
- Increase `pool_max_idle_per_host` to handle more concurrent connections
- Increase `pool_idle_timeout` to keep connections alive longer
- Ensure target service can handle persistent connections

**For Low-Volume Scenarios**:
- Decrease `pool_idle_timeout` to free resources faster
- Decrease `pool_max_idle_per_host` to reduce resource usage
- Connection pooling still provides benefits for intermittent traffic

### Performance Monitoring

Monitor these metrics to tune performance:

1. **Batch Size**: Log messages show actual batch sizes vs. target
2. **Throughput Level**: Logs indicate current traffic level (Idle, Low, Medium, High, Burst)
3. **Messages Per Second**: Calculated throughput in the adaptive window
4. **HTTP Response Times**: Monitor target service latency
5. **Error Rates**: Watch for timeouts or connection issues

**Example Debug Output**:
```
[adaptive-webhook] Adapted batching parameters - Level: High, Rate: 2543.1 msgs/sec, Batch: 500, Wait: 25ms
[adaptive-webhook] Adaptive batch collected - Size: 487, Target: 500, Wait: 25ms, Level: High
[adaptive-webhook] Processing adaptive batch of 487 results
[adaptive-webhook] Sending batch of 487 results to https://api.example.com/batch
[adaptive-webhook] Batch sent successfully
```

Enable debug logging:
```bash
RUST_LOG=debug cargo run
```

Or in configuration:
```yaml
server:
  log_level: "debug"
```

### Recommendations by Use Case

**Real-Time Event Notifications**:
```yaml
batch_endpoints_enabled: false
adaptive_enabled: false
timeout_ms: 5000
# Use CallSpec for custom per-event URLs
```

**High-Volume Analytics Ingestion**:
```yaml
batch_endpoints_enabled: true
adaptive_enabled: true
adaptive_max_batch_size: 2000
adaptive_max_wait_ms: 200
timeout_ms: 30000
```

**Mixed Workload Integration**:
```yaml
batch_endpoints_enabled: true
adaptive_enabled: true
# Use defaults for automatic optimization
```

**Bursty Traffic Patterns**:
```yaml
batch_endpoints_enabled: true
adaptive_enabled: true
adaptive_max_batch_size: 1000
adaptive_window_secs: 10  # Longer window for stability
```

## Troubleshooting

### Common Issues and Solutions

#### 1. Batch Endpoint Not Found (404)

**Symptoms**:
- HTTP 404 errors in logs
- Target service returns "Not Found"
- Logs show: `Batch HTTP request failed with status 404`

**Causes**:
- Target service doesn't implement `/batch` endpoint
- Incorrect `base_url` configuration
- Batch endpoint has a different path

**Solutions**:
1. **Verify batch endpoint exists**:
   ```bash
   curl -X POST https://api.example.com/batch \
     -H "Content-Type: application/json" \
     -d '[]'
   ```

2. **Disable batch endpoints** if not supported:
   ```yaml
   properties:
     batch_endpoints_enabled: false
   ```

3. **Check base_url configuration**:
   ```yaml
   properties:
     base_url: "https://api.example.com"  # Should not include /batch
   ```

4. **Implement batch endpoint** on target service (see batch-format.tsp for specification)

#### 2. Individual Request Fallback Behavior

**Symptoms**:
- Expected batches but seeing individual requests
- Logs show individual CallSpec requests instead of batch posts

**Causes**:
- `batch_endpoints_enabled` set to `false`
- Batches contain only single results
- Adaptive batcher producing small batches during low traffic

**Diagnosis**:
Check configuration:
```yaml
properties:
  batch_endpoints_enabled: true  # Must be true for batching
```

Check logs for batch size:
```
Adaptive batch collected - Size: 1, Target: 10, Wait: 1ms, Level: Idle
```

**Solutions**:
1. **Enable batch endpoints**:
   ```yaml
   batch_endpoints_enabled: true
   ```

2. **Increase minimum batch size** (only if you want to force larger batches):
   ```yaml
   adaptive_min_batch_size: 20
   ```

3. **Accept individual requests** during low traffic - this is expected adaptive behavior

#### 3. Connection Pool Exhaustion

**Symptoms**:
- Timeouts during high load
- Connection errors in logs
- Performance degradation under load

**Causes**:
- Too many concurrent reactions
- Target service limiting connections
- Hard-coded pool limits too low

**Solutions**:
1. **Monitor connection usage** - check target service connection limits

2. **Increase timeout** to allow connections to complete:
   ```yaml
   timeout_ms: 30000  # 30 seconds
   ```

3. **Deploy multiple reaction instances** with load balancing if needed

4. **Contact developers** if pool limits need to be configurable

**Current Limitation**: `pool_max_idle_per_host` is hard-coded to 10 connections and cannot be increased via configuration.

#### 4. HTTP/2 Issues

**Symptoms**:
- Connection errors
- Unexpected protocol errors
- Target service rejecting requests

**Causes**:
- Target service doesn't support HTTP/2
- HTTP/2 prior knowledge misconfiguration
- TLS certificate issues

**Diagnosis**:
Test HTTP/2 support:
```bash
curl --http2 -v https://api.example.com/batch
```

**Solutions**:
1. **Verify target supports HTTP/2** - check API documentation

2. **Check TLS configuration** - ensure valid certificates

3. **Test with standard HTTP reaction** to isolate HTTP/2 issues:
   ```yaml
   reaction_type: "http"  # Standard reaction uses HTTP/1.1
   ```

**Current Limitation**: HTTP/2 prior knowledge is hard-coded and cannot be disabled. If target service doesn't support HTTP/2, consider using the standard HTTP reaction.

#### 5. Batching Latency Too High

**Symptoms**:
- Results delayed by 50-100ms
- Poor responsiveness during low traffic
- Latency-sensitive applications suffering

**Causes**:
- `adaptive_max_wait_ms` too high
- Adaptive algorithm using long wait times
- Batching not appropriate for use case

**Solutions**:
1. **Reduce wait times**:
   ```yaml
   adaptive_max_wait_ms: 10   # From default 100ms
   adaptive_min_wait_ms: 1
   ```

2. **Disable adaptive mode** for predictable latency:
   ```yaml
   adaptive_enabled: false
   adaptive_min_batch_size: 1
   adaptive_min_wait_ms: 1
   ```

3. **Disable batching entirely**:
   ```yaml
   batch_endpoints_enabled: false
   ```

4. **Use standard HTTP reaction** for guaranteed single-request-per-result:
   ```yaml
   reaction_type: "http"
   ```

#### 6. Batches Too Small or Too Large

**Symptoms**:
- Batch sizes not matching expectations
- Inefficient batching patterns
- Logs showing unexpected batch sizes

**Diagnosis**:
Check logs for adaptive behavior:
```
Adapted batching parameters - Level: Medium, Rate: 234.5 msgs/sec, Batch: 250, Wait: 10ms
Adaptive batch collected - Size: 178, Target: 250, Wait: 10ms
```

**Solutions**:
1. **Adjust batch size limits**:
   ```yaml
   adaptive_min_batch_size: 50   # Raise minimum
   adaptive_max_batch_size: 500  # Lower maximum
   ```

2. **Disable adaptive mode** for fixed batch size:
   ```yaml
   adaptive_enabled: false
   adaptive_min_batch_size: 100  # Fixed size
   ```

3. **Adjust window size** for more/less reactive adaptation:
   ```yaml
   adaptive_window_secs: 10  # Longer window = smoother adaptation
   ```

### All Standard HTTP Issues

The Adaptive HTTP reaction is subject to all issues documented in the standard HTTP reaction. See the [HTTP Reaction README](../http/README.md#troubleshooting) for:

- Template syntax errors
- Variable not found in context
- HTTP errors (4xx, 5xx)
- Timeout issues
- Authentication failures
- Invalid JSON in body

### Debug Logging

Enable debug logging to diagnose issues:

```yaml
# In server configuration
server:
  log_level: "debug"
```

Or set environment variable:
```bash
RUST_LOG=debug cargo run
```

**Adaptive-Specific Debug Output**:
```
[adaptive-webhook] Adaptive HTTP batcher started
[adaptive-webhook] Adapted batching parameters - Level: High, Rate: 3421.2 msgs/sec, Batch: 500, Wait: 25ms
[adaptive-webhook] Processing adaptive batch of 487 results
[adaptive-webhook] Sending batch of 487 results to https://api.example.com/batch
[adaptive-webhook] Batch sent successfully
[adaptive-webhook] Adaptive HTTP metrics - Batches: 100, Results: 48532, Avg batch size: 485.3
```

## Limitations

### All Standard HTTP Reaction Limitations

The Adaptive HTTP reaction inherits all limitations from the standard HTTP reaction. See the [HTTP Reaction README](../http/README.md#limitations) for:

- HTTP version support (HTTP/1.1, HTTP/2)
- Request size limits
- Performance considerations
- Resource usage
- No automatic retry logic
- No response validation
- No circuit breaking

### Additional Adaptive-Specific Limitations

#### Requires Batch Endpoint Support

- Target service must implement `POST /batch` endpoint to benefit from batching
- Batch endpoint must accept array of `BatchResult` objects
- No fallback if batch endpoint exists but has different format
- See [batch-format.tsp](batch-format.tsp) for required format

**Workaround**: Set `batch_endpoints_enabled: false` to use individual requests only.

#### HTTP/2 Requirements

- HTTP/2 prior knowledge is always enabled (cannot be disabled)
- Target service must support HTTP/2 or gracefully fall back to HTTP/1.1
- TLS configuration must be compatible with HTTP/2

**Workaround**: Use standard HTTP reaction if HTTP/2 causes issues.

#### Connection Pooling Constraints

- Pool parameters are hard-coded and not configurable:
  - `pool_idle_timeout`: Fixed at 90 seconds
  - `pool_max_idle_per_host`: Fixed at 10 connections
- Connection pools are not shared between reaction instances
- No control over connection pool behavior

**Workaround**: Deploy multiple reaction instances for higher connection requirements.

#### Adaptive Algorithm Overhead

- Throughput monitoring adds CPU and memory overhead
- Adaptation calculations performed on every batch
- Logging adds I/O overhead in debug mode
- Slightly higher resource usage compared to standard HTTP reaction

**Impact**: Minimal for most use cases, but consider for extremely high-volume scenarios (>100k msgs/sec).

**Workaround**: Set `adaptive_enabled: false` to disable adaptation overhead.

#### Batching Delays

- Introduces latency (1-100ms) depending on traffic level and configuration
- Latency varies based on adaptive algorithm decisions
- Not suitable for applications requiring immediate delivery (<1ms latency)

**Workaround**:
- Use smaller `adaptive_max_wait_ms` values
- Set `adaptive_enabled: false` with minimal wait time
- Use standard HTTP reaction for zero batching delay

#### No Ordering Guarantees Across Queries

- Results from different queries may be reordered when batched
- Batches group by query_id, but order within batch may vary
- No cross-query ordering guarantee

**Impact**: Within a single query, results maintain order. Across queries, ordering may vary.

**Workaround**: Use separate reactions for queries requiring strict ordering.

#### No Partial Batch Retry

- If batch POST fails, entire batch is lost (logged but not retried)
- No automatic retry mechanism for failed batches
- No transaction support for partial batch failures

**Workaround**: Implement retry logic in target service or use message queue middleware.

#### Batch Size Limitations

- Maximum batch size configurable but constrained by:
  - Target service request size limits
  - Network MTU and packet size
  - Memory available for buffering
  - Timeout settings
- Very large batches (>10k results) may timeout or exceed service limits

**Workaround**: Tune `adaptive_max_batch_size` based on target service capabilities.

### Future Enhancements

Potential improvements not yet implemented:

- Configurable connection pool parameters
- HTTP/2 enable/disable option
- Batch retry logic with exponential backoff
- Circuit breaker pattern for failing endpoints
- Response validation and parsing
- Custom batch endpoint paths
- Compression support for large batches
- Metrics export (Prometheus, etc.)
