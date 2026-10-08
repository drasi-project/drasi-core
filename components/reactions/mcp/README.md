# MCP Reaction

Model Context Protocol (MCP) reaction plugin for Drasi that exposes continuous query results as MCP resources over HTTP + SSE.

## Overview

The MCP Reaction hosts an MCP server that allows clients to:

- **List resources** for configured queries
- **Read current results** for a query resource
- **Subscribe** to query resources and receive real-time notifications
- **Authenticate** requests using an optional bearer token

This enables MCP-compatible clients (AI assistants, IDEs, agents) to consume live Drasi query updates over the standard MCP protocol.

## Configuration

### Builder Pattern (Recommended)

```rust
use drasi_reaction_mcp::{McpReaction, QueryConfig, NotificationTemplate};

let reaction = McpReaction::builder("my-mcp-reaction")
    .with_port(3000)
    .with_bearer_token("secret-token")
    .with_queries(vec!["query1".to_string()])
    .with_route(
        "query1",
        QueryConfig {
            title: Some("Example Query".to_string()),
            description: Some("Example query resource".to_string()),
            added: Some(NotificationTemplate {
                template: r#"{"type":"added","data":{{json after}}}"#.to_string(),
            }),
            updated: Some(NotificationTemplate {
                template: r#"{"type":"updated","before":{{json before}},"after":{{json after}}}"#
                    .to_string(),
            }),
            deleted: Some(NotificationTemplate {
                template: r#"{"type":"deleted","data":{{json before}}}"#.to_string(),
            }),
        },
    )
    .build()?;
```

### YAML Configuration (Plugin Config)

```yaml
kind: Reaction
apiVersion: v1
name: my-mcp-reaction
spec:
  kind: MCP
  queries:
    query1: {}
  properties:
    port: 3000
    host: "0.0.0.0"
    bearerToken: "${MCP_AUTH_TOKEN}"
    maxSessions: 100
    sessionChannelCapacity: 1024
    routes:
      query1:
        title: "Example Query"
        description: "Example query resource"
        added:
          template: '{"type":"added","data":{{json after}}}'
        updated:
          template: '{"type":"updated","before":{{json before}},"after":{{json after}}}'
        deleted:
          template: '{"type":"deleted","data":{{json before}}}'
```

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `port` | HTTP port for MCP server | u16 | `3000` |
| `host` | Bind address for HTTP server | String | `0.0.0.0` |
| `bearerToken` | Optional bearer token (ConfigValue) | Option\<String\> | None |
| `maxSessions` | Maximum concurrent SSE sessions | usize | `100` |
| `sessionChannelCapacity` | Channel buffer size per session | usize | `1024` |
| `routes` | Query-specific template configurations (keyed by query ID) | HashMap\<String, QueryConfig\> | `{}` |

Per-query template config (`routes.<queryId>`):

| Option | Description | Type |
|--------|-------------|------|
| `title` | MCP resource title | String |
| `description` | MCP resource description | String |
| `added.template` | Handlebars template for ADD operations | String |
| `updated.template` | Handlebars template for UPDATE operations | String |
| `deleted.template` | Handlebars template for DELETE operations | String |

### Template Variables

| Variable | Description | Available Operations |
|----------|-------------|---------------------|
| `after` | New/current state | ADD, UPDATE |
| `before` | Previous state | UPDATE, DELETE |
| `data` | Diff payload | UPDATE |
| `queryId` | Query ID | ALL |

Use `{{json ...}}` to serialize complex objects safely.

## MCP Endpoints

- **POST /** — JSON-RPC requests (initialize, resources/list, subscribe, etc.)
- **GET /** — SSE stream for notifications (requires `mcp-session-id` header)

### Authentication

If `bearerToken` is configured, every request must include:

```
Authorization: Bearer <token>
```

## Expected Notifications

Notifications use MCP JSON-RPC format:

```json
{
  "jsonrpc": "2.0",
  "method": "notifications/resources/updated",
  "params": {
    "uri": "drasi://query/query1",
    "operation": "added",
    "data": { "type": "added", "data": { ... } }
  }
}
```

## Integration Test

Run the ordinary integration and lifecycle tests:

```bash
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3 cargo test -p drasi-reaction-mcp --lib --tests
```

## Lifecycle

`start()` binds the listener before reporting `Running`; the bound-port handle
is populated before start returns and cleared after successful stop. Bind
failures retain their I/O cause and do not start workers.

Await `stop()` before restarting. Server and processing workers are owned before
spawning. Stop closes live streams, drains HTTP requests, clears sessions and
subscription indexes, and joins processing before publishing `Stopped`.
Cancellation or a five-second server-drain timeout retains ownership; retry
`stop()` to complete cleanup. The server is not aborted while it owns unfinished
requests, and even cancelled session cleanup after worker exit blocks restart.

Disconnect closes the session channel synchronously and wakes the existing
owned processing worker to remove closed sessions. It does not spawn cleanup
tasks or add another worker. Closed sessions cannot recreate subscriptions while
cleanup is pending. Current query results retain their existing in-memory
behavior; these changes do not add durable notifications or client handling
acknowledgements. Full session queues still disconnect, and template failures
still log and skip.

## Limitations

- Bearer token only (no OAuth or mTLS)
- In-memory subscriptions (not persisted across restarts)
- SSE transport only (no WebSocket)
