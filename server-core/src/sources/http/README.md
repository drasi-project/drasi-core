# HTTP Source

The HTTP source provides a REST API endpoint for submitting data changes to Drasi. It supports both single event and batch submissions with adaptive batching for optimal throughput.

## Features

- **Single Event Submission**: Submit individual events via REST API
- **Batch Submission**: Submit multiple events in a single request
- **Adaptive Batching**: Automatically adjusts batch size and timing based on throughput
- **Direct Format**: Uses a clean JSON format that closely mirrors internal structures
- **OpenAPI Documentation**: Interactive API documentation available at `/docs`

## Configuration

The HTTP source is configured in your Drasi server YAML configuration file:

```yaml
sources:
  - id: "my-http-source"
    source_type: "http"
    auto_start: true
    properties:
      port: 8080         # Port to listen on (default: 8080)
      host: "0.0.0.0"    # Host to bind to (default: 0.0.0.0)
      
      # Adaptive batching configuration (all optional)
      adaptive_enabled: true              # Enable adaptive batching (default: true)
      adaptive_max_batch_size: 1000       # Maximum batch size (default: 1000)
      adaptive_min_batch_size: 10         # Minimum batch size (default: 10)
      adaptive_max_wait_ms: 100           # Maximum wait time in ms (default: 100)
      adaptive_min_wait_ms: 1             # Minimum wait time in ms (default: 1)
      adaptive_window_secs: 5             # Throughput window in seconds (default: 5)
```

### Batching Configuration

The adaptive batching feature automatically optimizes for different traffic patterns:

- **Idle** (< 1 msg/sec): Minimal batching for low latency
- **Low** (1-100 msgs/sec): Small batches with minimal wait
- **Medium** (100-1,000 msgs/sec): Moderate batching
- **High** (1,000-10,000 msgs/sec): Larger batches for efficiency
- **Burst** (> 10,000 msgs/sec): Maximum throughput mode

To disable adaptive batching for low-latency requirements:
```yaml
properties:
  adaptive_enabled: false  # Events are processed immediately without batching
```

## API Endpoints

When an HTTP source is running, it exposes the following endpoints on the configured port:

- **POST** `/sources/{source_name}/events` - Submit a single data change event
- **POST** `/sources/{source_name}/events/batch` - Submit multiple events (batch)
- **GET** `/health` - Health check endpoint (returns JSON with service status and features)

### Health Check Response

```json
{
  "status": "healthy",
  "service": "adaptive-http-source",
  "features": ["adaptive-batching", "batch-endpoint"]
}
```

## Event Format (Direct Format)

The HTTP source uses a Direct Format that closely mirrors the internal Drasi Core structure:

### Single Event Structure

#### Node Insert/Update
```json
{
  "operation": "insert",  // or "update"
  "element": {
    "type": "node",
    "id": "unique_id",
    "labels": ["Label1", "Label2"],
    "properties": {
      "key": "value",
      "nested": {
        "data": "supported"
      }
    }
  },
  "timestamp": 1234567890000  // Optional, nanoseconds since Unix epoch
}
```

#### Relation Insert/Update
```json
{
  "operation": "insert",  // or "update"
  "element": {
    "type": "relation",
    "id": "relation_id",
    "labels": ["RELATIONSHIP_TYPE"],
    "from": "source_node_id",
    "to": "target_node_id",
    "properties": {
      "key": "value"
    }
  }
}
```

#### Delete Operation
```json
{
  "operation": "delete",
  "id": "element_id",
  "labels": ["Label1"],  // Optional
  "timestamp": 1234567890000  // Optional
}
```

### Batch Event Structure

```json
{
  "events": [
    {
      "operation": "insert",
      "element": {
        "type": "node",
        "id": "node_1",
        "labels": ["User"],
        "properties": {"name": "Alice"}
      }
    },
    {
      "operation": "insert",
      "element": {
        "type": "relation",
        "id": "rel_1",
        "labels": ["FOLLOWS"],
        "from": "node_1",
        "to": "node_2",
        "properties": {}
      }
    }
  ]
}
```

### Field Descriptions

- **operation**: The operation type (`insert`, `update`, or `delete`)
- **element**: The element data (for insert/update operations)
  - **type**: Element type (`node` or `relation`)
  - **id**: Unique identifier for the element
  - **labels**: Array of labels/types for the element
  - **properties**: Key-value pairs of element properties (any JSON values)
  - **from**: Source node ID (relations only)
  - **to**: Target node ID (relations only)
- **id**: Element ID to delete (for delete operations)
- **timestamp**: Optional timestamp in nanoseconds since Unix epoch

## Examples

### Insert a User Node
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
        "active": true,
        "age": 30
      }
    }
  }'
```

### Update a User Node
```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "update",
    "element": {
      "type": "node",
      "id": "user_123",
      "labels": ["User", "Customer", "Premium"],
      "properties": {
        "name": "John Doe",
        "email": "john.doe@example.com",
        "active": true,
        "age": 31,
        "subscription": "premium"
      }
    }
  }'
```

### Create a Relationship
```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "insert",
    "element": {
      "type": "relation",
      "id": "follows_456",
      "labels": ["FOLLOWS"],
      "from": "user_123",
      "to": "user_789",
      "properties": {
        "since": "2024-01-01",
        "strength": 0.8
      }
    }
  }'
```

### Delete an Element
```bash
curl -X POST http://localhost:8080/sources/my-http-source/events \
  -H "Content-Type: application/json" \
  -d '{
    "operation": "delete",
    "id": "user_123",
    "labels": ["User", "Customer"]
  }'
```

### Batch Submission
```bash
curl -X POST http://localhost:8080/sources/my-http-source/events/batch \
  -H "Content-Type: application/json" \
  -d '{
    "events": [
      {
        "operation": "insert",
        "element": {
          "type": "node",
          "id": "product_1",
          "labels": ["Product"],
          "properties": {"name": "Laptop", "price": 999.99}
        }
      },
      {
        "operation": "insert",
        "element": {
          "type": "node",
          "id": "product_2",
          "labels": ["Product"],
          "properties": {"name": "Mouse", "price": 29.99}
        }
      },
      {
        "operation": "insert",
        "element": {
          "type": "relation",
          "id": "related_1",
          "labels": ["RELATED_TO"],
          "from": "product_1",
          "to": "product_2",
          "properties": {"type": "accessory"}
        }
      }
    ]
  }'
```

## Response Format

### Success Response
```json
{
  "success": true,
  "message": "Event processed successfully",
  "error": null
}
```

### Error Response
```json
{
  "success": false,
  "message": "Invalid event data",
  "error": "Missing required field: operation"
}
```

### Batch Response (Partial Success)
```json
{
  "success": true,
  "message": "Processed 8 events successfully, 2 failed",
  "error": "Last error: Invalid element type"
}
```

## Integration with Drasi Queries

When you submit events through the HTTP source, they are processed by Drasi's continuous query engine. The element structure maps to Cypher query patterns:

- **Element ID**: Used as the unique identifier for nodes/relations
- **Labels**: Match against Cypher patterns (e.g., `MATCH (n:User)`)
- **Properties**: Accessible in queries (e.g., `WHERE n.active = true`)

Example Cypher query that would process the above events:

```cypher
MATCH (u:User)
WHERE u.active = true
RETURN u.name, u.email, u.subscription

MATCH (u1:User)-[f:FOLLOWS]->(u2:User)
RETURN u1.name, u2.name, f.since
```

## Performance Profiling

The HTTP source automatically includes profiling metadata with each event when profiling is enabled in the DrasiServerCore configuration. This enables end-to-end performance tracking from source ingestion through query processing to reaction delivery.

### Profiling Timestamps

The HTTP source automatically captures the following timestamps:
- **source_send_ns**: Timestamp when the source sends the event to the processing pipeline

These timestamps are combined with timestamps from other components to provide a complete performance picture:
- **query_receive_ns**: When the query receives the event
- **query_core_call_ns**: When drasi-core processing begins
- **query_core_return_ns**: When drasi-core processing completes
- **reaction_receive_ns**: When the reaction receives the result
- **reaction_complete_ns**: When the reaction finishes processing

### Using Profiling Data

Profiling metadata flows through the entire pipeline and can be:
1. Analyzed for bottleneck identification
2. Used to measure end-to-end latency
3. Exported via profiler reactions for monitoring
4. Aggregated to understand system performance under load

For more information on configuring profiling, see the DrasiServerCore documentation.

## Performance Considerations

### When to Use Single Events
- Low-volume scenarios (< 100 events/sec)
- When individual event latency is critical
- Real-time user interactions
- Debugging and testing

### When to Use Batch Submission
- High-volume data ingestion
- Bulk data imports
- When throughput is more important than individual latency
- Processing data from batch systems

### Adaptive Batching Behavior
When adaptive batching is enabled (default), the system automatically:
- Batches events during high load for better throughput
- Processes events immediately during low load for better latency
- Adjusts batch size based on incoming traffic patterns
- Monitors throughput and adapts parameters accordingly

## Testing

You can test the HTTP source using the provided example files:

```bash
# Start the server with HTTP source configuration
cargo run -- --config examples/configs/http_source_example.yaml

# In another terminal, submit test events
./examples/test_direct_format.sh

# Or use the provided HTTP files with VS Code REST Client
# Open examples/http_source_direct_format.http in VS Code
```

## Important Notes

1. **Timestamps**: All timestamps are Unix epoch in nanoseconds (not milliseconds)
2. **Element IDs**: Must be unique within the source
3. **Labels**: Are case-sensitive and used for query matching
4. **Properties**: Can contain any valid JSON values (including nested objects and arrays)
5. **Operation Order**: Events are processed in the order received (per batch)
6. **Batch Size**: The batch endpoint accepts up to 10,000 events per request
7. **Error Handling**: Invalid events in a batch don't stop processing of valid events

## Migration from Old Format

If you were using the old "op/payload" format, migrate to the Direct Format:

**Old Format (Deprecated):**
```json
{
  "op": "i",
  "payload": {
    "source": {"db": "...", "table": "...", "ts_ns": 123, "lsn": 1},
    "after": {"id": "123", "labels": ["User"], "properties": {...}}
  }
}
```

**New Direct Format:**
```json
{
  "operation": "insert",
  "element": {
    "type": "node",
    "id": "123",
    "labels": ["User"],
    "properties": {...}
  }
}
```

## TypeScript Types

```typescript
// Direct Format Types
interface DirectSourceChange {
  operation: "insert" | "update" | "delete";
  element?: DirectElement;  // For insert/update
  id?: string;              // For delete
  labels?: string[];        // For delete (optional)
  timestamp?: number;       // Optional, nanoseconds
}

interface DirectElement {
  type: "node" | "relation";
  id: string;
  labels: string[];
  properties: Record<string, any>;
  from?: string;  // For relations only
  to?: string;    // For relations only
}

interface BatchEventRequest {
  events: DirectSourceChange[];
}

interface EventResponse {
  success: boolean;
  message: string;
  error?: string;
}
```