# HTTP Reaction

The HTTP reaction sends query results to external HTTP endpoints. It supports customizable HTTP calls for different result types (added, updated, deleted) with template-based URL and body rendering using Handlebars syntax.

## Overview

The HTTP reaction monitors query results and makes HTTP API calls when data changes. It can:
- Send different HTTP requests for add, update, and delete operations
- Customize URLs, methods, headers, and request bodies using Handlebars templates
- Access query result data in templates for dynamic request construction
- Handle authentication via bearer tokens
- Configure timeouts and retries

## Configuration

The HTTP reaction is configured in your Drasi server YAML configuration file:

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
      queries:
        my-query:
          added:
            url: "/entities"
            method: "POST"
            body: '{"id": "{{after.id}}", "name": "{{after.name}}", "type": "{{after.type}}"}'
            headers:
              X-Source: "drasi"
              X-Operation: "create"
          updated:
            url: "/entities/{{after.id}}"
            method: "PUT"
            body: '{"id": "{{after.id}}", "name": "{{after.name}}", "previousName": "{{before.name}}"}'
            headers:
              X-Source: "drasi"
              X-Operation: "update"
          deleted:
            url: "/entities/{{before.id}}"
            method: "DELETE"
            headers:
              X-Source: "drasi"
              X-Operation: "delete"
```

### Configuration Properties

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `base_url` | string | "http://localhost" | Base URL for HTTP requests. Individual call URLs are appended to this |
| `token` | string | null | Optional bearer token for authentication |
| `timeout_ms` | number | 10000 | Request timeout in milliseconds |
| `queries` | object | {} | Query-specific configurations (see below) |

### Query Configuration

Each query can have configurations for three operation types:

| Operation | Description | Available Template Variables |
|-----------|-------------|----------------------------|
| `added` | New rows added to query results | `after` - The new row data |
| `updated` | Existing rows modified | `before` - Previous values<br>`after` - New values<br>`data` - Additional change data |
| `deleted` | Rows removed from results | `before` - The deleted row data |

### Call Specification

Each operation (added/updated/deleted) can specify:

| Field | Type | Description |
|-------|------|-------------|
| `url` | string | URL path or full URL. Supports Handlebars templates |
| `method` | string | HTTP method: GET, POST, PUT, DELETE, PATCH |
| `body` | string | Request body template. If empty, sends raw JSON data |
| `headers` | object | Additional headers. Values support Handlebars templates |

## Template Variables

The HTTP reaction uses Handlebars templating for dynamic content. Available variables depend on the operation type:

### Common Variables
- `{{query_name}}` - Name of the query that produced this result
- `{{operation}}` - Operation type: "ADD", "UPDATE", or "DELETE"

### Operation-Specific Variables

#### Added Operations
- `{{after}}` - The new row data
- `{{after.fieldName}}` - Access specific fields

Example:
```json
{
  "id": "{{after.id}}",
  "name": "{{after.name}}",
  "created": "{{after.timestamp}}"
}
```

#### Updated Operations
- `{{before}}` - Previous row values
- `{{after}}` - New row values
- `{{data}}` - Additional change metadata

Example:
```json
{
  "id": "{{after.id}}",
  "changes": {
    "name": {
      "from": "{{before.name}}",
      "to": "{{after.name}}"
    }
  }
}
```

#### Deleted Operations
- `{{before}}` - The deleted row data

Example:
```json
{
  "id": "{{before.id}}",
  "deletedAt": "{{before.timestamp}}"
}
```

## Data Format

### Query Result Structure

The HTTP reaction receives query results in the following format:

```json
{
  "queryName": "my-query",
  "resultType": "ADD" | "UPDATE" | "DELETE",
  "data": {
    // For ADD: The new row
    // For UPDATE: Object with before/after/data fields
    // For DELETE: The deleted row
  }
}
```

### HTTP Request Format

Based on the configuration, the reaction sends HTTP requests:

**Request:**
```
POST https://api.example.com/entities
Authorization: Bearer your-api-token
Content-Type: application/json
X-Source: drasi
X-Operation: create

{
  "id": "user_123",
  "name": "John Doe",
  "type": "user"
}
```

**Response Handling:**
- 2xx status codes are considered successful
- 4xx/5xx status codes log errors but don't stop the reaction
- Network errors are logged and may trigger retries

## TypeSpec Definition

```tsp
import "@typespec/http";
import "@typespec/rest";
import "@typespec/openapi3";

using TypeSpec.Http;
using TypeSpec.Rest;

namespace DrasiHttpReaction;

// Configuration models
model HttpReactionConfig {
  name: string;
  reaction_type: "http";
  queries: string[];
  auto_start?: boolean = true;
  properties: HttpReactionProperties;
}

model HttpReactionProperties {
  base_url?: string = "http://localhost";
  token?: string;
  timeout_ms?: int32 = 10000;
  queries: Record<QueryConfig>;
}

model QueryConfig {
  added?: CallSpec;
  updated?: CallSpec;
  deleted?: CallSpec;
}

model CallSpec {
  url: string;
  method: "GET" | "POST" | "PUT" | "DELETE" | "PATCH";
  body?: string = "";
  headers?: Record<string>;
}

// Runtime data models
model QueryResult {
  queryName: string;
  resultType: ResultType;
  data: unknown;
}

enum ResultType {
  ADD,
  UPDATE,
  DELETE,
}

// Template context models
model AddContext {
  after: Record<unknown>;
  query_name: string;
  operation: "ADD";
}

model UpdateContext {
  before: Record<unknown>;
  after: Record<unknown>;
  data?: Record<unknown>;
  query_name: string;
  operation: "UPDATE";
}

model DeleteContext {
  before: Record<unknown>;
  query_name: string;
  operation: "DELETE";
}

// HTTP request/response models
@route("/entities")
interface EntityEndpoint {
  @post
  create(@body entity: unknown): {
    @statusCode statusCode: 201;
  } | {
    @statusCode statusCode: 400 | 500;
    @body error: Error;
  };

  @route("/{id}")
  @put
  update(@path id: string, @body entity: unknown): {
    @statusCode statusCode: 200;
  } | {
    @statusCode statusCode: 404 | 400 | 500;
    @body error: Error;
  };

  @route("/{id}")
  @delete
  delete(@path id: string): {
    @statusCode statusCode: 204;
  } | {
    @statusCode statusCode: 404 | 500;
    @body error: Error;
  };
}

model Error {
  message: string;
  code?: string;
}
```

## Example Usage

### Simple CRUD API Integration

```yaml
reactions:
  - name: "user-sync"
    reaction_type: "http"
    queries: ["active-users"]
    properties:
      base_url: "https://api.myapp.com/v1"
      token: "${API_TOKEN}"
      queries:
        active-users:
          added:
            url: "/users"
            method: "POST"
            body: '{"id": "{{after.id}}", "email": "{{after.email}}", "name": "{{after.name}}"}'
          updated:
            url: "/users/{{after.id}}"
            method: "PATCH"
            body: '{"email": "{{after.email}}", "name": "{{after.name}}"}'
          deleted:
            url: "/users/{{before.id}}"
            method: "DELETE"
```

### Webhook Notifications

```yaml
reactions:
  - name: "change-notifications"
    reaction_type: "http"
    queries: ["inventory-changes"]
    properties:
      base_url: "https://webhooks.example.com"
      queries:
        inventory-changes:
          added:
            url: "/notify"
            method: "POST"
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
            url: "/notify"
            method: "POST"
            body: |
              {
                "event": "inventory.item.updated",
                "data": {
                  "sku": "{{after.sku}}",
                  "previous_quantity": {{before.quantity}},
                  "new_quantity": {{after.quantity}},
                  "location": "{{after.location}}"
                }
              }
```

### Custom Headers and Authentication

```yaml
reactions:
  - name: "secure-sync"
    reaction_type: "http"
    queries: ["sensitive-data"]
    properties:
      base_url: "https://secure-api.example.com"
      token: "secret-token"
      queries:
        sensitive-data:
          added:
            url: "/records"
            method: "POST"
            headers:
              X-API-Version: "2.0"
              X-Request-ID: "{{after.id}}-{{operation}}"
              X-Timestamp: "{{after.timestamp}}"
            body: '{"record": {{after}}}'
```

## Best Practices

1. **Use Templates Wisely**: Keep templates simple and readable. Complex logic should be in queries, not templates.

2. **Handle Errors Gracefully**: The reaction logs errors but continues processing. Ensure your endpoints can handle retries.

3. **Secure Sensitive Data**: 
   - Store tokens in environment variables
   - Use HTTPS for production endpoints
   - Don't log sensitive data in templates

4. **Optimize Performance**:
   - Set appropriate timeouts
   - Use batch endpoints when possible
   - Consider rate limits of target APIs

5. **Test Templates**: Test your Handlebars templates with sample data before deployment.

## Troubleshooting

### Common Issues

1. **Template Rendering Errors**
   - Check for typos in variable names
   - Ensure all referenced fields exist in query results
   - Use proper Handlebars syntax for nested objects

2. **Authentication Failures**
   - Verify token is correct
   - Check if token needs "Bearer " prefix
   - Ensure headers are properly formatted

3. **Timeout Errors**
   - Increase `timeout_ms` for slow endpoints
   - Check network connectivity
   - Verify endpoint URL is correct

### Debug Logging

Enable debug logging to see detailed request/response information:

```yaml
server:
  log_level: "debug"
```

This will log:
- Template rendering steps
- Full HTTP requests (method, URL, headers, body)
- Response status codes
- Error details