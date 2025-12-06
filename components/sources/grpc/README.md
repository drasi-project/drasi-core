# gRPC Source

The gRPC Source provides a high-performance gRPC service endpoint for submitting data change events to Drasi. It exposes a Protocol Buffer-based API that supports both single event submission and streaming for high-throughput scenarios.

## Overview

The gRPC Source is a server-side component that implements the `drasi.v1.SourceService` gRPC service. External systems can push data changes (insert, update, delete operations) to Drasi using efficient binary serialization and bidirectional streaming via HTTP/2.

### Key Capabilities

- **Dual submission modes**: Unary RPC for single events or client streaming for bulk ingestion
- **High performance**: Binary Protocol Buffers over HTTP/2 with multiplexing
- **Type safety**: Strongly-typed messages defined in protobuf schemas
- **Health monitoring**: Built-in health check endpoint
- **Bootstrap support**: Extensible API for initial data snapshots (future enhancement)
- **Error handling**: Detailed error responses with validation messages
- **Lifecycle management**: Graceful startup/shutdown with status tracking
- **Flexible dispatch**: Configurable channel-based or broadcast dispatch modes

### Use Cases

- **IoT data ingestion**: Streaming sensor data from edge devices
- **Microservice integration**: Real-time event streaming between services
- **ETL pipelines**: Bulk data loading with streaming support
- **Change data capture**: Pushing database changes via gRPC clients
- **System integration**: Language-agnostic integration using protobuf
- **High-volume scenarios**: Efficient handling of thousands of events per second

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides the most ergonomic and type-safe way to construct a gRPC source:

```rust
use drasi_source_grpc::GrpcSource;
use drasi_lib::channels::DispatchMode;

// Basic configuration
let source = GrpcSource::builder("my-grpc-source")
    .with_host("0.0.0.0")
    .with_port(50051)
    .build()?;

// Advanced configuration with custom dispatch settings
let source = GrpcSource::builder("production-grpc")
    .with_host("0.0.0.0")
    .with_port(50051)
    .with_timeout_ms(10000)
    .with_dispatch_mode(DispatchMode::Channel)
    .with_dispatch_buffer_capacity(2000)
    .build()?;

// With bootstrap provider
let source = GrpcSource::builder("bootstrapped-grpc")
    .with_host("127.0.0.1")
    .with_port(50052)
    .with_bootstrap_provider(my_bootstrap_provider)
    .build()?;
```

### Config Struct Approach

For programmatic configuration or deserialization from external sources:

```rust
use drasi_source_grpc::{GrpcSource, GrpcSourceConfig};

// Using config struct
let config = GrpcSourceConfig {
    host: "0.0.0.0".to_string(),
    port: 50051,
    endpoint: None,
    timeout_ms: 5000,
};

let source = GrpcSource::new("my-grpc-source", config)?;

// With custom dispatch settings
let source = GrpcSource::with_dispatch(
    "my-grpc-source",
    config,
    Some(DispatchMode::Channel),
    Some(1500)
)?;
```

### Direct Integration with DrasiLib

```rust
use drasi_lib::DrasiLib;
use drasi_source_grpc::GrpcSource;

let drasi = DrasiLib::builder()
    .with_id("my-drasi-instance")
    .build()
    .await?;

let source = GrpcSource::builder("events-grpc")
    .with_host("0.0.0.0")
    .with_port(50051)
    .build()?;

drasi.add_source(source).await?;
drasi.start_source("events-grpc").await?;
```

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `id` | Unique identifier for the source instance | `String` | Any non-empty string | **(Required)** |
| `host` | Host address to bind the gRPC server to | `String` | Valid hostname or IP address (e.g., "0.0.0.0", "127.0.0.1") | `"0.0.0.0"` |
| `port` | Port number for the gRPC server | `u16` | 1-65535 | `50051` |
| `endpoint` | Optional custom service endpoint path | `Option<String>` | Any valid path string | `None` |
| `timeout_ms` | Request timeout in milliseconds | `u64` | Positive integer (milliseconds) | `5000` |
| `dispatch_mode` | Event dispatch strategy | `Option<DispatchMode>` | `Channel` (isolated, backpressure) or `Broadcast` (shared, no backpressure) | `Channel` |
| `dispatch_buffer_capacity` | Buffer size for dispatch channel | `Option<usize>` | Positive integer | `1000` |
| `bootstrap_provider` | Provider for initial data snapshots | `Option<Box<dyn BootstrapProvider>>` | Any type implementing `BootstrapProvider` | `None` |

### Configuration Notes

- **Host binding**: Use `"0.0.0.0"` to accept connections from any network interface, or `"127.0.0.1"` for localhost only
- **Port**: Must be available (not in use by another service)
- **Timeout**: Applies to request processing; higher values for slow network conditions
- **Dispatch mode**:
  - `Channel`: Each subscriber gets an isolated channel with backpressure (prevents message loss)
  - `Broadcast`: Single shared channel across subscribers (faster but may drop messages under load)
- **Buffer capacity**: Higher values handle bursts better but use more memory

## Input Schema

The gRPC Source accepts events in Protocol Buffer format as defined in `proto/drasi/v1/source.proto` and `proto/drasi/v1/common.proto`.

### Core Message Types

#### SourceChange (Top-level event message)

```protobuf
message SourceChange {
    ChangeType type = 1;              // INSERT, UPDATE, or DELETE
    oneof change {
        Element element = 2;          // For INSERT/UPDATE operations
        ElementMetadata metadata = 3; // For DELETE operations
    }
    google.protobuf.Timestamp timestamp = 4;
    string source_id = 5;
}

enum ChangeType {
    CHANGE_TYPE_UNSPECIFIED = 0;
    CHANGE_TYPE_INSERT = 1;
    CHANGE_TYPE_UPDATE = 2;
    CHANGE_TYPE_DELETE = 3;
}
```

#### Element (Node or Relation)

```protobuf
message Element {
    oneof element {
        Node node = 1;
        Relation relation = 2;
    }
}
```

#### Node Element

```protobuf
message Node {
    ElementMetadata metadata = 1;
    google.protobuf.Struct properties = 2;
}
```

**Fields:**
- `metadata`: Required metadata containing reference, labels, and effective timestamp
- `properties`: Key-value properties using `google.protobuf.Struct` (flexible schema)

#### Relation Element

```protobuf
message Relation {
    ElementMetadata metadata = 1;
    ElementReference in_node = 2;    // Target node (relationship points to)
    ElementReference out_node = 3;   // Source node (relationship comes from)
    google.protobuf.Struct properties = 4;
}
```

**Fields:**
- `metadata`: Required metadata for the relation
- `in_node`: Reference to the target node (where the arrow points)
- `out_node`: Reference to the source node (where the arrow originates)
- `properties`: Key-value properties of the relationship

**Direction semantics:** `(out_node)-[relation]->(in_node)`

#### ElementMetadata

```protobuf
message ElementMetadata {
    ElementReference reference = 1;
    repeated string labels = 2;
    uint64 effective_from = 3;  // Unix timestamp in nanoseconds
}
```

**Fields:**
- `reference`: Unique identifier (source_id + element_id)
- `labels`: Classification labels for pattern matching (e.g., ["User", "Customer"])
- `effective_from`: Timestamp in nanoseconds since Unix epoch

#### ElementReference

```protobuf
message ElementReference {
    string source_id = 1;
    string element_id = 2;
}
```

**Fields:**
- `source_id`: Identifies which source owns this element
- `element_id`: Unique identifier within the source

### Service Methods

The gRPC Source implements the `drasi.v1.SourceService`:

#### 1. SubmitEvent (Unary RPC)

Submit a single event.

```protobuf
rpc SubmitEvent(SubmitEventRequest) returns (SubmitEventResponse);

message SubmitEventRequest {
    SourceChange event = 1;
}

message SubmitEventResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    string event_id = 4;  // UUID assigned to the event
}
```

#### 2. StreamEvents (Client Streaming RPC)

Stream multiple events for bulk processing.

```protobuf
rpc StreamEvents(stream SourceChange) returns (stream StreamEventResponse);

message StreamEventResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    uint64 events_processed = 4;
}
```

**Behavior:**
- Accepts a stream of `SourceChange` messages
- Returns periodic progress updates (every 100 events)
- Returns final count when stream completes
- Individual event errors don't stop the stream

#### 3. RequestBootstrap (Server Streaming RPC)

Request initial data snapshot (extensible for future use).

```protobuf
rpc RequestBootstrap(BootstrapRequest) returns (stream BootstrapResponse);

message BootstrapRequest {
    string query_id = 1;
    repeated string node_labels = 2;
    repeated string relation_labels = 3;
}

message BootstrapResponse {
    repeated Element elements = 1;
    uint32 total_count = 2;
}
```

**Current behavior:** Returns empty stream (placeholder for future implementation)

#### 4. HealthCheck (Unary RPC)

Check service health status.

```protobuf
rpc HealthCheck(google.protobuf.Empty) returns (HealthCheckResponse);

message HealthCheckResponse {
    enum Status {
        STATUS_UNSPECIFIED = 0;
        STATUS_HEALTHY = 1;
        STATUS_DEGRADED = 2;
        STATUS_UNHEALTHY = 3;
    }

    Status status = 1;
    string message = 2;
    string version = 3;  // Package version
}
```

### Property Value Types

The `google.protobuf.Struct` supports the following value types:

| Protobuf Type | Drasi ElementValue | Notes |
|---------------|-------------------|-------|
| `null_value` | `Null` | Null/missing value |
| `bool_value` | `Bool` | Boolean true/false |
| `number_value` (integer) | `Integer` | Whole numbers without fractional part |
| `number_value` (float) | `Float` | Numbers with fractional part |
| `string_value` | `String` | UTF-8 text |
| `list_value` | `String` (JSON) | Arrays converted to JSON string |
| `struct_value` | `String` (JSON) | Objects converted to JSON string |

**Note:** Complex types (lists, structs) are serialized as JSON strings for storage in Drasi's graph model.

## Usage Examples

### Example 1: Basic Server Setup

```rust
use drasi_source_grpc::GrpcSource;
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create Drasi instance
    let drasi = DrasiLib::builder()
        .with_id("my-instance")
        .build()
        .await?;

    // Create and start gRPC source
    let source = GrpcSource::builder("events")
        .with_host("0.0.0.0")
        .with_port(50051)
        .build()?;

    drasi.add_source(source).await?;
    drasi.start_source("events").await?;

    println!("gRPC source listening on 0.0.0.0:50051");

    // Keep server running
    tokio::signal::ctrl_c().await?;

    Ok(())
}
```

### Example 2: Python Client - Submit Single Node

```python
import grpc
from google.protobuf import struct_pb2
import drasi.v1.source_pb2 as source_pb2
import drasi.v1.source_pb2_grpc as source_pb2_grpc
import drasi.v1.common_pb2 as common_pb2
import time

# Connect to gRPC source
channel = grpc.insecure_channel('localhost:50051')
stub = source_pb2_grpc.SourceServiceStub(channel)

# Create properties
props = struct_pb2.Struct()
props['name'] = 'Alice'
props['email'] = 'alice@example.com'
props['age'] = 30
props['active'] = True

# Create metadata
metadata = common_pb2.ElementMetadata(
    reference=common_pb2.ElementReference(
        source_id="my-grpc-source",
        element_id="user_001"
    ),
    labels=["User", "Customer"],
    effective_from=int(time.time() * 1e9)  # Nanoseconds
)

# Create node
node = common_pb2.Node(
    metadata=metadata,
    properties=props
)

# Create source change
change = common_pb2.SourceChange(
    type=common_pb2.CHANGE_TYPE_INSERT,
    element=common_pb2.Element(node=node)
)

# Submit event
request = source_pb2.SubmitEventRequest(event=change)
response = stub.SubmitEvent(request)

print(f"Success: {response.success}")
print(f"Message: {response.message}")
print(f"Event ID: {response.event_id}")
```

### Example 3: Python Client - Stream Events

```python
import grpc
from google.protobuf import struct_pb2
import drasi.v1.source_pb2_grpc as source_pb2_grpc
import drasi.v1.common_pb2 as common_pb2
import time

channel = grpc.insecure_channel('localhost:50051')
stub = source_pb2_grpc.SourceServiceStub(channel)

def event_generator():
    """Generate 1000 user nodes"""
    for i in range(1000):
        props = struct_pb2.Struct()
        props['name'] = f'User {i}'
        props['index'] = i
        props['active'] = i % 2 == 0

        metadata = common_pb2.ElementMetadata(
            reference=common_pb2.ElementReference(
                source_id="my-grpc-source",
                element_id=f"user_{i:04d}"
            ),
            labels=["User"],
            effective_from=int(time.time() * 1e9)
        )

        node = common_pb2.Node(metadata=metadata, properties=props)

        yield common_pb2.SourceChange(
            type=common_pb2.CHANGE_TYPE_INSERT,
            element=common_pb2.Element(node=node)
        )

# Stream events
responses = stub.StreamEvents(event_generator())

for response in responses:
    print(f"Processed: {response.events_processed} events")
    if not response.success:
        print(f"Error: {response.error}")

print("Stream completed")
```

### Example 4: Go Client - Insert Relation

```go
package main

import (
    "context"
    "log"
    "time"

    pb "your-module/drasi/v1"
    "google.golang.org/grpc"
    "google.golang.org/protobuf/types/known/structpb"
)

func main() {
    // Connect to gRPC source
    conn, err := grpc.Dial("localhost:50051", grpc.WithInsecure())
    if err != nil {
        log.Fatalf("Failed to connect: %v", err)
    }
    defer conn.Close()

    client := pb.NewSourceServiceClient(conn)

    // Create relation properties
    properties, _ := structpb.NewStruct(map[string]interface{}{
        "since": "2024-01-01",
        "strength": 0.85,
    })

    // Create relation (Alice FOLLOWS Bob)
    event := &pb.SourceChange{
        Type: pb.ChangeType_CHANGE_TYPE_INSERT,
        Change: &pb.SourceChange_Element{
            Element: &pb.Element{
                Element: &pb.Element_Relation{
                    Relation: &pb.Relation{
                        Metadata: &pb.ElementMetadata{
                            Reference: &pb.ElementReference{
                                SourceId: "my-grpc-source",
                                ElementId: "follows_001",
                            },
                            Labels: []string{"FOLLOWS"},
                            EffectiveFrom: uint64(time.Now().UnixNano()),
                        },
                        OutNode: &pb.ElementReference{
                            SourceId: "my-grpc-source",
                            ElementId: "user_001",  // Alice
                        },
                        InNode: &pb.ElementReference{
                            SourceId: "my-grpc-source",
                            ElementId: "user_002",  // Bob
                        },
                        Properties: properties,
                    },
                },
            },
        },
    }

    // Submit event
    resp, err := client.SubmitEvent(context.Background(), &pb.SubmitEventRequest{
        Event: event,
    })

    if err != nil {
        log.Fatalf("Failed to submit: %v", err)
    }

    log.Printf("Success: %v, Message: %s", resp.Success, resp.Message)
}
```

### Example 5: Rust Client - Update and Delete

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
    let mut client = SourceServiceClient::connect("http://localhost:50051").await?;

    // Update example
    let mut update_props = HashMap::new();
    update_props.insert(
        "email".to_string(),
        Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "alice.new@example.com".to_string()
            )),
        },
    );
    update_props.insert(
        "verified".to_string(),
        Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        },
    );

    let update_event = SourceChange {
        r#type: ChangeType::Update as i32,
        change: Some(drasi::v1::source_change::Change::Element(Element {
            element: Some(drasi::v1::element::Element::Node(Node {
                metadata: Some(ElementMetadata {
                    reference: Some(ElementReference {
                        source_id: "my-grpc-source".to_string(),
                        element_id: "user_001".to_string(),
                    }),
                    labels: vec!["User".to_string(), "Verified".to_string()],
                    effective_from: chrono::Utc::now().timestamp_nanos() as u64,
                }),
                properties: Some(Struct { fields: update_props }),
            })),
        })),
        timestamp: None,
        source_id: String::new(),
    };

    let response = client.submit_event(SubmitEventRequest {
        event: Some(update_event),
    }).await?;

    println!("Update: {}", response.into_inner().message);

    // Delete example
    let delete_event = SourceChange {
        r#type: ChangeType::Delete as i32,
        change: Some(drasi::v1::source_change::Change::Metadata(ElementMetadata {
            reference: Some(ElementReference {
                source_id: "my-grpc-source".to_string(),
                element_id: "user_999".to_string(),
            }),
            labels: vec!["User".to_string()],
            effective_from: chrono::Utc::now().timestamp_nanos() as u64,
        })),
        timestamp: None,
        source_id: String::new(),
    };

    let response = client.submit_event(SubmitEventRequest {
        event: Some(delete_event),
    }).await?;

    println!("Delete: {}", response.into_inner().message);

    Ok(())
}
```

### Example 6: Testing with grpcurl

```bash
# Install grpcurl
go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest

# List available services
grpcurl -plaintext localhost:50051 list

# Health check
grpcurl -plaintext localhost:50051 drasi.v1.SourceService/HealthCheck

# Submit a node insert event
grpcurl -plaintext -d '{
  "event": {
    "type": "CHANGE_TYPE_INSERT",
    "element": {
      "node": {
        "metadata": {
          "reference": {
            "sourceId": "my-grpc-source",
            "elementId": "test_001"
          },
          "labels": ["TestNode"],
          "effectiveFrom": "1234567890000000000"
        },
        "properties": {
          "name": "Test Item",
          "value": 42
        }
      }
    }
  }
}' localhost:50051 drasi.v1.SourceService/SubmitEvent
```

## Integration with Drasi Queries

Events submitted through the gRPC source flow into Drasi's continuous query engine where they can be matched against Cypher patterns.

### Label Matching

```rust
// Submit a node with labels ["User", "Premium"]
// This will match Cypher patterns like:
// MATCH (u:User) ...
// MATCH (p:Premium) ...
// MATCH (u:User:Premium) ...
```

### Property Filtering

```rust
// Submit a node with property active=true
// This will be filtered by Cypher WHERE clauses:
// MATCH (u:User) WHERE u.active = true
// MATCH (u:User) WHERE u.age > 18
```

### Relationship Traversal

```rust
// Submit a relation (user_001)-[:FOLLOWS]->(user_002)
// This enables Cypher patterns:
// MATCH (a:User)-[:FOLLOWS]->(b:User)
// MATCH (a)-[f:FOLLOWS]->(b) WHERE f.strength > 0.8
```

## Advanced Topics

### Dispatch Modes

**Channel Mode (Default):**
- Each subscriber gets an isolated channel
- Provides backpressure (blocks if subscriber is slow)
- Guarantees zero message loss
- Higher memory usage under load

**Broadcast Mode:**
- Single shared channel for all subscribers
- No backpressure (continues if subscriber is slow)
- May drop messages if buffer fills
- Lower latency and memory usage

```rust
let source = GrpcSource::builder("events")
    .with_dispatch_mode(DispatchMode::Broadcast)
    .with_dispatch_buffer_capacity(5000)
    .build()?;
```

### Bootstrap Providers

Enable initial data snapshots for queries:

```rust
use drasi_lib::bootstrap::BootstrapProvider;

struct MyBootstrapProvider {
    // ... your implementation
}

impl BootstrapProvider for MyBootstrapProvider {
    // ... implement trait methods
}

let source = GrpcSource::builder("events")
    .with_bootstrap_provider(MyBootstrapProvider::new())
    .build()?;
```

### Profiling and Performance

The gRPC source includes built-in profiling metadata:

```rust
// Automatically tracked:
// - source_send_ns: Timestamp when event enters the source
// - Propagated through the query pipeline
// - Available in reactions for end-to-end latency measurement
```

### Error Handling

**Validation Errors:**
- Missing required fields (metadata, in_node, out_node)
- Invalid change types
- Malformed protobuf messages

**Response contains:**
```rust
SubmitEventResponse {
    success: false,
    message: "Invalid event data",
    error: "Validation error: Node element missing required 'metadata' field",
    event_id: ""
}
```

**Streaming behavior:**
- Individual errors don't stop the stream
- Valid events continue processing
- Periodic responses include error counts

### Graceful Shutdown

```rust
// The source handles shutdown signals gracefully:
// 1. Stops accepting new connections
// 2. Completes in-flight requests
// 3. Closes channels
// 4. Releases resources

drasi.stop_source("events").await?;
```

## Generating Client Code

### Python

```bash
# Install tools
pip install grpcio-tools

# Generate code
python -m grpc_tools.protoc \
    -I./proto \
    --python_out=./generated \
    --grpc_python_out=./generated \
    proto/drasi/v1/source.proto \
    proto/drasi/v1/common.proto
```

### Go

```bash
# Install tools
go install google.golang.org/protobuf/cmd/protoc-gen-go@latest
go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@latest

# Generate code
protoc \
    --go_out=./generated \
    --go-grpc_out=./generated \
    --go_opt=paths=source_relative \
    --go-grpc_opt=paths=source_relative \
    -I ./proto \
    proto/drasi/v1/source.proto \
    proto/drasi/v1/common.proto
```

### Rust

Add to your `build.rs`:

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile(
            &[
                "proto/drasi/v1/source.proto",
                "proto/drasi/v1/common.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
```

### JavaScript/TypeScript

```bash
# Install tools
npm install -g grpc-tools grpc_tools_node_protoc_ts

# Generate code
grpc_tools_node_protoc \
    --js_out=import_style=commonjs,binary:./generated \
    --grpc_out=grpc_js:./generated \
    --plugin=protoc-gen-grpc=`which grpc_tools_node_protoc_plugin` \
    -I ./proto \
    proto/drasi/v1/source.proto \
    proto/drasi/v1/common.proto
```

## Performance Guidelines

### When to Use SubmitEvent (Unary)

- Low to moderate event rates (< 100 events/second)
- Need immediate per-event confirmation
- Interactive applications with user feedback
- Critical events requiring acknowledgment
- Testing and debugging

### When to Use StreamEvents (Streaming)

- High-volume ingestion (> 1000 events/second)
- Bulk data imports or migrations
- ETL pipelines with batching
- IoT sensor streams
- Log aggregation
- When network efficiency matters

### Best Practices

1. **Connection reuse**: Create one gRPC channel and reuse for all requests
2. **Stream for bulk**: Use streaming for bulk operations (10x+ faster)
3. **Buffer sizing**: Tune `dispatch_buffer_capacity` based on throughput
4. **Connection pooling**: For high concurrency scenarios
5. **Keep-alive**: Configure gRPC keep-alive for long-lived connections
6. **Batch client-side**: Accumulate events before streaming
7. **Monitor metrics**: Track event processing rates and latency

### Performance Benchmarks (Approximate)

| Scenario | Events/sec | Notes |
|----------|------------|-------|
| Unary RPC (single event) | 500-1000 | Round-trip per event |
| Streaming (batched) | 10,000-50,000 | Depends on event size |
| Streaming (small events) | 100,000+ | Minimal properties |

## Comparison with HTTP Source

| Feature | gRPC Source | HTTP Source |
|---------|------------|-------------|
| Protocol | HTTP/2 + Protocol Buffers | HTTP/1.1 + JSON |
| Type Safety | Strongly typed (protobuf) | JSON schema validation |
| Performance | Higher throughput, lower latency | Good for moderate loads |
| Streaming | Native bidirectional streaming | Batch POST endpoint |
| Message Size | Smaller (binary encoding) | Larger (text JSON) |
| Client Generation | Auto-generated from .proto files | Manual or OpenAPI codegen |
| Browser Support | Limited (requires grpc-web proxy) | Native browser support |
| Debugging | Requires gRPC tools (grpcurl, Postman) | Standard HTTP tools (curl) |
| Best For | High-volume, microservices, IoT | Web apps, simple integration |

## Security Considerations

The gRPC source currently runs without authentication or encryption. For production deployments:

### TLS/SSL

```rust
// Future enhancement: TLS configuration
// Configure server TLS certificates
// Enforce encrypted connections
```

### Authentication

```rust
// Future enhancement: Authentication interceptors
// Implement token-based auth
// Add API key validation
```

### Network Security

- Use firewall rules to restrict access
- Deploy behind a reverse proxy (Envoy, Nginx)
- Use Kubernetes network policies
- Implement rate limiting at proxy level

### Monitoring

- Track request rates and patterns
- Alert on unusual traffic spikes
- Log failed authentication attempts
- Monitor resource usage

## Troubleshooting

### Connection Refused

```
Error: Connection refused
```

**Solutions:**
- Verify source is started: `drasi.start_source("my-grpc").await?`
- Check port availability: `lsof -i :50051`
- Verify host binding (use "0.0.0.0" not "localhost" for external access)

### Invalid Event Data

```
SubmitEventResponse { success: false, error: "Validation error: Node element missing required 'metadata' field" }
```

**Solutions:**
- Ensure all required fields are populated
- Check protobuf message structure
- Validate element_id is not empty
- Verify labels array is not empty

### Slow Event Processing

**Symptoms:** High latency, timeouts

**Solutions:**
- Increase `dispatch_buffer_capacity`
- Use streaming instead of unary calls
- Check query complexity
- Monitor system resources

### No Subscribers

```
[my-grpc-source] Failed to dispatch (no subscribers): No subscribers available
```

**This is normal:** Events are dropped if no queries are subscribed. Add a query:

```rust
let query = Query::cypher("my-query")
    .query("MATCH (n) RETURN n")
    .from_source("my-grpc-source")
    .build();

drasi.add_query(query).await?;
```

## Developer Notes

### Source Code Structure

- `src/lib.rs`: Main source implementation and gRPC service
- `src/config.rs`: Configuration structs and validation
- `src/tests.rs`: Unit tests
- `build.rs`: Protobuf compilation
- `proto/drasi/v1/`: Protobuf schema definitions

### Testing

```bash
# Run unit tests
cargo test -p drasi-source-grpc

# Run with logging
RUST_LOG=debug cargo test -p drasi-source-grpc -- --nocapture

# Test specific module
cargo test -p drasi-source-grpc proto_conversion
```

### Contributing

When modifying the gRPC source:

1. Update protobuf schemas in `proto/drasi/v1/`
2. Regenerate code: `cargo build` (runs `build.rs`)
3. Add tests for new functionality
4. Update this README with examples
5. Run `cargo clippy` and `cargo fmt`

## License

Licensed under the Apache License, Version 2.0. See the LICENSE file for details.

## Related Components

- **HTTP Source**: REST API alternative for web applications
- **PostgreSQL Source**: CDC from PostgreSQL databases
- **Application Source**: Programmatic event submission in Rust
- **Query Engine**: Continuous Cypher query evaluation
- **Reactions**: Output handlers for query results
