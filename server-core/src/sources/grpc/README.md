# gRPC Source

The gRPC source provides a gRPC service endpoint for submitting data changes to Drasi. It supports both single event submission and streaming for high-throughput scenarios.

## Features

- **Single Event Submission**: Submit individual events via gRPC unary call
- **Streaming Events**: Stream multiple events for high-throughput ingestion
- **Bootstrap Support**: Request initial data snapshot (extensible)
- **Health Checks**: Built-in health check endpoint
- **Protocol Buffers**: Strongly-typed message definitions
- **High Performance**: Efficient binary protocol with HTTP/2 transport

## Configuration

The gRPC source is configured in your Drasi server YAML configuration file:

```yaml
sources:
  - id: "my-grpc-source"
    source_type: "grpc"
    auto_start: true
    properties:
      port: 50051       # Port to listen on (default: 50051)
      host: "0.0.0.0"   # Host to bind to (default: 0.0.0.0)
```

## Service Definition

The gRPC source implements the `SourceService` defined in `proto/drasi/v1/source.proto`:

```protobuf
service SourceService {
    // Submit a single source change event
    rpc SubmitEvent(SubmitEventRequest) returns (SubmitEventResponse);
    
    // Stream source change events
    rpc StreamEvents(stream SourceChange) returns (stream StreamEventResponse);
    
    // Request bootstrap data for a query
    rpc RequestBootstrap(BootstrapRequest) returns (stream BootstrapResponse);
    
    // Health check
    rpc HealthCheck(google.protobuf.Empty) returns (HealthCheckResponse);
}
```

## Message Formats

### SourceChange Message

The core message for submitting data changes:

```protobuf
message SourceChange {
    ChangeType type = 1;              // INSERT, UPDATE, or DELETE
    oneof change {
        Element element = 2;          // For insert/update
        ElementMetadata metadata = 3; // For delete
    }
    google.protobuf.Timestamp timestamp = 4;
    string source_id = 5;
}
```

### Element Types

#### Node Element
```protobuf
message Node {
    ElementMetadata metadata = 1;
    google.protobuf.Struct properties = 2;
}

message ElementMetadata {
    ElementReference reference = 1;
    repeated string labels = 2;
    uint64 effective_from = 3; // Unix timestamp in nanoseconds
}

message ElementReference {
    string source_id = 1;
    string element_id = 2;
}
```

#### Relation Element
```protobuf
message Relation {
    ElementMetadata metadata = 1;
    ElementReference in_node = 2;    // Target node
    ElementReference out_node = 3;   // Source node
    google.protobuf.Struct properties = 4;
}
```

## API Methods

### 1. SubmitEvent (Unary)

Submit a single event to the source.

**Request:**
```protobuf
message SubmitEventRequest {
    SourceChange event = 1;
}
```

**Response:**
```protobuf
message SubmitEventResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    string event_id = 4;
}
```

### 2. StreamEvents (Client Streaming)

Stream multiple events for efficient batch processing.

**Request:** Stream of `SourceChange` messages

**Response:** Stream of `StreamEventResponse` messages
```protobuf
message StreamEventResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    uint64 events_processed = 4;
}
```

The service sends periodic updates (every 100 events) and a final response when the stream completes.

### 3. RequestBootstrap (Server Streaming)

Request initial data snapshot for a query (currently returns empty, extensible for future use).

**Request:**
```protobuf
message BootstrapRequest {
    string query_id = 1;
    repeated string node_labels = 2;
    repeated string relation_labels = 3;
}
```

**Response:** Stream of `BootstrapResponse` messages
```protobuf
message BootstrapResponse {
    repeated Element elements = 1;
    uint32 total_count = 2;
}
```

### 4. HealthCheck (Unary)

Check the health status of the gRPC source.

**Request:** Empty

**Response:**
```protobuf
message HealthCheckResponse {
    Status status = 1;    // HEALTHY, DEGRADED, or UNHEALTHY
    string message = 2;
    string version = 3;
}
```

## Client Examples

### Python Client Example

```python
import grpc
from google.protobuf import struct_pb2, timestamp_pb2
import drasi.v1.source_pb2 as source_pb2
import drasi.v1.source_pb2_grpc as source_pb2_grpc
import drasi.v1.common_pb2 as common_pb2
import time

# Connect to the gRPC source
channel = grpc.insecure_channel('localhost:50051')
stub = source_pb2_grpc.SourceServiceStub(channel)

# Create a node insert event
def create_node_insert(node_id, labels, properties):
    # Create properties struct
    props = struct_pb2.Struct()
    for key, value in properties.items():
        if isinstance(value, bool):
            props[key] = value
        elif isinstance(value, (int, float)):
            props[key] = value
        else:
            props[key] = str(value)
    
    # Create metadata
    metadata = common_pb2.ElementMetadata(
        reference=common_pb2.ElementReference(
            source_id="my-grpc-source",
            element_id=node_id
        ),
        labels=labels,
        effective_from=int(time.time() * 1e9)  # Nanoseconds
    )
    
    # Create node
    node = common_pb2.Node(
        metadata=metadata,
        properties=props
    )
    
    # Create element
    element = common_pb2.Element(node=node)
    
    # Create source change
    change = common_pb2.SourceChange(
        type=common_pb2.CHANGE_TYPE_INSERT,
        element=element
    )
    
    return change

# Submit a single event
event = create_node_insert(
    "user_123",
    ["User", "Customer"],
    {"name": "John Doe", "email": "john@example.com", "active": True}
)

request = source_pb2.SubmitEventRequest(event=event)
response = stub.SubmitEvent(request)
print(f"Response: success={response.success}, message={response.message}")

# Stream multiple events
def event_generator():
    for i in range(100):
        yield create_node_insert(
            f"user_{i}",
            ["User"],
            {"name": f"User {i}", "index": i}
        )

responses = stub.StreamEvents(event_generator())
for response in responses:
    print(f"Stream response: {response.message}")
```

### Go Client Example

```go
package main

import (
    "context"
    "log"
    "time"
    
    pb "your-module/drasi/v1"
    "google.golang.org/grpc"
    "google.golang.org/protobuf/types/known/structpb"
    "google.golang.org/protobuf/types/known/timestamppb"
)

func main() {
    // Connect to gRPC source
    conn, err := grpc.Dial("localhost:50051", grpc.WithInsecure())
    if err != nil {
        log.Fatalf("Failed to connect: %v", err)
    }
    defer conn.Close()
    
    client := pb.NewSourceServiceClient(conn)
    
    // Create a node insert event
    properties, _ := structpb.NewStruct(map[string]interface{}{
        "name": "John Doe",
        "email": "john@example.com",
        "active": true,
    })
    
    event := &pb.SourceChange{
        Type: pb.ChangeType_CHANGE_TYPE_INSERT,
        Change: &pb.SourceChange_Element{
            Element: &pb.Element{
                Element: &pb.Element_Node{
                    Node: &pb.Node{
                        Metadata: &pb.ElementMetadata{
                            Reference: &pb.ElementReference{
                                SourceId: "my-grpc-source",
                                ElementId: "user_123",
                            },
                            Labels: []string{"User", "Customer"},
                            EffectiveFrom: uint64(time.Now().UnixNano()),
                        },
                        Properties: properties,
                    },
                },
            },
        },
        Timestamp: timestamppb.Now(),
    }
    
    // Submit single event
    resp, err := client.SubmitEvent(context.Background(), &pb.SubmitEventRequest{
        Event: event,
    })
    
    if err != nil {
        log.Fatalf("Failed to submit event: %v", err)
    }
    
    log.Printf("Response: success=%v, message=%s", resp.Success, resp.Message)
}
```

### Rust Client Example

```rust
use tonic::transport::Channel;
use prost_types::{Struct, Value};
use std::collections::HashMap;

pub mod drasi {
    pub mod v1 {
        tonic::include_proto!("drasi.v1");
    }
}

use drasi::v1::{
    source_service_client::SourceServiceClient,
    SubmitEventRequest, SourceChange, Element, Node, ElementMetadata,
    ElementReference, ChangeType,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Connect to gRPC source
    let mut client = SourceServiceClient::connect("http://localhost:50051").await?;
    
    // Create properties
    let mut properties = HashMap::new();
    properties.insert(
        "name".to_string(),
        Value {
            kind: Some(prost_types::value::Kind::StringValue("John Doe".to_string())),
        },
    );
    properties.insert(
        "active".to_string(),
        Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        },
    );
    
    // Create node insert event
    let event = SourceChange {
        r#type: ChangeType::Insert as i32,
        change: Some(drasi::v1::source_change::Change::Element(Element {
            element: Some(drasi::v1::element::Element::Node(Node {
                metadata: Some(ElementMetadata {
                    reference: Some(ElementReference {
                        source_id: "my-grpc-source".to_string(),
                        element_id: "user_123".to_string(),
                    }),
                    labels: vec!["User".to_string(), "Customer".to_string()],
                    effective_from: chrono::Utc::now().timestamp_nanos() as u64,
                }),
                properties: Some(Struct { fields: properties }),
            })),
        })),
        timestamp: None,
        source_id: String::new(),
    };
    
    // Submit event
    let request = tonic::Request::new(SubmitEventRequest {
        event: Some(event),
    });
    
    let response = client.submit_event(request).await?;
    let resp = response.into_inner();
    
    println!("Response: success={}, message={}", resp.success, resp.message);
    
    Ok(())
}
```

## Example Messages

### Insert Node

```json
{
  "type": "CHANGE_TYPE_INSERT",
  "element": {
    "node": {
      "metadata": {
        "reference": {
          "sourceId": "my-grpc-source",
          "elementId": "user_123"
        },
        "labels": ["User", "Customer"],
        "effectiveFrom": "1234567890000000000"
      },
      "properties": {
        "name": "John Doe",
        "email": "john@example.com",
        "age": 30,
        "active": true
      }
    }
  }
}
```

### Update Node

```json
{
  "type": "CHANGE_TYPE_UPDATE",
  "element": {
    "node": {
      "metadata": {
        "reference": {
          "sourceId": "my-grpc-source",
          "elementId": "user_123"
        },
        "labels": ["User", "Customer", "Premium"],
        "effectiveFrom": "1234567891000000000"
      },
      "properties": {
        "name": "John Doe",
        "email": "john.doe@example.com",
        "subscription": "premium"
      }
    }
  }
}
```

### Insert Relation

```json
{
  "type": "CHANGE_TYPE_INSERT",
  "element": {
    "relation": {
      "metadata": {
        "reference": {
          "sourceId": "my-grpc-source",
          "elementId": "follows_456"
        },
        "labels": ["FOLLOWS"],
        "effectiveFrom": "1234567890000000000"
      },
      "inNode": {
        "sourceId": "my-grpc-source",
        "elementId": "user_789"
      },
      "outNode": {
        "sourceId": "my-grpc-source",
        "elementId": "user_123"
      },
      "properties": {
        "since": "2024-01-01",
        "strength": 0.8
      }
    }
  }
}
```

### Delete Element

```json
{
  "type": "CHANGE_TYPE_DELETE",
  "metadata": {
    "reference": {
      "sourceId": "my-grpc-source",
      "elementId": "user_123"
    },
    "labels": ["User", "Customer"],
    "effectiveFrom": "1234567892000000000"
  }
}
```

## Performance Considerations

### When to Use Single Events (SubmitEvent)
- Low-volume scenarios (< 100 events/sec)
- When you need immediate confirmation for each event
- Interactive applications requiring per-event error handling
- Testing and debugging

### When to Use Streaming (StreamEvents)
- High-volume data ingestion (> 1000 events/sec)
- Bulk data imports
- When network efficiency is important
- Processing data from streaming sources
- When you can handle errors in batches

### Performance Tips
1. **Reuse Connections**: Create one gRPC channel and reuse it for multiple requests
2. **Use Streaming**: For bulk operations, streaming is significantly more efficient
3. **Batch Processing**: The service processes streamed events in batches internally
4. **Connection Pooling**: For high concurrency, implement connection pooling
5. **Keep-Alive**: Configure gRPC keep-alive for long-lived connections

## Comparison with HTTP Source

| Feature | gRPC Source | HTTP Source |
|---------|------------|-------------|
| Protocol | HTTP/2 + Protocol Buffers | HTTP/1.1 + JSON |
| Type Safety | Strongly typed | JSON schema validation |
| Performance | Higher throughput | Good for moderate loads |
| Streaming | Native bidirectional streaming | Batch endpoint only |
| Message Size | Smaller (binary) | Larger (text JSON) |
| Client Generation | Auto-generated from .proto | Manual or OpenAPI |
| Browser Support | Limited (requires proxy) | Native support |
| Debugging | Requires special tools | Standard HTTP tools |

## Integration with Drasi Queries

Events submitted through the gRPC source are processed by Drasi's continuous query engine:

- **Element IDs**: Unique identifiers for nodes/relations
- **Labels**: Match against Cypher patterns (e.g., `MATCH (n:User)`)
- **Properties**: Accessible in queries (e.g., `WHERE n.active = true`)
- **Relations**: Define graph connections between nodes

Example Cypher queries:

```cypher
// Match inserted users
MATCH (u:User)
WHERE u.active = true
RETURN u.name, u.email

// Match relationships
MATCH (u1:User)-[f:FOLLOWS]->(u2:User)
RETURN u1.name as follower, u2.name as followed
```

## Generating Client Code

To generate client code from the protobuf definitions:

### Python
```bash
pip install grpcio-tools
python -m grpc_tools.protoc -I./proto --python_out=. --grpc_python_out=. proto/drasi/v1/*.proto
```

### Go
```bash
go install google.golang.org/protobuf/cmd/protoc-gen-go@latest
go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@latest
protoc --go_out=. --go-grpc_out=. proto/drasi/v1/*.proto
```

### Rust
Add to `build.rs`:
```rust
fn main() {
    tonic_build::configure()
        .build_server(false)
        .compile(&["proto/drasi/v1/source.proto"], &["proto"])
        .unwrap();
}
```

## Testing

Test the gRPC source using grpcurl:

```bash
# Check health
grpcurl -plaintext localhost:50051 drasi.v1.SourceService/HealthCheck

# Submit an event (requires JSON input)
grpcurl -plaintext -d '{
  "event": {
    "type": 1,
    "element": {
      "node": {
        "metadata": {
          "reference": {
            "sourceId": "test",
            "elementId": "node1"
          },
          "labels": ["TestNode"]
        },
        "properties": {
          "name": "Test"
        }
      }
    }
  }
}' localhost:50051 drasi.v1.SourceService/SubmitEvent
```

## Important Notes

1. **Timestamps**: All timestamps are Unix epoch in nanoseconds
2. **Element IDs**: Must be unique within the source
3. **Source ID**: Usually matches the configured source ID
4. **Labels**: Case-sensitive and used for query matching
5. **Properties**: Use `google.protobuf.Struct` for flexible schemas
6. **Relations**: `out_node` is the source, `in_node` is the target
7. **Streaming**: Responses are sent periodically, not for every event
8. **Error Handling**: Stream continues processing valid events even if some fail

## Error Handling

The gRPC source provides detailed error information:

- **Invalid Data**: Returns error in response with details
- **Channel Errors**: Logged and returned as internal errors
- **Connection Issues**: Standard gRPC error codes apply
- **Streaming Errors**: Individual errors don't stop the stream

## Security Considerations

Currently, the gRPC source runs without authentication. For production:

1. **TLS**: Configure TLS certificates for encrypted connections
2. **Authentication**: Implement gRPC interceptors for auth
3. **Rate Limiting**: Add rate limiting to prevent abuse
4. **Network Security**: Use firewall rules to restrict access
5. **Monitoring**: Track metrics and unusual patterns

## Future Enhancements

- **Bootstrap Implementation**: Full support for initial data snapshots
- **Compression**: gRPC compression for large payloads
- **Metadata Propagation**: Custom headers and tracing
- **Batch Unary**: Single call for multiple events
- **Schema Registry**: Integration with schema management