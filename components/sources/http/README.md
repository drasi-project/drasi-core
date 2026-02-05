# HTTP Source

A Drasi source plugin that exposes HTTP endpoints for receiving data change events. It supports two mutually exclusive modes: **Standard Mode** for structured events and **Webhook Mode** for receiving arbitrary payloads from external services like GitHub, Shopify, or custom applications.

## Overview

The HTTP Source is a plugin for the Drasi continuous query system that allows applications to submit graph data changes (nodes and relations) via REST API endpoints. It features:

- **REST API Endpoints**: Simple HTTP POST interface for submitting events
- **Adaptive Batching**: Automatically adjusts batch size and timing based on throughput patterns
- **Dual Submission Modes**: Single-event and batch endpoints for different use cases
- **Webhook Mode**: Configurable routes with template-based payload transformation for external services
- **Authentication**: HMAC signature verification and Bearer token support
- **Multi-Format Payloads**: JSON, XML, YAML, and plain text content types
- **Universal Bootstrap**: Supports any bootstrap provider (PostgreSQL, ScriptFile, Platform, etc.)
- **Graph Data Model**: Native support for nodes and relations with labels and properties
- **Flexible Configuration**: Builder pattern or configuration struct approaches

## Modes

The HTTP source operates in one of two mutually exclusive modes:

| Mode | When Active | Endpoints Available |
|------|-------------|---------------------|
| **Standard Mode** | No `webhooks` config present | `/sources/{id}/events`, `/sources/{id}/events/batch`, `/health` |
| **Webhook Mode** | `webhooks` config present | Custom routes defined in config + `/health` |

> **Note**: When webhook mode is active, standard endpoints return 404. The modes cannot be combined.

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
- **Webhook Integration**: Receive webhooks from GitHub, Shopify, Stripe, or any external service
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

## Webhook Mode

Webhook mode enables the HTTP source to receive arbitrary payloads from external services and transform them into graph events using Handlebars templates.

### Webhook Configuration

```yaml
sources:
  - id: "webhook-source"
    source_type: "http"
    host: "0.0.0.0"
    port: 8080
    webhooks:
      error_behavior: accept_and_log  # Global default
      routes:
        - path: "/github/events"
          methods: ["POST"]
          auth:
            signature:
              type: hmac-sha256
              secret_env: GITHUB_WEBHOOK_SECRET
              header: X-Hub-Signature-256
              prefix: "sha256="
          error_behavior: reject  # Override for this route
          mappings:
            - when:
                header: X-GitHub-Event
                equals: push
              operation: insert
              element_type: node
              effective_from: "{{payload.head_commit.timestamp}}"
              template:
                id: "commit-{{payload.head_commit.id}}"
                labels: ["Commit", "GitHubPush"]
                properties:
                  message: "{{payload.head_commit.message}}"
                  author: "{{payload.head_commit.author.name}}"
                  branch: "{{payload.ref}}"
```

### Webhook Configuration Options

#### Top-Level Webhook Settings

| Name | Description | Values | Default |
|------|-------------|--------|---------|
| `error_behavior` | How to handle mapping errors | `reject`, `accept_and_log`, `accept_silent` | `reject` |
| `cors` | CORS configuration | Object (see below) | None (CORS disabled) |
| `routes` | Array of route configurations | Array | **Required** |

#### CORS Configuration

Enable CORS (Cross-Origin Resource Sharing) for browser-based clients:

```yaml
webhooks:
  cors:
    enabled: true
    allow_origins: ["*"]  # or specific origins
    allow_methods: ["GET", "POST", "PUT", "DELETE", "OPTIONS"]
    allow_headers: ["Content-Type", "Authorization"]
    expose_headers: []
    allow_credentials: false
    max_age: 3600
  routes:
    # ...
```

| Name | Description | Default |
|------|-------------|---------|
| `enabled` | Enable/disable CORS | `true` |
| `allow_origins` | Allowed origins (`["*"]` for any) | `["*"]` |
| `allow_methods` | Allowed HTTP methods | `["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]` |
| `allow_headers` | Allowed request headers | `["Content-Type", "Authorization", "X-Requested-With"]` |
| `expose_headers` | Headers to expose to browser | `[]` |
| `allow_credentials` | Allow cookies/auth headers | `false` |
| `max_age` | Preflight cache duration (seconds) | `3600` |

#### Route Settings

| Name | Description | Required |
|------|-------------|----------|
| `path` | URL path pattern (supports `:param` syntax) | Yes |
| `methods` | HTTP methods to accept | No (defaults to all) |
| `auth` | Authentication configuration | No |
| `error_behavior` | Override global error behavior | No |
| `mappings` | Array of payload-to-event mappings | Yes |

#### Authentication Options

**HMAC Signature Verification** (GitHub, Shopify):

```yaml
auth:
  signature:
    type: hmac-sha256    # or hmac-sha1
    secret_env: SECRET_VAR  # Environment variable name
    header: X-Hub-Signature-256
    prefix: "sha256="    # Optional prefix to strip
    encoding: hex        # hex (default) or base64
```

**Bearer Token**:

```yaml
auth:
  bearer:
    token_env: API_TOKEN  # Environment variable name
```

> **Note**: When both signature and bearer auth are configured, both must pass.

#### Mapping Settings

| Name | Description | Required |
|------|-------------|----------|
| `when` | Condition for this mapping to apply | No |
| `operation` | Graph operation: `insert`, `update`, `delete` | No (can derive from payload) |
| `operation_field` | Path to operation value in payload | No |
| `element_type` | `node` or `relation` | Yes |
| `effective_from` | Handlebars template for timestamp | No |
| `template` | Element template (see below) | Yes |

#### Condition Syntax (`when`)

```yaml
# Match header value
when:
  header: X-GitHub-Event
  equals: push

# Match path parameter
when:
  path_param: resource_type
  equals: orders

# Match query parameter
when:
  query_param: action
  equals: create

# Match payload field
when:
  payload_field: event.type
  equals: order.created
```

#### Template Structure

**Node Template**:
```yaml
template:
  id: "{{payload.id}}"
  labels: ["Order", "{{payload.type}}"]
  properties:
    customer: "{{payload.customer.name}}"
    total: "{{payload.total}}"
    items: "{{payload.line_items}}"  # Preserves arrays/objects
```

**Relation Template**:
```yaml
template:
  id: "rel-{{payload.id}}"
  labels: ["PURCHASED"]
  from: "customer-{{payload.customer_id}}"
  to: "product-{{payload.product_id}}"
  properties:
    quantity: "{{payload.quantity}}"
```

**Spread Entire Payload as Properties**:

To use all fields from the incoming payload as node/relation properties, use a template string instead of an object:

```yaml
template:
  id: "event-{{payload.id}}"
  labels: ["Event"]
  properties: "{{payload}}"  # Spreads all payload fields as properties
```

With this configuration, an incoming payload like:
```json
{
  "id": "123",
  "name": "Test",
  "status": "active",
  "metadata": { "source": "webhook" }
}
```

Creates a node with properties: `id`, `name`, `status`, and `metadata` (as nested object).

**Spread Nested Object**:
```yaml
template:
  id: "{{payload.id}}"
  labels: ["Event"]
  properties: "{{payload.data}}"  # Spread only the 'data' sub-object
```

### Template Context Variables

Templates have access to these variables:

| Variable | Description | Example |
|----------|-------------|---------|
| `payload` | Parsed request body | `{{payload.user.name}}` |
| `headers` | HTTP headers (lowercase keys) | `{{headers.x-request-id}}` |
| `path_params` | Path parameters from route | `{{path_params.id}}` |
| `query_params` | Query string parameters | `{{query_params.filter}}` |
| `method` | HTTP method | `{{method}}` |
| `path` | Request path | `{{path}}` |
| `content_type` | Content-Type header | `{{content_type}}` |

### Template Helpers

| Helper | Description | Example |
|--------|-------------|---------|
| `lowercase` | Convert to lowercase | `{{lowercase payload.name}}` |
| `uppercase` | Convert to uppercase | `{{uppercase payload.status}}` |
| `now` | Current timestamp (ns) | `{{now}}` |
| `concat` | Concatenate strings | `{{concat "prefix-" payload.id}}` |
| `default` | Fallback value | `{{default payload.name "Unknown"}}` |
| `json` | Serialize to JSON string | `{{json payload.metadata}}` |

### Property Value Types

Template values preserve their original types from the payload:

| Payload Type | Result Type |
|--------------|-------------|
| String | `ElementValue::String` |
| Number | `ElementValue::Integer` or `Float` |
| Boolean | `ElementValue::Bool` |
| Array | `ElementValue::List` |
| Object | `ElementValue::Object` |
| Null | `ElementValue::Null` |

To force a value to a JSON string, use the `json` helper:
```yaml
metadata: "{{json payload.complex_object}}"  # Becomes a string
```

### Timestamp Handling (`effective_from`)

The `effective_from` field sets the element's effective timestamp. It auto-detects format:

| Input | Detection | Conversion |
|-------|-----------|------------|
| `1699900000` | Unix seconds | × 1,000,000,000 |
| `1699900000000` | Unix milliseconds | × 1,000,000 |
| `1699900000000000000` | Unix nanoseconds | Used directly |
| `2024-01-15T10:30:00Z` | ISO 8601 | Parsed to nanoseconds |
| `2024-01-15T10:30:00.123Z` | ISO 8601 with ms | Parsed to nanoseconds |

### Error Behavior

| Value | Behavior |
|-------|----------|
| `reject` | Return HTTP 400/500 error |
| `accept_and_log` | Return HTTP 200, log error |
| `accept_silent` | Return HTTP 200, ignore silently |

### Example: GitHub Webhooks

```yaml
webhooks:
  routes:
    - path: "/github/events"
      methods: ["POST"]
      auth:
        signature:
          type: hmac-sha256
          secret_env: GITHUB_WEBHOOK_SECRET
          header: X-Hub-Signature-256
          prefix: "sha256="
      mappings:
        # Push events
        - when:
            header: X-GitHub-Event
            equals: push
          operation: insert
          element_type: node
          effective_from: "{{payload.head_commit.timestamp}}"
          template:
            id: "commit-{{payload.head_commit.id}}"
            labels: ["Commit"]
            properties:
              message: "{{payload.head_commit.message}}"
              author: "{{payload.head_commit.author.name}}"
              repo: "{{payload.repository.full_name}}"
              branch: "{{payload.ref}}"

        # Issue events
        - when:
            header: X-GitHub-Event
            equals: issues
          operation_field: payload.action  # opened, closed, etc.
          element_type: node
          template:
            id: "issue-{{payload.issue.id}}"
            labels: ["Issue"]
            properties:
              title: "{{payload.issue.title}}"
              state: "{{payload.issue.state}}"
              author: "{{payload.issue.user.login}}"
```

### Example: Shopify Webhooks

```yaml
webhooks:
  routes:
    - path: "/shopify/orders"
      methods: ["POST"]
      auth:
        signature:
          type: hmac-sha256
          secret_env: SHOPIFY_WEBHOOK_SECRET
          header: X-Shopify-Hmac-SHA256
          encoding: base64
      mappings:
        - when:
            header: X-Shopify-Topic
            equals: orders/create
          operation: insert
          element_type: node
          template:
            id: "order-{{payload.id}}"
            labels: ["Order"]
            properties:
              order_number: "{{payload.order_number}}"
              total: "{{payload.total_price}}"
              currency: "{{payload.currency}}"
              customer_email: "{{payload.email}}"
              line_items: "{{payload.line_items}}"
```

### Example: Generic REST API

```yaml
webhooks:
  routes:
    - path: "/api/:resource/:id"
      methods: ["POST", "PUT", "DELETE"]
      auth:
        bearer:
          token_env: API_SECRET_TOKEN
      mappings:
        # Create
        - when:
            path_param: resource
            equals: users
          operation_field: method  # Uses HTTP method
          element_type: node
          template:
            id: "user-{{path_params.id}}"
            labels: ["User"]
            properties:
              name: "{{payload.name}}"
              email: "{{payload.email}}"
```

## Input Schema (Standard Mode)

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

#### Webhook Modules

- **auth.rs**: HMAC signature and Bearer token verification
- **content_parser.rs**: JSON, XML, YAML, and text payload parsing
- **template_engine.rs**: Handlebars template processing with custom helpers
- **route_matcher.rs**: Route pattern matching and condition evaluation

### Data Flow

**Standard Mode:**
1. HTTP POST request arrives at Axum endpoint
2. JSON deserialized to `HttpSourceChange`
3. Converted to `drasi_core::models::SourceChange`
4. Sent to adaptive batcher via mpsc channel
5. Batcher accumulates events based on throughput
6. Batch forwarded to dispatchers
7. Dispatchers route to subscribed queries

**Webhook Mode:**
1. HTTP request arrives at configured route
2. Authentication verified (signature/bearer token)
3. Payload parsed based on Content-Type
4. Route matched and conditions evaluated
5. Template engine renders element from payload
6. Converted to `SourceChange` and dispatched
7. Dispatchers route to subscribed queries

### Thread Safety

- All operations are async using Tokio
- Shared state protected by Arc<RwLock<_>>
- Channel-based communication for event flow
- Component status tracked with atomic updates

## See Also

- [Drasi Core Documentation](../../README.md)
- [Source Plugin API](../../../lib/src/sources/traits.rs)
- [Bootstrap Providers](../../../lib/src/bootstrap/)
- [Continuous Queries](../../../query-cypher/)
