# HTTP Source

The HTTP source provides a REST API endpoint for receiving graph data changes via HTTP requests. It supports both single events and batch submissions with optional adaptive batching for optimal throughput.

## Configuration Schema

### Basic Configuration

```yaml
sources:
  - id: "my-http-source"
    source_type: "http"
    auto_start: true
    properties:
      host: "0.0.0.0"        # HTTP server host (default: localhost)
      port: 8080             # HTTP server port (default: 8080)
      endpoint: "/events"    # Optional custom endpoint path
      timeout_ms: 30000      # Request timeout in milliseconds (default: 10000)
```

### With Adaptive Batching

Adaptive batching automatically adjusts batch size and timing based on throughput for optimal performance:

```yaml
sources:
  - id: "high-throughput-source"
    source_type: "http"
    properties:
      host: "0.0.0.0"
      port: 9000

      # Adaptive batching configuration
      adaptive_enabled: true              # Enable adaptive batching (default: true)
      adaptive_max_batch_size: 1000       # Maximum batch size (default: 1000)
      adaptive_min_batch_size: 10         # Minimum batch size (default: 10)
      adaptive_max_wait_ms: 100           # Maximum wait time ms (default: 100)
      adaptive_min_wait_ms: 1             # Minimum wait time ms (default: 1)
      adaptive_window_secs: 5             # Throughput measurement window (default: 5)
```

### With Bootstrap Provider

HTTP source supports **any** bootstrap provider through the universal SourceBase pattern. This enables powerful use cases like "bootstrap from database, stream changes via HTTP".

#### PostgreSQL Bootstrap Example

Initial data load from PostgreSQL, then receive updates via HTTP:

```yaml
sources:
  - id: "http-with-postgres-bootstrap"
    source_type: "http"
    bootstrap_provider:
      type: postgres
    properties:
      # HTTP source configuration
      host: "localhost"
      port: 9000

      # PostgreSQL bootstrap provider configuration
      # These properties are used by the bootstrap provider, not the HTTP source
      database: "mydb"
      user: "dbuser"
      password: "dbpass"
      tables: ["inventory", "orders"]
      table_keys:
        - table: inventory
          key_columns: ["id"]
        - table: orders
          key_columns: ["order_id"]
```

#### ScriptFile Bootstrap Example

Initial data load from JSONL files, then receive updates via HTTP:

```yaml
sources:
  - id: "http-with-file-bootstrap"
    source_type: "http"
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/data/initial_nodes.jsonl"
        - "/data/initial_relations.jsonl"
    properties:
      host: "localhost"
      port: 9000
```

#### Other Bootstrap Providers

- **Platform**: Bootstrap from remote Drasi Query API via HTTP
- **Application**: Replay previously stored insert events
- **Noop**: No bootstrap, streaming only

### Configuration Reference

#### HTTP Source Configuration

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `host` | string | Yes | - | HTTP server host address |
| `port` | u16 | Yes | - | HTTP server port |
| `endpoint` | string | No | None | Custom endpoint path |
| `timeout_ms` | u64 | No | 10000 | Request timeout in milliseconds |
| `adaptive_enabled` | bool | No | None | Enable adaptive batching |
| `adaptive_max_batch_size` | usize | No | None | Maximum batch size |
| `adaptive_min_batch_size` | usize | No | None | Minimum batch size |
| `adaptive_max_wait_ms` | u64 | No | None | Maximum wait time in ms |
| `adaptive_min_wait_ms` | u64 | No | None | Minimum wait time in ms |
| `adaptive_window_secs` | u64 | No | None | Throughput measurement window in seconds |

#### Bootstrap Provider Configuration

These properties are used by bootstrap providers when configured. They remain in the generic properties map for bootstrap providers to access via `BootstrapContext`.

**PostgreSQL Bootstrap Provider**:
- `database` (string): PostgreSQL database name
- `user` (string): PostgreSQL user
- `password` (string): PostgreSQL password
- `tables` (array): Tables to bootstrap
- `table_keys` (array): Primary key configuration per table

**ScriptFile Bootstrap Provider**:
- `file_paths` (array): Paths to JSONL files for bootstrap

**Platform Bootstrap Provider**:
- `query_api_url` (string): Remote Drasi Query API URL

## Pushing Data to HTTP Source

### API Endpoints

When an HTTP source is running, it exposes the following endpoints:

- `POST /sources/{source_id}/events` - Submit a single event
- `POST /sources/{source_id}/events/batch` - Submit multiple events
- `GET /health` - Health check

### Event Format

The HTTP source uses an event format that directly maps to internal Drasi Core structures for efficient conversion.

#### Insert Node

```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "insert",
    "element": {
      "type": "node",
      "id": "user_123",
      "labels": ["User", "Customer"],
      "properties": {
        "name": "John Doe",
        "email": "john@example.com",
        "age": 30
      }
    },
    "timestamp": 1234567890000
  }'
```

#### Insert Relation

```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "insert",
    "element": {
      "type": "relation",
      "id": "rel_123",
      "labels": ["PURCHASED"],
      "from": "user_123",
      "to": "product_456",
      "properties": {
        "quantity": 2,
        "price": 29.99,
        "date": "2025-01-15"
      }
    }
  }'
```

#### Update Operations

```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "update",
    "element": {
      "type": "node",
      "id": "user_123",
      "labels": ["User", "Premium"],
      "properties": {
        "name": "John Doe",
        "email": "john@example.com",
        "age": 31,
        "membership": "premium"
      }
    }
  }'
```

#### Delete Operations

```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "delete",
    "id": "user_123",
    "labels": ["User"],
    "timestamp": 1234567890000
  }'
```

#### Batch Operations

Submit multiple events in a single request:

```bash
curl -X POST http://localhost:8080/sources/my-http-source/events/batch \
  -H "Content-Type: application/json" \
  -d '{
    "events": [
      {
        "operation": "insert",
        "element": {
          "type": "node",
          "id": "user_1",
          "labels": ["User"],
          "properties": {"name": "Alice"}
        }
      },
      {
        "operation": "insert",
        "element": {
          "type": "node",
          "id": "user_2",
          "labels": ["User"],
          "properties": {"name": "Bob"}
        }
      }
    ]
  }'
```

### Response Format

**Success Response:**
```json
{
  "success": true,
  "message": "All 2 events processed successfully",
  "error": null
}
```

**Error Response:**
```json
{
  "success": false,
  "message": "All 1 events failed",
  "error": "Invalid element type"
}
```

**Health Check Response:**
```json
{
  "status": "healthy",
  "service": "http-source",
  "features": ["adaptive-batching", "batch-endpoint"]
}
```

## Integration with Continuous Queries

Events submitted to the HTTP source are processed by continuous queries. The event format maps to Cypher graph elements:

- **Nodes**: Match Cypher `(label {properties})` patterns
- **Relations**: Match Cypher `-[label {properties}]->` patterns
- **Labels**: Used for query filtering and pattern matching
- **Properties**: Accessible in Cypher queries via dot notation

Example query consuming HTTP source events:

```cypher
MATCH (u:User)
WHERE u.age > 25
RETURN u.name, u.email
```

## Bootstrap Providers

### Overview

HTTP source uses the **universal bootstrap architecture** - any bootstrap provider can be used with any source. Bootstrap runs asynchronously while streaming continues.

### PostgreSQL Bootstrap

**Use case**: Load 1M records from existing database, then receive incremental updates via HTTP.

**Configuration**: See "With Bootstrap Provider" section above.

**How it works**: PostgreSQL bootstrap provider extracts database connection details from the source properties map, performs a snapshot read of configured tables, and sends initial data through the bootstrap channel.

### ScriptFile Bootstrap

**Use case**: Load test data from JSONL files for development/testing, then receive updates via HTTP.

**Configuration**: See "With Bootstrap Provider" section above.

**How it works**: Reads JSONL files containing nodes and relations, sends them through the bootstrap channel before streaming begins.

### Platform Bootstrap

**Use case**: Bootstrap from a remote Drasi Query API, then receive updates via HTTP.

### Application Bootstrap

**Use case**: Replay previously stored insert events, then receive new updates via HTTP.

## Performance Considerations

### Adaptive Batching

**When to use**:
- High throughput scenarios (>100 msgs/sec)
- Variable traffic patterns
- Need to balance latency and throughput

**How to tune**:
- `adaptive_max_batch_size`: Increase for higher throughput scenarios
- `adaptive_max_wait_ms`: Decrease for lower latency requirements
- `adaptive_window_secs`: Adjust based on how quickly traffic patterns change

**Disable for**:
- Real-time applications requiring minimal latency
- Low traffic scenarios (<10 msgs/sec)

### Single Event vs Batch Endpoint

**Use `/events` (single) when**:
- Sending individual updates as they occur
- Simplicity is preferred
- Traffic is sporadic

**Use `/events/batch` when**:
- You have multiple events to send at once
- Reduce HTTP request overhead
- Maximize throughput

### Bootstrap Performance

Bootstrap runs asynchronously in a separate task, allowing queries to start processing streaming events immediately while bootstrap data loads in the background.

## Error Handling

### Common Errors

**Port Already in Use**:
```
Failed to bind HTTP server to 0.0.0.0:8080: Address already in use
```
**Solution**: Change the port in configuration or stop the conflicting service.

**Invalid JSON**:
```json
{
  "success": false,
  "message": "Failed to parse JSON",
  "error": "..."
}
```
**Solution**: Validate JSON structure matches the event format.

**Source Name Mismatch**:
```json
{
  "success": false,
  "message": "Source name mismatch",
  "error": "Expected 'my-source', got 'wrong-source'"
}
```
**Solution**: Ensure URL path matches the source ID in configuration.

## Testing

### Running Tests

```bash
# Run all HTTP source tests
cargo test -p drasi-server-core sources::http

# Run specific test
cargo test -p drasi-server-core test_http_source_start_stop
```

### Testing with curl

Start a test HTTP source and submit events:

```bash
# Health check
curl http://localhost:8080/health

# Submit test event
curl -X POST http://localhost:8080/sources/test-source/events \
  -H "Content-Type: application/json" \
  -d '{"operation":"insert","element":{"type":"node","id":"1","labels":["Test"],"properties":{}}}'
```

## Migration from Previous Versions

See [MIGRATION.md](MIGRATION.md) for detailed migration instructions when upgrading from versions with different configuration schemas.
