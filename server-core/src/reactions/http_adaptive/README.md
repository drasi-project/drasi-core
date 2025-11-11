# Adaptive HTTP Reaction

The Adaptive HTTP reaction extends the standard HTTP reaction with intelligent batching and HTTP/2 connection pooling. It automatically adjusts batch size and timing based on throughput patterns to optimize for either low latency or high throughput scenarios.

## Key Differences from Standard HTTP Reaction

The standard HTTP reaction sends one HTTP request per query result. The Adaptive HTTP reaction adds:

1. **Intelligent Batching**: Groups multiple results based on traffic patterns
2. **Batch Endpoint Support**: Sends batches to a dedicated `/batch` endpoint
3. **Adaptive Algorithm**: Dynamically adjusts batch size (10-1000 items) and wait time (1-100ms)
4. **HTTP/2 Connection Pooling**: Maintains persistent connections for better performance

## When to Use

**Use Adaptive HTTP when**:
- You expect high-volume or bursty traffic patterns
- Your target service supports batch endpoints (`POST /batch`)
- You want to reduce HTTP request overhead
- You need automatic optimization for varying load patterns

**Use Standard HTTP when**:
- You need exactly one HTTP request per result
- Your target service doesn't support batch endpoints
- You have consistent, low-volume traffic
- You prefer simpler, more predictable behavior

## Configuration Properties

### Standard HTTP Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `base_url` | string | `"http://localhost"` | Base URL for HTTP requests |
| `token` | string | `null` | Bearer token for authentication |
| `timeout_ms` | number | `10000` | Request timeout in milliseconds |
| `queries` | object | `{}` | Query-specific configurations (see [HTTP Reaction README](../http/README.md)) |

### Adaptive-Specific Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `adaptive_max_batch_size` | number | `100` | Maximum number of results in a batch |
| `adaptive_min_batch_size` | number | `1` | Minimum batch size for efficient batching |
| `adaptive_window_size` | number | `10` | Window size in 100ms units (1-255) |
| `adaptive_batch_timeout_ms` | number | `1000` | Maximum wait time before flush (ms) |

**Note**:
- The `adaptive_window_size` is measured in 100ms units. For example, a value of 10 equals 1 second, 50 equals 5 seconds, etc.
- Batch endpoints (`POST /batch`) are always enabled in Adaptive HTTP reactions.
- Connection pool settings (`pool_idle_timeout: 90s`, `pool_max_idle_per_host: 10`) are hard-coded and not configurable.

## Route Configuration and Templates

The Adaptive HTTP reaction supports the same route configuration and Handlebars templating as the [standard HTTP reaction](../http/README.md#configuration-properties). You can configure per-query routes with custom URLs, methods, bodies, and headers using Handlebars templates.

**Key template variables available**:
- `{{query_id}}` - Query identifier
- `{{operation}}` - Operation type (`"added"`, `"updated"`, `"deleted"`)
- `{{data}}` - Result data
- `{{after}}` - New values (for ADD and UPDATE)
- `{{before}}` - Previous values (for UPDATE and DELETE)

For detailed route configuration and templating examples, see the [HTTP Reaction Routes documentation](../http/README.md#configuration-examples).

**Note**: When using individual routes (not batch endpoints), the adaptive batching still applies - results are batched by the batcher, but each result in the batch is sent as a separate HTTP request according to its route configuration.

## Configuration Examples

### Basic Adaptive Configuration

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
      adaptive_max_batch_size: 500
      adaptive_min_batch_size: 20
      adaptive_window_size: 10  # 1 second
      adaptive_batch_timeout_ms: 1000
```

### High-Throughput Configuration

```yaml
reactions:
  - id: "high-volume-sync"
    reaction_type: "http_adaptive"
    queries:
      - "inventory-updates"
    properties:
      base_url: "https://data-ingestion.example.com"
      adaptive_max_batch_size: 1000
      adaptive_min_batch_size: 50
      adaptive_window_size: 100  # 10 seconds
      adaptive_batch_timeout_ms: 100
```

### Query-Specific Routes

```yaml
reactions:
  - id: "routed-webhook"
    reaction_type: "http_adaptive"
    queries:
      - "sensor-data"
    properties:
      base_url: "https://api.example.com"
      routes:
        sensor-data:
          added:
            url: "/sensors/data"
            method: "POST"
            body: '{{json after}}'
```

## Batch Endpoint Format

When multiple results are available, the reaction sends a POST request to `{base_url}/batch` with the following format:

### Request Format

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

### BatchResult Structure

- `query_id` (String): The ID of the query that produced these results
- `results` (Array): Array of query result objects with type (ADD/UPDATE/DELETE) and data
- `timestamp` (String): ISO 8601 timestamp when the batch was created
- `count` (Number): Number of results in this batch

### Headers

- `Content-Type: application/json` (always included)
- `Authorization: Bearer {token}` (if configured)

See [batch-format.tsp](batch-format.tsp) for the complete TypeSpec definition.

### Individual Request Format

When only single results are available, the reaction uses individual HTTP requests with query-specific routes. See the [HTTP Reaction README](../http/README.md#output-data-format) for details on CallSpec configuration and templating with the `routes` property.

## Adaptive Batching Algorithm

The adaptive batcher monitors throughput over a configurable window (default: 1 second / window_size=10) and adjusts parameters based on traffic level:

| Throughput Level | Messages/Second | Batch Size | Wait Time |
|-----------------|-----------------|------------|-----------|
| **Idle** | < 1 | min_batch_size (1) | 1ms |
| **Low** | 1-100 | 2 × min_batch_size (2) | 1ms |
| **Medium** | 100-1,000 | 25% of max (25) | 10ms |
| **High** | 1,000-10,000 | 50% of max (50) | 25ms |
| **Burst** | > 10,000 | max_batch_size (100) | 50ms |

### Adaptive Behavior

1. **Throughput Monitoring**: Tracks messages per second in a rolling window (measured in 100ms units)
2. **Parameter Adjustment**: Adjusts batch size and wait time based on current throughput
3. **Burst Detection**: Detects pending messages and fills batches completely
4. **Latency Optimization**: Uses small batches during idle/low traffic
5. **Throughput Optimization**: Uses large batches during burst traffic

### Batch Endpoint Behavior

**Multiple results available**:
- Sends to `POST /batch` endpoint with batch format

**Single result available**:
- Sends individual request using query-specific routes (if configured)
- If no route configured for operation, result is skipped

## Performance Tuning

### Low-Latency Scenarios

Optimize for minimal latency:

```yaml
properties:
  adaptive_min_batch_size: 1           # Send immediately
  adaptive_max_batch_size: 50          # Keep batches small
  adaptive_window_size: 30             # 3 seconds - fast adaptation
  adaptive_batch_timeout_ms: 10        # Short wait time
```

**Use cases**: Real-time notifications, interactive applications, low-volume updates

### High-Throughput Scenarios

Optimize for maximum throughput:

```yaml
properties:
  adaptive_min_batch_size: 100         # Larger minimum batch
  adaptive_max_batch_size: 2000        # Very large batches
  adaptive_window_size: 100            # 10 seconds - stable adaptation
  adaptive_batch_timeout_ms: 200       # Longer wait for more results
```

**Use cases**: Bulk data synchronization, analytics pipelines, high-volume event streams

### Tuning Parameters

**Key parameters and their effects**:
- Larger `adaptive_max_batch_size`: Allows more results per batch during burst traffic
- Larger `adaptive_min_batch_size`: Ensures minimum efficiency threshold
- Longer `adaptive_batch_timeout_ms`: Allows more time to collect results
- Longer `adaptive_window_size`: Smoother adaptation to traffic changes (measured in 100ms units)

## HTTP/2 Connection Pooling

The reaction uses HTTP/2 connection pooling to improve performance:

### Connection Pool Settings (Hard-Coded)

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

### Performance Benefits

**Reduced Latency**:
- Eliminates TCP handshake for subsequent requests
- Avoids TLS negotiation overhead

**Improved Throughput**:
- Allows request multiplexing over a single connection
- Reduces connection overhead for high-volume scenarios

**Resource Efficiency**:
- Fewer TCP connections reduce kernel overhead
- Better scaling for multiple concurrent reactions

## Troubleshooting

### Batch Endpoint Not Found (404)

**Symptoms**: HTTP 404 errors, target service returns "Not Found"

**Solutions**:
1. Verify batch endpoint exists: `curl -X POST https://api.example.com/batch`
2. Implement batch endpoint on target service (see [batch-format.tsp](batch-format.tsp))
3. Use standard HTTP reaction if batch endpoint cannot be implemented

### Batching Latency Too High

**Symptoms**: Results delayed by 50-1000ms, poor responsiveness

**Solutions**:
1. Reduce wait times: `adaptive_batch_timeout_ms: 10`
2. Reduce window size for faster adaptation: `adaptive_window_size: 30` (3 seconds)
3. Use standard HTTP reaction for zero batching delay

### Batches Too Small or Too Large

**Symptoms**: Batch sizes not matching expectations

**Solutions**:
1. Adjust batch size limits:
   ```yaml
   adaptive_min_batch_size: 50   # Raise minimum
   adaptive_max_batch_size: 500  # Lower maximum
   ```
2. Adjust window size to change adaptation speed:
   ```yaml
   adaptive_window_size: 50      # 5 seconds for more stable batching
   ```

### Connection Pool Exhaustion

**Symptoms**: Timeouts during high load, connection errors

**Solutions**:
1. Monitor connection usage
2. Increase timeout: `timeout_ms: 30000`
3. Deploy multiple reaction instances with load balancing

**Current Limitation**: `pool_max_idle_per_host` is hard-coded to 10 and cannot be increased.

### Debug Logging

Enable debug logging to diagnose issues:

```bash
# Enable all debug logs
RUST_LOG=debug cargo run

# Or enable only HTTP Adaptive reaction logs
RUST_LOG=drasi_server_core::reactions::http_adaptive=debug cargo run
```

**Example debug output**:
```
[adaptive-webhook] Adaptive HTTP batcher started
[adaptive-webhook] Processing adaptive batch of 487 results
[adaptive-webhook] Sending batch of 487 results to https://api.example.com/batch
[adaptive-webhook] Batch sent successfully
[adaptive-webhook] Adaptive HTTP metrics - Batches: 100, Results: 48700, Avg batch size: 487.0
```

## Limitations

### Inherited from Standard HTTP Reaction

The Adaptive HTTP reaction inherits all limitations from the standard HTTP reaction. See the [HTTP Reaction README](../http/README.md#limitations) for details on:
- HTTP version support
- Request size limits
- No automatic retry logic
- No response validation
- No circuit breaking

### Adaptive-Specific Limitations

**Requires Batch Endpoint Support**:
- Target service must implement `POST /batch` endpoint to benefit from batching
- Batch endpoint must accept array of `BatchResult` objects (see [batch-format.tsp](batch-format.tsp))
- **Workaround**: Use standard HTTP reaction for individual requests only

**HTTP/2 Requirements**:
- HTTP/2 prior knowledge is always enabled (cannot be disabled)
- Target service must support HTTP/2 or gracefully fall back to HTTP/1.1
- **Workaround**: Use standard HTTP reaction if HTTP/2 causes issues

**Connection Pooling Constraints**:
- Pool parameters are hard-coded and not configurable
- Connection pools are not shared between reaction instances
- **Workaround**: Deploy multiple reaction instances for higher connection requirements

**Batching Delays**:
- Introduces latency (1-100ms) depending on traffic level and configuration
- Not suitable for applications requiring immediate delivery (<1ms latency)
- **Workaround**: Use smaller `adaptive_max_wait_ms` values or standard HTTP reaction

**No Partial Batch Retry**:
- If batch POST fails, entire batch is lost (logged but not retried)
- No automatic retry mechanism for failed batches
- **Workaround**: Implement retry logic in target service or use message queue middleware

**Batch Size Limitations**:
- Very large batches (>10k results) may timeout or exceed service limits
- **Workaround**: Tune `adaptive_max_batch_size` based on target service capabilities

## Receiver Endpoint Requirements

To receive batches from the Adaptive HTTP reaction, your endpoint must:

1. **Accept POST requests** at `{base_url}/batch`
2. **Parse JSON array** of `BatchResult` objects
3. **Handle different operation types**: ADD, UPDATE, DELETE
4. **Process multiple queries** in a single batch (if subscribing to multiple queries)
5. **Return appropriate HTTP status codes**:
   - `200-299`: Success
   - `4xx/5xx`: Error (logged by reaction)

### Example Node.js Receiver

```javascript
app.post('/batch', express.json(), (req, res) => {
  const batches = req.body; // Array of BatchResult

  for (const batch of batches) {
    console.log(`Processing ${batch.count} results from query ${batch.query_id}`);

    for (const result of batch.results) {
      switch (result.type) {
        case 'ADD':
          handleAdd(result.data);
          break;
        case 'UPDATE':
          handleUpdate(result.before, result.after);
          break;
        case 'DELETE':
          handleDelete(result.data);
          break;
      }
    }
  }

  res.json({
    status: 'success',
    batches_processed: batches.length,
    total_results: batches.reduce((sum, b) => sum + b.count, 0)
  });
});
```

### Example Python Receiver

```python
from flask import Flask, request, jsonify

@app.route('/batch', methods=['POST'])
def handle_batch():
    batches = request.json
    total_results = 0

    for batch in batches:
        query_id = batch['query_id']
        results = batch['results']
        total_results += batch['count']

        for result in results:
            if result['type'] == 'ADD':
                handle_add(result['data'])
            elif result['type'] == 'UPDATE':
                handle_update(result['before'], result['after'])
            elif result['type'] == 'DELETE':
                handle_delete(result['data'])

    return jsonify({
        'status': 'success',
        'batches_processed': len(batches),
        'total_results': total_results
    })
```

## See Also

- [HTTP Reaction README](../http/README.md) - Standard HTTP reaction documentation
- [batch-format.tsp](batch-format.tsp) - Complete TypeSpec batch format specification
- [Batch Format Example](batch-format.tsp#L220-L301) - Example batch request payload
