# HTTP Reaction

The HTTP reaction sends query results to external HTTP endpoints via webhook calls with customizable HTTP requests for different result types (added, updated, deleted) using Handlebars templating.

## Overview

Enables real-time integration with external systems by pushing Drasi query results to HTTP endpoints for webhook integration, API synchronization, custom notifications, ETL pipelines, and third-party service integration.

## Configuration Properties

### Standard Reaction Properties

| Property | Type | Required | Description |
|----------|------|----------|-------------|
| `id` | string | Yes | Unique identifier for the reaction |
| `reaction_type` | string | Yes | Must be `"http"` |
| `queries` | array | Yes | List of query IDs to subscribe to |
| `auto_start` | boolean | No | Whether to start automatically (default: `false`) |

### HTTP-Specific Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `base_url` | string | `"http://localhost"` | Base URL for HTTP requests. Call URLs are appended unless absolute |
| `token` | string | - | Bearer token for authentication (adds `Authorization: Bearer <token>` header) |
| `timeout_ms` | number | `10000` | Request timeout in milliseconds |
| `routes` | object | `{}` | Query-specific CallSpec configurations (see below) |

### CallSpec Structure

Each query can specify `added`, `updated`, and `deleted` operations with:

| Field | Type | Description |
|-------|------|-------------|
| `url` | string | URL path or absolute URL (supports Handlebars templates) |
| `method` | string | HTTP method: `GET`, `POST`, `PUT`, `DELETE`, or `PATCH` |
| `body` | string | Request body template (empty = raw JSON data) |
| `headers` | object | Additional HTTP headers (values support templates) |

**Default behavior**: If unconfigured, POSTs all changes to `/changes/<query_name>` with raw data.

## Configuration Examples

### Basic YAML Configuration

```yaml
reactions:
  - id: "my-http-reaction"
    reaction_type: "http"
    queries: ["my-query"]
    auto_start: true
    base_url: "https://api.example.com"
    token: "your-api-token"
    timeout_ms: 10000
```

### Per-Query Configuration with Templates

```yaml
reactions:
  - id: "entity-sync"
    reaction_type: "http"
    queries: ["users-query"]
    base_url: "https://api.example.com"
    token: "your-api-token"
    routes:
      users-query:
        added:
          url: "/users"
          method: "POST"
          body: '{"id": "{{after.id}}", "name": "{{after.name}}"}'
          headers:
            X-Source: "drasi"
        updated:
          url: "/users/{{after.id}}"
          method: "PUT"
          body: '{"name": "{{after.name}}", "previousName": "{{before.name}}"}'
        deleted:
          url: "/users/{{before.id}}"
          method: "DELETE"
```

## Programmatic API (Rust)

```rust
use drasi_server_core::config::{ReactionConfig, ReactionSpecificConfig};
use drasi_server_core::config::typed::HttpReactionConfig;
use drasi_server_core::reactions::http::{CallSpec, QueryConfig};
use std::collections::HashMap;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut routes = HashMap::new();
    routes.insert(
        "users-query".to_string(),
        QueryConfig {
            added: Some(CallSpec {
                url: "/users".to_string(),
                method: "POST".to_string(),
                body: r#"{"id": "{{after.id}}", "name": "{{after.name}}"}"#.to_string(),
                headers: {
                    let mut h = HashMap::new();
                    h.insert("X-Source".to_string(), "drasi".to_string());
                    h
                },
            }),
            updated: None,
            deleted: None,
        },
    );

    let config = ReactionConfig {
        id: "my-http-reaction".to_string(),
        queries: vec!["users-query".to_string()],
        auto_start: true,
        config: ReactionSpecificConfig::Http(HttpReactionConfig {
            base_url: "https://api.example.com".to_string(),
            token: Some("your-api-token".to_string()),
            timeout_ms: 10000,
            routes,
        }),
        priority_queue_capacity: None,
    };

    // Use config to create and add reaction to DrasiServerCore
    Ok(())
}
```

## Data Format

### Result Types

Results are structured based on operation type:

**ADD** - New rows:
```json
{"type": "ADD", "data": {"id": "user_123", "name": "John Doe"}}
```

**UPDATE** - Modified rows (includes `before` and `after` states):
```json
{
  "type": "UPDATE",
  "before": {"id": "user_123", "name": "John Doe"},
  "after": {"id": "user_123", "name": "John Smith"},
  "data": {"id": "user_123", "name": "John Smith"}
}
```

**DELETE** - Removed rows:
```json
{"type": "DELETE", "data": {"id": "user_123", "name": "John Doe"}}
```

## Template Context Variables

Available in Handlebars templates for all operations:
- `{{query_name}}` - Query name/ID
- `{{operation}}` - Operation type: `"ADD"`, `"UPDATE"`, or `"DELETE"`

**ADD**: `{{after}}` - New row data, `{{after.fieldName}}` - Access specific fields

**UPDATE**: `{{before}}` / `{{after}}` / `{{data}}` - Previous/new/current values. Note: `{{data}}` is equivalent to `{{after}}` for UPDATE operations. Use `{{before.fieldName}}` / `{{after.fieldName}}` to access specific fields

**DELETE**: `{{before}}` - Deleted row data, `{{before.fieldName}}` - Access fields

### Example Request

Configuration:
```yaml
added:
  url: "/users"
  method: "POST"
  body: '{"id": "{{after.id}}", "name": "{{after.name}}"}'
  headers:
    X-Source: "drasi"
```

Generated:
```http
POST https://api.example.com/users
Authorization: Bearer your-api-token
Content-Type: application/json
X-Source: drasi

{"id": "user_123", "name": "John Doe"}
```

Headers: `Content-Type: application/json` (always), `Authorization: Bearer <token>` (if configured), plus custom headers.

## Handlebars Templating

Uses [Handlebars](https://handlebarsjs.com/) syntax with `{{variable}}`, `{{object.field}}`, `{{array.[0]}}` for dynamic content.

### json Helper

Serializes values as JSON strings:

```yaml
body: '{"event": "user.created", "data": {{json after}}}'
# Renders: {"event": "user.created", "data": {"id": "user_123", "name": "John Doe"}}
```

### Common Patterns

**Dynamic URLs**:
```yaml
url: "/api/v1/{{after.entity_type}}/{{after.id}}/update"
```

**Complex Bodies**:
```yaml
body: |
  {
    "action": "update",
    "item_id": "{{after.id}}",
    "changes": {
      "before": {{json before}},
      "after": {{json after}}
    }
  }
```

## Authentication

**Bearer Token** (automatic):
```yaml
reactions:
  - id: "my-http-reaction"
    reaction_type: "http"
    token: "your-secret-token"  # Adds: Authorization: Bearer your-secret-token
```

**Custom Authentication** (via headers):
```yaml
headers:
  X-API-Key: "{{after.api_key}}"
  # or
  Authorization: "Custom {{after.auth_token}}"
```

**Security**: Use HTTPS, store tokens in environment variables (`token: "${API_TOKEN}"`), avoid logging sensitive data.

## Troubleshooting

**Template Syntax Errors**: Check Handlebars syntax (`{{ }}` braces), variable names, JSON escaping. Example: `{{afer.id}}` → `{{after.id}}`

**Variable Not Found**: Verify variable exists in query results. Use `after`/`before`/`data`/`query_name`/`operation`. Check operation type context.

**HTTP Errors**:
- **400**: Invalid JSON body - use `{{json variable}}` helper
- **401/403**: Check token value, expiration, permissions
- **404**: Verify base_url and URL template
- **500/503**: Check external system logs and status

**Timeouts**: Increase `timeout_ms` (default: 10000ms), verify network connectivity, test endpoint manually.

**Debug Logging**: Set `RUST_LOG=debug` to see template rendering, HTTP requests/responses, and error details.

**Testing Tips**: Start with minimal templates, use webhook testing tools (webhook.site), verify query output structure, test operation types incrementally.

## Limitations

- **HTTP Versions**: HTTP/1.1 (default), HTTP/2 (if negotiated), no HTTP/3
- **Connection Pooling**: One `reqwest::Client` per reaction, automatic pooling, no sharing between reactions
- **Request Size**: No explicit limit; keep under 10MB for reliability
- **Retry Logic**: No automatic retries - implement in target system
- **Response Validation**: Only status code checked, body not parsed
- **Batching**: One HTTP request per result (no batching)
- **Circuit Breaking**: Not supported - use API gateway if needed
- **Processing**: Sequential per query, concurrent across queries

**Performance**: Async/non-blocking requests, throughput limited by network latency and target API response times. For high volume, use multiple reaction instances or intermediate message queues.
