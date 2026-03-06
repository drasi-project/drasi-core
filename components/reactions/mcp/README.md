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
    query1:
      title: "Example Query"
      description: "Example query resource"
      added:
        template: '{"type":"added","data":{{json after}}}'
      updated:
        template: '{"type":"updated","before":{{json before}},"after":{{json after}}}'
      deleted:
        template: '{"type":"deleted","data":{{json before}}}'
  properties:
    port: 3000
    bearerToken: "${MCP_AUTH_TOKEN}"
```

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `port` | HTTP port for MCP server | u16 | `3000` |
| `bearerToken` | Optional bearer token (ConfigValue) | Option\<String\> | None |
| `routes` | Query-specific template configurations | HashMap\<String, QueryConfig\> | `{}` |

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

Run the integration test (ignored by default):

```bash
cargo test -p drasi-reaction-mcp -- --ignored --nocapture
```

## Limitations

- Bearer token only (no OAuth or mTLS)
- In-memory subscriptions (not persisted across restarts)
- SSE transport only (no WebSocket)
