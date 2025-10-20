# HTTP Reaction

The HTTP reaction sends query results to external HTTP endpoints via webhook calls. It supports customizable HTTP requests for different result types (added, updated, deleted) with template-based URL and body rendering using Handlebars syntax.

## Purpose

The HTTP reaction enables real-time integration with external systems by pushing Drasi query results to HTTP endpoints. Use cases include:

- **Webhook Integration**: Send data changes to webhook endpoints for event-driven workflows
- **API Synchronization**: Keep external systems synchronized with Drasi query results
- **Custom Notifications**: Trigger custom notification systems when query results change
- **ETL Pipelines**: Feed data changes into external ETL or data processing systems
- **Service Integration**: Integrate with third-party services that expose HTTP APIs

### When to Use This Reaction

Use the HTTP reaction when you need to:
- Push query results to REST APIs or webhook endpoints
- Integrate with external systems that don't support gRPC or SSE
- Trigger actions in other systems based on data changes
- Send different payloads for add, update, and delete operations
- Customize HTTP headers, methods, and request bodies per operation type

## Configuration Properties

The HTTP reaction is configured through the `properties` section of a reaction configuration.

### Top-Level Properties

| Property | Type | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `base_url` | string | No | `"http://localhost"` | Base URL for HTTP requests. Individual call URLs are appended to this base unless they are absolute URLs (starting with http:// or https://) |
| `token` | string | No | `null` | Bearer token for authentication. When provided, automatically adds `Authorization: Bearer <token>` header to all requests |
| `timeout_ms` | number | No | `10000` | Request timeout in milliseconds. Requests exceeding this timeout will fail |
| `queries` | object | No | `{}` | Query-specific configurations mapping query names to their CallSpec configurations (see below) |

### CallSpec Structure

Each query in the `queries` object can specify configurations for three operation types: `added`, `updated`, and `deleted`. Each operation uses a `CallSpec` structure:

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `url` | string | Yes | - | URL path (appended to `base_url`) or absolute URL. Supports Handlebars templates for dynamic URLs |
| `method` | string | Yes | - | HTTP method: `GET`, `POST`, `PUT`, `DELETE`, or `PATCH` (case-insensitive) |
| `body` | string | No | `""` | Request body as a Handlebars template. If empty, sends the raw JSON data. Supports full Handlebars syntax |
| `headers` | object | No | `{}` | Additional HTTP headers as key-value pairs. Header values support Handlebars templates |

**Note**: If no configuration is provided for a query, a default configuration is used that POSTs all changes to `/changes/<query_name>` with the raw data.

## Configuration Examples

### YAML Configuration

#### Basic Configuration

```yaml
reactions:
  - id: "my-http-reaction"
    reaction_type: "http"
    queries:
      - "my-query"
    auto_start: true
    properties:
      base_url: "https://api.example.com"
      token: "your-api-token"
      timeout_ms: 10000
```

#### Per-Query Configuration for Different Operations

```yaml
reactions:
  - id: "entity-sync"
    reaction_type: "http"
    queries:
      - "users-query"
    auto_start: true
    properties:
      base_url: "https://api.example.com"
      token: "your-api-token"
      timeout_ms: 10000
      queries:
        users-query:
          added:
            url: "/users"
            method: "POST"
            body: '{"id": "{{after.id}}", "name": "{{after.name}}", "email": "{{after.email}}"}'
            headers:
              X-Source: "drasi"
              X-Operation: "create"
          updated:
            url: "/users/{{after.id}}"
            method: "PUT"
            body: '{"name": "{{after.name}}", "email": "{{after.email}}", "previousName": "{{before.name}}"}'
            headers:
              X-Source: "drasi"
              X-Operation: "update"
          deleted:
            url: "/users/{{before.id}}"
            method: "DELETE"
            headers:
              X-Source: "drasi"
              X-Operation: "delete"
```

#### Handlebars Template Usage in URLs, Bodies, and Headers

```yaml
reactions:
  - id: "webhook-integration"
    reaction_type: "http"
    queries:
      - "inventory-changes"
    properties:
      base_url: "https://webhooks.example.com"
      queries:
        inventory-changes:
          added:
            url: "/notify/{{after.location}}/new-item"
            method: "POST"
            headers:
              X-Event-Type: "inventory.item.created"
              X-Timestamp: "{{after.timestamp}}"
              X-Item-SKU: "{{after.sku}}"
            body: |
              {
                "event": "inventory.item.created",
                "timestamp": "{{after.timestamp}}",
                "data": {
                  "sku": "{{after.sku}}",
                  "quantity": {{after.quantity}},
                  "location": "{{after.location}}"
                }
              }
          updated:
            url: "/notify/{{after.location}}/update-item"
            method: "POST"
            headers:
              X-Event-Type: "inventory.item.updated"
              X-Timestamp: "{{after.timestamp}}"
            body: |
              {
                "event": "inventory.item.updated",
                "query": "{{query_name}}",
                "data": {
                  "sku": "{{after.sku}}",
                  "previous_quantity": {{before.quantity}},
                  "new_quantity": {{after.quantity}},
                  "change": {{after.quantity}} - {{before.quantity}}
                }
              }
```

#### Authentication with Bearer Token

```yaml
reactions:
  - id: "secure-api-sync"
    reaction_type: "http"
    queries:
      - "sensitive-data"
    properties:
      base_url: "https://secure-api.example.com"
      token: "${API_TOKEN}"  # Token from environment variable
      timeout_ms: 15000
      queries:
        sensitive-data:
          added:
            url: "/records"
            method: "POST"
            headers:
              X-API-Version: "2.0"
              X-Request-ID: "{{after.id}}-{{operation}}"
            body: '{{json after}}'
```

### JSON Configuration

Equivalent JSON configuration examples:

#### Basic Configuration

```json
{
  "reactions": [
    {
      "id": "my-http-reaction",
      "reaction_type": "http",
      "queries": ["my-query"],
      "auto_start": true,
      "properties": {
        "base_url": "https://api.example.com",
        "token": "your-api-token",
        "timeout_ms": 10000
      }
    }
  ]
}
```

#### Per-Query Configuration

```json
{
  "reactions": [
    {
      "id": "entity-sync",
      "reaction_type": "http",
      "queries": ["users-query"],
      "auto_start": true,
      "properties": {
        "base_url": "https://api.example.com",
        "token": "your-api-token",
        "timeout_ms": 10000,
        "queries": {
          "users-query": {
            "added": {
              "url": "/users",
              "method": "POST",
              "body": "{\"id\": \"{{after.id}}\", \"name\": \"{{after.name}}\", \"email\": \"{{after.email}}\"}",
              "headers": {
                "X-Source": "drasi",
                "X-Operation": "create"
              }
            },
            "updated": {
              "url": "/users/{{after.id}}",
              "method": "PUT",
              "body": "{\"name\": \"{{after.name}}\", \"email\": \"{{after.email}}\"}",
              "headers": {
                "X-Source": "drasi",
                "X-Operation": "update"
              }
            },
            "deleted": {
              "url": "/users/{{before.id}}",
              "method": "DELETE",
              "headers": {
                "X-Source": "drasi",
                "X-Operation": "delete"
              }
            }
          }
        }
      }
    }
  ]
}
```

## Programmatic Construction in Rust

### Creating an HTTP Reaction Configuration

```rust
use drasi_server_core::{Reaction, Properties};
use serde_json::json;

// Basic HTTP reaction
let reaction = Reaction::http("my-http-reaction")
    .subscribe_to("my-query")
    .with_properties(
        Properties::new()
            .with_string("base_url", "https://api.example.com")
            .with_string("token", "your-api-token")
            .with_int("timeout_ms", 15000)
    )
    .auto_start(true)
    .build();
```

### Setting up CallSpec for Different Operations

```rust
use drasi_server_core::{Reaction, Properties};
use serde_json::json;

// HTTP reaction with per-query, per-operation configuration
let queries_config = json!({
    "users-query": {
        "added": {
            "url": "/users",
            "method": "POST",
            "body": "{\"id\": \"{{after.id}}\", \"name\": \"{{after.name}}\"}",
            "headers": {
                "X-Source": "drasi",
                "X-Operation": "create"
            }
        },
        "updated": {
            "url": "/users/{{after.id}}",
            "method": "PUT",
            "body": "{\"name\": \"{{after.name}}\"}",
            "headers": {
                "X-Source": "drasi",
                "X-Operation": "update"
            }
        },
        "deleted": {
            "url": "/users/{{before.id}}",
            "method": "DELETE",
            "headers": {
                "X-Source": "drasi",
                "X-Operation": "delete"
            }
        }
    }
});

let reaction = Reaction::http("entity-sync")
    .subscribe_to("users-query")
    .with_properties(
        Properties::new()
            .with_string("base_url", "https://api.example.com")
            .with_string("token", "your-api-token")
            .with_value("queries", queries_config)
    )
    .build();
```

### Template Configuration with JSON Helper

```rust
use drasi_server_core::{Reaction, Properties};
use serde_json::json;

let queries_config = json!({
    "inventory-changes": {
        "added": {
            "url": "/webhook",
            "method": "POST",
            "body": "{\"event\": \"item.created\", \"data\": {{json after}}}",
            "headers": {
                "X-Event-Type": "inventory.created"
            }
        }
    }
});

let reaction = Reaction::http("webhook-notifier")
    .subscribe_to("inventory-changes")
    .with_properties(
        Properties::new()
            .with_string("base_url", "https://webhooks.example.com")
            .with_value("queries", queries_config)
    )
    .build();
```

### Starting the Reaction

```rust
use drasi_server_core::DrasiServerCore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build server with HTTP reaction
    let core = DrasiServerCore::builder()
        .with_id("my-server")
        .add_reaction(
            Reaction::http("my-http-reaction")
                .subscribe_to("my-query")
                .with_properties(
                    Properties::new()
                        .with_string("base_url", "https://api.example.com")
                        .with_string("token", "your-api-token")
                )
                .build()
        )
        .build()
        .await?;

    // Start the server (starts all auto_start reactions)
    core.start().await?;

    Ok(())
}
```

## Input Data Format

The HTTP reaction receives query results from the Drasi query engine in the following structure:

### QueryResult Structure

```rust
pub struct QueryResult {
    pub query_id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub results: Vec<serde_json::Value>,
    pub metadata: HashMap<String, serde_json::Value>,
    pub profiling: Option<ProfilingMetadata>,
}
```

Each element in the `results` array has the following structure based on operation type:

### Result Types

#### ADD Operation

When new rows are added to query results:

```json
{
  "type": "ADD",
  "data": {
    "id": "user_123",
    "name": "John Doe",
    "email": "john@example.com",
    "created_at": "2025-10-19T12:00:00Z"
  }
}
```

#### UPDATE Operation

When existing rows in query results are modified:

```json
{
  "type": "UPDATE",
  "data": {
    "id": "user_123",
    "name": "John Smith",
    "email": "john@example.com"
  },
  "before": {
    "id": "user_123",
    "name": "John Doe",
    "email": "john@example.com"
  },
  "after": {
    "id": "user_123",
    "name": "John Smith",
    "email": "john@example.com"
  }
}
```

**Note**: For UPDATE operations, the result contains `data`, `before`, and `after` fields. The `data` and `after` fields contain the same values (the new state), while `before` contains the previous state.

#### DELETE Operation

When rows are removed from query results:

```json
{
  "type": "DELETE",
  "data": {
    "id": "user_123",
    "name": "John Doe",
    "email": "john@example.com"
  }
}
```

## Output Data Format

The HTTP reaction transforms query results into HTTP requests based on the configured CallSpec.

### Template Context Variables

The following variables are available in Handlebars templates:

#### Common Variables (All Operations)

- `{{query_name}}` - String: The name/ID of the query that produced this result
- `{{operation}}` - String: The operation type: `"ADD"`, `"UPDATE"`, or `"DELETE"`

#### ADD Operation Context

- `{{after}}` - Object: The new row data
- `{{after.fieldName}}` - Any: Access specific fields from the new row

#### UPDATE Operation Context

- `{{before}}` - Object: The previous row values
- `{{after}}` - Object: The new row values
- `{{data}}` - Object: Additional change data (same as `after`)
- `{{before.fieldName}}` / `{{after.fieldName}}` - Any: Access specific fields

#### DELETE Operation Context

- `{{before}}` - Object: The deleted row data
- `{{before.fieldName}}` - Any: Access specific fields from the deleted row

### Example HTTP Requests

#### ADD Operation Request

Configuration:
```yaml
added:
  url: "/users"
  method: "POST"
  body: '{"id": "{{after.id}}", "name": "{{after.name}}", "email": "{{after.email}}"}'
  headers:
    X-Source: "drasi"
    X-Operation: "{{operation}}"
```

Generated HTTP Request:
```http
POST https://api.example.com/users
Authorization: Bearer your-api-token
Content-Type: application/json
X-Source: drasi
X-Operation: ADD

{
  "id": "user_123",
  "name": "John Doe",
  "email": "john@example.com"
}
```

#### UPDATE Operation Request

Configuration:
```yaml
updated:
  url: "/users/{{after.id}}"
  method: "PUT"
  body: '{"name": "{{after.name}}", "email": "{{after.email}}", "changed_from": "{{before.name}}"}'
  headers:
    X-Source: "drasi"
    X-Operation: "{{operation}}"
    X-Previous-Name: "{{before.name}}"
```

Generated HTTP Request:
```http
PUT https://api.example.com/users/user_123
Authorization: Bearer your-api-token
Content-Type: application/json
X-Source: drasi
X-Operation: UPDATE
X-Previous-Name: John Doe

{
  "name": "John Smith",
  "email": "john@example.com",
  "changed_from": "John Doe"
}
```

#### DELETE Operation Request

Configuration:
```yaml
deleted:
  url: "/users/{{before.id}}"
  method: "DELETE"
  headers:
    X-Source: "drasi"
    X-Operation: "{{operation}}"
    X-Deleted-User: "{{before.name}}"
```

Generated HTTP Request:
```http
DELETE https://api.example.com/users/user_123
Authorization: Bearer your-api-token
Content-Type: application/json
X-Source: drasi
X-Operation: DELETE
X-Deleted-User: John Doe
```

### Header Format with Authentication

All HTTP requests automatically include:
- `Content-Type: application/json` - Always added
- `Authorization: Bearer <token>` - Added if `token` property is configured
- Custom headers from `headers` field in CallSpec (after template rendering)

Header rendering order:
1. `Content-Type` is set to `application/json`
2. If `token` is configured, `Authorization` header is added
3. Custom headers from CallSpec are added (can override Content-Type if needed)

## Handlebars Templates

The HTTP reaction uses [Handlebars](https://handlebarsjs.com/) templating engine for dynamic content generation in URLs, bodies, and headers.

### Template Syntax and Usage

Handlebars uses double curly braces `{{ }}` for variable interpolation:

```handlebars
{{variable}}           - Simple variable interpolation
{{object.field}}       - Nested object access
{{array.[0]}}          - Array indexing
{{#if condition}}...{{/if}}  - Conditional blocks (standard Handlebars helpers)
```

### Available Context Variables

The context variables available depend on the operation type (see "Template Context Variables" section above).

### Built-in Helpers

#### json Helper

The HTTP reaction registers a custom `json` helper that serializes values as JSON strings:

```handlebars
{{json after}}              - Serializes the entire 'after' object as JSON
{{json after.metadata}}     - Serializes a nested object as JSON
```

**Usage Example**:
```yaml
body: '{"event": "user.created", "data": {{json after}}}'
```

**Rendered Output**:
```json
{"event": "user.created", "data": {"id": "user_123", "name": "John Doe", "email": "john@example.com"}}
```

**Note**: Standard Handlebars helpers (if, each, with, etc.) are also available but use with caution as template logic should be kept simple.

### Example Templates for Common Patterns

#### Simple POST with JSON Body

```yaml
added:
  url: "/webhooks/new-entity"
  method: "POST"
  body: |
    {
      "entity_id": "{{after.id}}",
      "entity_type": "user",
      "data": {{json after}}
    }
```

#### Dynamic URL Construction

```yaml
updated:
  url: "/api/v1/{{after.entity_type}}/{{after.id}}/update"
  method: "PATCH"
  body: '{{json after}}'
```

This generates URLs like: `/api/v1/users/123/update` or `/api/v1/products/456/update`

#### Conditional Headers

While Handlebars supports conditionals, use them sparingly in headers. For complex logic, consider using different CallSpecs or handling in the query itself:

```yaml
added:
  url: "/items"
  method: "POST"
  headers:
    X-Priority: "{{after.priority}}"  # Simple variable interpolation
    X-Category: "{{after.category}}"
  body: '{{json after}}'
```

#### Different Templates per Operation Type

```yaml
queries:
  inventory-items:
    added:
      url: "/inventory/create"
      method: "POST"
      body: |
        {
          "action": "create",
          "item": {{json after}},
          "timestamp": "{{after.created_at}}"
        }
    updated:
      url: "/inventory/update"
      method: "POST"
      body: |
        {
          "action": "update",
          "item_id": "{{after.id}}",
          "changes": {
            "before": {{json before}},
            "after": {{json after}}
          }
        }
    deleted:
      url: "/inventory/archive"
      method: "POST"
      body: |
        {
          "action": "archive",
          "item": {{json before}},
          "archived_at": "{{before.deleted_at}}"
        }
```

## Authentication

### Bearer Token Authentication

The HTTP reaction supports automatic Bearer token authentication:

```yaml
properties:
  token: "your-secret-token"
```

This automatically adds the following header to all requests:
```
Authorization: Bearer your-secret-token
```

### Header-Based Authentication via Templates

For custom authentication schemes, use the `headers` field in CallSpec:

#### API Key Authentication
```yaml
added:
  url: "/api/data"
  method: "POST"
  headers:
    X-API-Key: "{{after.api_key}}"
  body: '{{json after}}'
```

#### Custom Authorization Schemes
```yaml
added:
  url: "/api/data"
  method: "POST"
  headers:
    Authorization: "Custom {{after.auth_token}}"
  body: '{{json after}}'
```

#### Multiple Authentication Headers
```yaml
added:
  url: "/api/data"
  method: "POST"
  headers:
    X-Client-ID: "drasi-client-001"
    X-Client-Secret: "secret-from-config"
    X-Request-Signature: "{{after.signature}}"
  body: '{{json after}}'
```

### Security Considerations

1. **Environment Variables**: Store sensitive tokens in environment variables, not in configuration files:
   ```yaml
   token: "${API_TOKEN}"  # References environment variable
   ```

2. **HTTPS Only**: Always use HTTPS for production endpoints to prevent token interception:
   ```yaml
   base_url: "https://api.example.com"  # Not http://
   ```

3. **Token Rotation**: Implement token rotation mechanisms outside of Drasi configuration

4. **Logging**: Debug logging shows request details but consider security when enabling:
   - Request URLs and headers are logged (including Authorization header)
   - Request bodies are logged at debug level
   - Avoid logging sensitive data in templates

5. **Network Security**: Consider using private networks or VPNs for sensitive integrations

## Troubleshooting

### Common Issues and Solutions

#### 1. Template Syntax Errors

**Symptoms**:
- Requests fail with template rendering errors
- Logs show "Failed to render template" messages

**Solutions**:
- Verify Handlebars syntax is correct (matching `{{ }}` braces)
- Check for typos in variable names
- Ensure JSON is properly escaped in templates
- Test templates with sample data before deployment

**Example Error**:
```
Failed to process result: Failed to render template: variable `afer` not found
```
**Fix**: Correct typo `{{afer.id}}` → `{{after.id}}`

#### 2. Variable Not Found in Context

**Symptoms**:
- Template rendering fails with "variable not found"
- Empty values in rendered templates

**Solutions**:
- Verify the variable exists in the query result data
- Check operation type matches expected context (e.g., `before` only available in UPDATE/DELETE)
- Use correct variable names: `after`, `before`, `data`, `query_name`, `operation`
- Inspect query results to understand available fields

**Example Error**:
```
Failed to render template: variable `user_id` not found
```
**Fix**: Change `{{user_id}}` to `{{after.id}}` if the field is named `id` in query results

#### 3. HTTP Errors (4xx, 5xx)

**Symptoms**:
- Logs show HTTP status codes 400-599
- External system not receiving data correctly

**Common 4xx Errors**:
- **400 Bad Request**: Invalid JSON in body or incorrect Content-Type
  - Verify body template produces valid JSON
  - Check Content-Type header if overridden
- **401 Unauthorized**: Authentication failure
  - Verify token is correct
  - Check authentication scheme (Bearer vs custom)
- **404 Not Found**: Incorrect URL
  - Verify base_url and url path are correct
  - Check URL template renders correctly
- **422 Unprocessable Entity**: Valid JSON but invalid data
  - Verify field names match API expectations
  - Check data types and required fields

**Common 5xx Errors**:
- **500 Internal Server Error**: External system error
  - Check external system logs
  - Verify request payload structure
- **503 Service Unavailable**: External system overloaded or down
  - Implement retry logic if needed
  - Check external system status

**Solutions**:
- Enable debug logging to see full request/response
- Verify endpoint URL and authentication
- Test endpoint manually with curl or Postman
- Check external system API documentation
- Verify request body format matches API expectations

**Debug Command**:
```yaml
# In server configuration
server:
  log_level: "debug"  # Shows full HTTP requests/responses
```

#### 4. Timeout Issues

**Symptoms**:
- Requests fail with timeout errors
- Logs show "request timeout" messages

**Solutions**:
- Increase `timeout_ms` for slow endpoints:
  ```yaml
  timeout_ms: 30000  # 30 seconds
  ```
- Verify network connectivity to endpoint
- Check if endpoint is responding slowly
- Consider if endpoint URL is correct
- Test endpoint response time manually

**Example**:
```yaml
# For slow API endpoints
properties:
  timeout_ms: 60000  # 60 seconds for slow operations
```

#### 5. Authentication Failures

**Symptoms**:
- 401 Unauthorized responses
- 403 Forbidden responses
- Authentication-related error messages

**Solutions**:
- **Bearer Token Issues**:
  - Verify token value is correct
  - Check token hasn't expired
  - Ensure token has required permissions
  - Verify no extra spaces in token value

- **Custom Auth Headers**:
  - Check header name matches API requirements (case-sensitive)
  - Verify header value format
  - Ensure all required auth headers are present

- **Testing**:
  ```bash
  # Test authentication manually
  curl -H "Authorization: Bearer your-token" https://api.example.com/test
  ```

#### 6. Invalid JSON in Body

**Symptoms**:
- 400 Bad Request errors
- Logs show JSON parsing errors from external system

**Solutions**:
- Validate JSON template syntax
- Use `{{json variable}}` helper for complex objects
- Ensure proper escaping of quotes in templates
- Test JSON validity with a JSON validator

**Common Mistakes**:
```yaml
# ❌ Wrong - Missing quotes around string values
body: '{\"name\": {{after.name}}}'

# ✅ Correct - String values in quotes
body: '{\"name\": \"{{after.name}}\"}'

# ✅ Better - Use json helper for objects
body: '{\"user\": {{json after}}}'
```

### Debug Logging

Enable debug logging to diagnose issues:

```yaml
# In DrasiServerCore configuration or environment
server:
  log_level: "debug"
```

Or set environment variable:
```bash
RUST_LOG=debug cargo run
```

**Debug logs include**:
- Template rendering with context data
- Full HTTP request details (method, URL, headers, body)
- HTTP response status codes
- Error details and stack traces
- Template variable resolution

**Example Debug Output**:
```
[my-http-reaction] Rendering template: {"id": "{{after.id}}"} with context: {"after": {"id": "123", "name": "Test"}, "query_name": "users", "operation": "ADD"}
[my-http-reaction] Rendered body: {"id": "123"}
[my-http-reaction] Sending POST request to https://api.example.com/users with body: {"id": "123"}
[my-http-reaction] HTTP POST https://api.example.com/users - Status: 201
```

### Testing Tips

1. **Start Simple**: Test with minimal templates first, then add complexity
2. **Validate Templates**: Test Handlebars templates separately before deployment
3. **Mock Endpoints**: Use webhook testing tools (webhook.site, requestbin.com) to inspect requests
4. **Check Query Results**: Verify query produces expected data structure
5. **Incremental Testing**: Test one operation type at a time (added, then updated, then deleted)

## Limitations

### HTTP Version Support
- Uses HTTP/1.1 protocol via the Rust `reqwest` library
- HTTP/2 is supported automatically if the server negotiates it
- HTTP/3 (QUIC) is not currently supported

### Connection Pooling Behavior
- The HTTP reaction uses a single `reqwest::Client` instance per reaction
- Connection pooling is handled automatically by reqwest
- Persistent connections are reused when possible
- Default connection pool settings from reqwest apply (no custom tuning)
- Connection pools are not shared between different reaction instances

### Request Size Limits
- No explicit request size limit imposed by the HTTP reaction
- Practical limits depend on:
  - Target server's maximum request size
  - Network MTU and fragmentation
  - Memory available for buffering
  - Timeout settings (large requests may timeout)
- Recommended: Keep request bodies under 10MB for reliable delivery
- For large payloads, consider:
  - Pagination in queries
  - Compression (if supported by target API)
  - Alternative reactions (gRPC with streaming)

### Performance Considerations

#### Throughput
- Each result is processed sequentially within a query's results
- Multiple results from the same query are processed in order
- Results from different queries can be processed concurrently
- Network latency and target API response time directly impact throughput

#### Blocking Behavior
- HTTP requests are async (non-blocking) using Tokio runtime
- Failed requests don't block subsequent requests
- Timeouts prevent indefinite blocking

#### Scalability
- One reaction instance per configured reaction
- No built-in retry mechanism for failed requests
- No request queuing or buffering beyond the query result channel
- For high-volume scenarios, consider:
  - Multiple reaction instances with load balancing
  - Batch-capable endpoints on the target system
  - Intermediate message queue (external to Drasi)

#### Resource Usage
- Memory: One HTTP client instance per reaction (minimal overhead)
- CPU: Template rendering and JSON serialization per result
- Network: One connection pool per reaction instance
- No disk I/O (unless target system requires file uploads)

#### Best Practices for Performance
1. **Optimize Templates**: Keep templates simple to minimize rendering overhead
2. **Appropriate Timeouts**: Set timeouts based on expected API response times
3. **Monitor Target APIs**: Ensure target systems can handle request volume
4. **Query Optimization**: Use selective queries to reduce result volume
5. **Error Handling**: Implement proper error handling in target systems for duplicate/retry scenarios

### Other Limitations
- **No Request Retry Logic**: Failed requests are logged but not automatically retried
- **No Response Validation**: Response body is not parsed or validated (only status code checked)
- **No Request Batching**: Each query result generates a separate HTTP request
- **No Circuit Breaking**: No automatic circuit breaker for failing endpoints
- **Synchronous Processing**: Results are processed one at a time per query (within the same result set)

### Workarounds for Limitations

If you need capabilities beyond these limitations:

1. **Retries**: Implement retry logic in the target HTTP endpoint
2. **Batching**: Use intermediate aggregation in Drasi queries or external message queue
3. **High Volume**: Deploy multiple Drasi instances with different reaction endpoints
4. **Reliability**: Use a message queue between Drasi and target systems
5. **Circuit Breaking**: Implement at the target system or use an API gateway
