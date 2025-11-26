# HTTP Source

The HTTP source provides a REST API endpoint for receiving graph data changes via HTTP requests. It supports both single events and batch submissions with optional adaptive batching for optimal throughput.

## Configuration

### Configuration Settings

The HTTP Source supports the following configuration settings:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `id` | String | Unique identifier for the source | Any string | **(Required)** |
| `source_type` | String | Source type discriminator | "http" | **(Required)** |
| `auto_start` | Boolean | Whether to automatically start this source | true, false | `true` |
| `host` | String | HTTP server host address to bind to | Any valid hostname or IP | **(Required)** |
| `port` | Integer (u16) | HTTP server port number | 1 - 65535 | **(Required)** |
| `endpoint` | String (Optional) | Optional custom endpoint path | Any valid path string | `None` |
| `timeout_ms` | Integer (u64) | Request timeout in milliseconds | Any positive integer | `10000` |
| `adaptive_enabled` | Boolean (Optional) | Enable adaptive batching | `true`, `false` | `None` (uses default) |
| `adaptive_max_batch_size` | Integer (usize, Optional) | Maximum batch size for adaptive batching | Any positive integer | `None` (uses default: 1000) |
| `adaptive_min_batch_size` | Integer (usize, Optional) | Minimum batch size for adaptive batching | Any positive integer | `None` (uses default: 10) |
| `adaptive_max_wait_ms` | Integer (u64, Optional) | Maximum wait time in milliseconds | Any positive integer | `None` (uses default: 100) |
| `adaptive_min_wait_ms` | Integer (u64, Optional) | Minimum wait time in milliseconds | Any positive integer | `None` (uses default: 1) |
| `adaptive_window_secs` | Integer (u64, Optional) | Throughput measurement window in seconds | Any positive integer | `None` (uses default: 5) |
| `dispatch_mode` | String (Optional) | Event dispatch mode: "channel" (isolated channels per subscriber with backpressure, zero message loss) or "broadcast" (shared channel, no backpressure, possible message loss) | "channel", "broadcast" | `"channel"` |
| `dispatch_buffer_capacity` | Integer (Optional) | Buffer size for dispatch channel | Any positive integer | `1000` |
| `bootstrap_provider` | Object (Optional) | Bootstrap provider configuration | See Bootstrap Providers section | `None` |

**Note**: The HTTP Source does not use any common configuration types from `config/common.rs` (such as SslMode or TableKeyConfig).

### Bootstrap Provider Configuration

When using bootstrap providers with the HTTP source, additional properties may be required depending on the provider type. These properties are used by bootstrap providers and remain in the generic properties map for bootstrap providers to access via `BootstrapContext`.

#### PostgreSQL Bootstrap Provider Properties

When using `bootstrap_provider.type: postgres`:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `database` | String | PostgreSQL database name | Any valid database name | **(Required by provider)** |
| `user` | String | PostgreSQL user | Any valid username | **(Required by provider)** |
| `password` | String | PostgreSQL password | Any string | **(Required by provider)** |
| `host` | String | PostgreSQL host | Any valid hostname or IP | `"localhost"` |
| `port` | Integer | PostgreSQL port | 1 - 65535 | `5432` |
| `tables` | Array[String] | Tables to bootstrap | Array of table names | `[]` |
| `table_keys` | Array[TableKeyConfig] | Primary key configuration per table | Array of TableKeyConfig objects | `[]` |
| `ssl_mode` | SslMode (Enum) | SSL connection mode | `"disable"`, `"prefer"`, `"require"` | `"prefer"` |

See the PostgreSQL source documentation for details on TableKeyConfig and SslMode.

#### ScriptFile Bootstrap Provider Properties

When using `bootstrap_provider.type: scriptfile`:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `file_paths` | Array[String] | Paths to JSONL files for bootstrap | Array of file paths | **(Required by provider)** |

#### Platform Bootstrap Provider Properties

When using `bootstrap_provider.type: platform`:

| Setting Name | Data Type | Description | Valid Values/Range | Default Value |
|--------------|-----------|-------------|-------------------|---------------|
| `query_api_url` | String | Remote Drasi Query API URL | Valid HTTP(S) URL | **(Required by provider)** |

### Configuration Examples

#### Basic Configuration

```yaml
sources:
  - id: "my-http-source"
    source_type: "http"
    auto_start: true
    dispatch_mode: "channel"  # Isolated channels with backpressure
    dispatch_buffer_capacity: 1500
    host: "0.0.0.0"        # HTTP server host (default: localhost)
    port: 8080             # HTTP server port (default: 8080)
    endpoint: "/events"    # Optional custom endpoint path
    timeout_ms: 30000      # Request timeout in milliseconds (default: 10000)
```

#### With Adaptive Batching

Adaptive batching automatically adjusts batch size and timing based on throughput for optimal performance:

```yaml
sources:
  - id: "high-throughput-source"
    source_type: "http"
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

#### With Bootstrap Provider

HTTP source supports **any** bootstrap provider through the universal SourceBase pattern. This enables powerful use cases like "bootstrap from database, stream changes via HTTP".

##### PostgreSQL Bootstrap Example

Initial data load from PostgreSQL, then receive updates via HTTP:

```yaml
sources:
  - id: "http-with-postgres-bootstrap"
    source_type: "http"
    bootstrap_provider:
      type: postgres
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

##### ScriptFile Bootstrap Example

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
    host: "localhost"
    port: 9000
```

##### Other Bootstrap Providers

- **Platform**: Bootstrap from remote Drasi Query API via HTTP
- **Application**: Replay previously stored insert events
- **Noop**: No bootstrap, streaming only

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
cargo test -p drasi-lib sources::http

# Run specific test
cargo test -p drasi-lib test_http_source_start_stop
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
