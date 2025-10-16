# DrasiServerCore

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

DrasiServerCore is a Rust library for building change-driven solutions that detect and react to data changes with precision. It simplifies the complex task of tracking specific, contextual data transitions across distributed systems, enabling you to build responsive applications faster with less complexity.

## Overview

DrasiServerCore enables **change-driven solutions** through a powerful **Source → Query → Reaction** pattern:

- **Sources** capture data changes from your existing systems
- **Continuous Queries** detect precisely the changes that matter using openCypher graph queries
- **Reactions** respond to those changes by triggering downstream actions

Unlike traditional event-driven systems that require complex event parsing and state management, DrasiServerCore uses continuous queries to maintain live result sets that automatically update as your data changes. This transforms complex change detection into declarative, cost-effective implementations.

## Why DrasiServerCore?

Traditional event-driven architectures often struggle with:
- **Event Overload**: Processing streams of generic events to find what matters
- **Complex Filtering**: Writing custom logic to parse event payloads and filter irrelevant changes
- **State Management**: Maintaining context across multiple events to detect meaningful patterns
- **Brittle Implementations**: Hard-coded change detection that breaks when systems evolve

DrasiServerCore solves these challenges by focusing on **precise change semantics** - detecting not just that data changed, but understanding exactly what changed and why it matters to your business logic.

## Key Use Cases

- **Infrastructure Health Monitoring**: Track resource utilization across VMs, containers, and services to detect anomalies and optimize costs
- **Business Exception Detection**: Identify delayed orders, inventory shortages, or SLA violations as they occur
- **Building Management & IoT**: Monitor sensor networks for environmental changes that require immediate action
- **Resource Optimization**: Detect underutilized resources or capacity constraints before they impact operations
- **Compliance & Audit**: Track sensitive data changes with full context for regulatory requirements
- **Real-time Personalization**: Update user experiences instantly based on behavior changes

## Quick Start

### Installation

Add DrasiServerCore to your `Cargo.toml`:

```toml
[dependencies]
drasi-server-core = "0.1.0"
tokio = { version = "1", features = ["full"] }
serde_json = "1.0"
```

### Basic Example - Builder API

Create a change detection pipeline using the fluent builder API:

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure a change-driven pipeline using the fluent builder API
    let core = DrasiServerCore::builder()
        .with_id("my-server")
        .add_source(
            Source::mock("sensor-data")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "sensor")
                        .with_int("interval_ms", 1000)
                )
                .build()
        )
        .add_query(
            Query::cypher("anomaly-detection")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s.id, s.temperature")
                .from_source("sensor-data")
                .build()
        )
        .add_reaction(
            Reaction::log("alert-handler")
                .subscribe_to("anomaly-detection")
                .with_property("log_level", json!("info"))
                .build()
        )
        .build()
        .await?;

    // Start the change detection engine
    core.start().await?;

    // Run for 10 seconds
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Graceful shutdown
    core.stop().await?;
    Ok(())
}
```

### Loading from Configuration File

Load pre-existing YAML/JSON configuration:

```rust
use drasi_server_core::DrasiServerCore;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load and initialize from config file in one step
    let core = DrasiServerCore::from_config_file("config.yaml").await?;

    core.start().await?;

    // ... your application logic ...

    core.stop().await?;
    Ok(())
}
```

## Architecture

DrasiServerCore implements a change-driven architecture with these key components:

### The Continuous Query Pattern

```
Sources → [Change Events] → Continuous Queries → [Change Notifications] → Reactions
```

1. **Sources** emit changes as graph elements (nodes and relationships)
2. **Continuous Queries** maintain live result sets that update incrementally
3. **Reactions** receive precise change notifications with before/after states

### Data Model

DrasiServerCore uses a labeled property graph model that naturally represents complex relationships:
- **Nodes**: Entities with labels and properties (e.g., `(:VM {id: "vm-1", cpu: 85})`)
- **Relationships**: Connections that provide context (e.g., `[:HOSTED_ON]`)
- **Changes**: Insert, Update, or Delete operations with full change semantics

This model enables queries that understand not just individual changes, but changes in context - like detecting when a VM's CPU usage exceeds threshold while it's hosting critical services.

## Configuration

### Fluent Builder API (Recommended)

Build configurations programmatically with type-safe, fluent API:

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
use serde_json::json;

let core = DrasiServerCore::builder()
    .with_id("order-monitoring")
    // Configure a PostgreSQL source for change data capture
    .add_source(
        Source::postgres("orders-db")
            .with_properties(
                Properties::new()
                    .with_string("host", "localhost")
                    .with_int("port", 5432)
                    .with_string("database", "orders")
                    .with_string("user", "postgres")
                    .with_string("password", "secret")
            )
            .with_postgres_bootstrap()
            .build()
    )
    // Define a continuous query to detect business exceptions
    .add_query(
        Query::cypher("delayed-orders")
            .query(r#"
                MATCH (o:Order)-[:CONTAINS]->(i:OrderItem)
                WHERE o.status = 'pending'
                  AND duration.between(o.created_at, datetime()) > duration('PT24H')
                RETURN o.id, o.customer_id, sum(i.quantity * i.price) as total
            "#)
            .from_source("orders-db")
            .build()
    )
    // Configure HTTP reaction for alerts
    .add_reaction(
        Reaction::http("alert-webhook")
            .subscribe_to("delayed-orders")
            .with_properties(
                Properties::new()
                    .with_string("endpoint", "https://api.example.com/alerts")
                    .with_string("method", "POST")
            )
            .build()
    )
    .build()
    .await?;

core.start().await?;
```

### YAML Configuration

Load change detection pipelines from YAML or JSON files:

```rust
use drasi_server_core::DrasiServerCore;

// Load configuration from YAML file
let core = DrasiServerCore::from_config_file("config.yaml").await?;
core.start().await?;
```

Example YAML configuration (`config.yaml`):

```yaml
server:
  id: infrastructure-monitor

sources:
  - id: infrastructure-metrics
    source_type: postgres
    auto_start: true
    properties:
      host: metrics.example.com
      port: 5432
      database: telemetry
      user: drasi_user
      password: secret

queries:
  - id: vm-health-monitor
    query: |
      MATCH (vm:VM)-[:RUNS]->(s:Service)
      WHERE vm.cpu_usage > 80 OR vm.memory_usage > 90
      RETURN vm.id, vm.name, collect(s.name) as services,
             vm.cpu_usage, vm.memory_usage
    sources:
      - infrastructure-metrics
    auto_start: true

reactions:
  - id: ops-alerts
    reaction_type: http
    queries:
      - vm-health-monitor
    auto_start: true
    properties:
      endpoint: https://ops.example.com/alerts
      method: POST
```

## Sources

Sources are adapters that capture changes from your existing systems and convert them into the graph model:

### Mock Source

Generate test data for development:

```rust
Source::mock("test-source")
    .with_properties(
        Properties::new()
            .with_string("data_type", "sensor")  // sensor|counter|random
            .with_int("interval_ms", 500)
    )
    .build()
```

### HTTP Source

Receive changes via REST API:

```rust
Source::http("http-ingestion")
    .with_properties(
        Properties::new()
            .with_string("host", "0.0.0.0")
            .with_int("port", 9000)
            .with_string("path", "/changes")
    )
    .build()
```

Submit changes:
```bash
curl -X POST http://localhost:9000/changes \
  -H "Content-Type: application/json" \
  -d '{
    "op": "i",
    "element": {
      "type": "node",
      "id": "order-123",
      "labels": ["Order"],
      "properties": {
        "status": "pending",
        "created_at": "2024-01-15T10:00:00Z",
        "customer_id": "cust-456"
      }
    }
  }'
```

### gRPC Source

High-performance change ingestion:

```rust
Source::grpc("grpc-stream")
    .with_properties(
        Properties::new()
            .with_string("host", "0.0.0.0")
            .with_int("port", 50051)
            .with_int("max_message_size", 4194304)
    )
    .build()
```

### PostgreSQL Source

Stream changes directly from PostgreSQL using logical replication:

```rust
Source::postgres("postgres-cdc")
    .with_properties(
        Properties::new()
            .with_string("host", "localhost")
            .with_int("port", 5432)
            .with_string("database", "production")
            .with_string("user", "replication_user")
            .with_string("password", "secret")
    )
    .with_postgres_bootstrap()
    .build()
```

PostgreSQL setup:
```sql
-- Enable logical replication
ALTER SYSTEM SET wal_level = logical;

-- Create publication for change tracking
CREATE PUBLICATION drasi_publication FOR TABLE orders, inventory;

-- Create replication slot
SELECT pg_create_logical_replication_slot('drasi_slot', 'pgoutput');
```

### Application Source

Embed change detection directly in your application:

```rust
use drasi_server_core::{DrasiServerCore, Source};

// Configure application source
let core = DrasiServerCore::builder()
    .add_source(Source::application("app-events").build())
    .build()
    .await?;

core.start().await?;

// Get handle to send changes
let source = core.source_handle("app-events")?;

// In your business logic - send changes
source.send_node_insert(
    "order-123",
    vec!["Order"],
    property_map! {
        "customer_id" => "cust-456",
        "total" => 150.00,
        "status" => "pending"
    }
).await?;
```

## Continuous Queries

Continuous queries are the heart of DrasiServerCore's change detection capabilities. Written in openCypher, they declaratively specify what changes matter to your application.

### Query Capabilities

- **Pattern Matching**: Detect complex relationships between entities
- **Temporal Operations**: Track time-based conditions and durations
- **Aggregations**: Compute running totals, averages, and other metrics
- **Contextual Detection**: Understand changes in relation to surrounding data
- **Multi-Source Joins**: Correlate changes across different systems

### Example Continuous Queries

**Detect Resource Anomalies**:
```rust
Query::cypher("resource-anomalies")
    .query(r#"
        MATCH (vm:VM)-[:HOSTS]->(app:Application)
        WHERE vm.cpu > 80 AND app.tier = 'production'
          AND duration.between(vm.high_cpu_start, datetime()) > duration('PT5M')
        RETURN vm.id, vm.name, app.name, vm.cpu, vm.high_cpu_start
    "#)
    .from_source("infrastructure")
    .build()
```

**Track Inventory Depletion**:
```rust
Query::cypher("low-inventory")
    .query(r#"
        MATCH (p:Product)-[:STORED_IN]->(w:Warehouse)
        WHERE p.quantity < p.reorder_point
          AND NOT EXISTS((p)<-[:PENDING]-(:PurchaseOrder))
        RETURN p.sku, p.name, w.location, p.quantity, p.reorder_point
    "#)
    .from_sources(vec!["inventory-db".into(), "purchasing-db".into()])
    .build()
```

**Monitor Service Dependencies**:
```rust
Query::cypher("service-health")
    .query(r#"
        MATCH path = (s1:Service)-[:DEPENDS_ON*1..3]->(s2:Service)
        WHERE s2.status = 'unhealthy'
        RETURN s1.name as affected_service,
               [n IN nodes(path) | n.name] as dependency_chain,
               s2.name as failed_service
    "#)
    .from_source("service-mesh")
    .build()
```

## Reactions

Reactions respond to detected changes by triggering downstream actions:

### Log Reaction

Debug and monitor detected changes:

```rust
Reaction::log("change-logger")
    .subscribe_to("my-query")
    .with_property("log_level", json!("info"))
    .build()
```

### HTTP Reaction

Trigger webhooks and REST APIs:

```rust
Reaction::http("alert-webhook")
    .subscribe_to("anomaly-detection")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "https://api.example.com/alerts")
            .with_string("method", "POST")
    )
    .build()
```

### SSE Reaction

Stream changes to web applications:

```rust
Reaction::sse("live-updates")
    .subscribe_to("real-time-data")
    .with_properties(
        Properties::new()
            .with_string("host", "0.0.0.0")
            .with_int("port", 8081)
            .with_string("path", "/changes")
    )
    .build()
```

Client connection:
```javascript
const changes = new EventSource('http://localhost:8081/changes');
changes.onmessage = (event) => {
    const change = JSON.parse(event.data);
    updateUI(change);
};
```

### gRPC Reaction

High-performance change streaming:

```rust
Reaction::grpc("grpc-notifications")
    .subscribe_to("critical-events")
    .with_properties(
        Properties::new()
            .with_string("host", "notifications.example.com")
            .with_int("port", 50052)
    )
    .build()
```

### Application Reaction

Process detected changes in-process:

```rust
use drasi_server_core::{DrasiServerCore, Reaction};

let core = DrasiServerCore::builder()
    .add_reaction(
        Reaction::application("change-handler")
            .subscribe_to("my-query")
            .build()
    )
    .build()
    .await?;

core.start().await?;

// Get handle to receive changes
let reaction = core.reaction_handle("change-handler")?;

// Process changes asynchronously
let mut stream = reaction.as_stream().await.unwrap();
while let Some(result) = stream.next().await {
    println!("Detected change: {:?}", result);
    // Handle the change...
}
```

## Error Handling

The new API provides typed errors for better error handling:

```rust
use drasi_server_core::{DrasiError, Result};

match core.start().await {
    Ok(_) => println!("Started successfully"),
    Err(DrasiError::Configuration(msg)) => {
        eprintln!("Configuration error: {}", msg);
    }
    Err(DrasiError::ComponentNotFound { kind, id }) => {
        eprintln!("{} '{}' not found", kind, id);
    }
    Err(DrasiError::InvalidState(msg)) => {
        eprintln!("Invalid state: {}", msg);
    }
    Err(e) => eprintln!("Error: {}", e),
}
```

## Building from Source

### Prerequisites

- Rust 1.75 or later
- Protocol Buffers compiler (`protoc`)

### Build Steps

```bash
# Clone the repository
git clone https://github.com/drasi-project/drasi-core
cd drasi-core/server-core

# Build the library
cargo build --release

# Run tests
cargo test

# Build documentation
cargo doc --open
```

## Examples

The `examples/` directory contains working examples:

```bash
# Run basic integration example with builder API
cargo run --example basic_integration builder

# Run with config file
cargo run --example basic_integration config config/basic.yaml
```

## Performance Optimization

### Query Optimization

1. **Use Specific Patterns**: Be precise in your MATCH clauses
2. **Filter Early**: Apply WHERE conditions as early as possible
3. **Index Properties**: Ensure frequently queried properties are indexed

### Source Tuning

1. **Batch Changes**: Group related changes together
2. **Buffer Sizing**: Configure appropriate buffer sizes for throughput
3. **Connection Pooling**: Use connection pools for database sources

### Reaction Optimization

1. **Enable Batching**: Group change notifications for efficiency
2. **Async Processing**: Use async reactions for better throughput
3. **Timeout Configuration**: Set appropriate timeouts for reliability

## Testing

Run the test suite:

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run specific test
cargo test test_builder_api
```

## Troubleshooting

### Common Issues

**Component Not Found**:
```rust
// Ensure component IDs match
let handle = core.source_handle("my-source")?;
```

**PostgreSQL Replication Not Working**:
- Ensure `wal_level = logical`
- Check replication user permissions
- Verify publication includes desired tables

**Query Not Detecting Changes**:
- Verify query syntax with Cypher validator
- Check that sources emit expected labels
- Enable debug logging to trace change flow

### Debug Logging

```rust
// Enable detailed logging
std::env::set_var("RUST_LOG", "drasi_server_core=debug");
env_logger::init();
```

## API Migration

For users migrating from the old API, see the key changes:

**Old API (Deprecated)**:
```rust
let config = Arc::new(RuntimeConfig { ... });
let mut core = DrasiServerCore::new(config);
core.initialize().await?;  // Two-phase
core.start().await?;
```

**New API (Recommended)**:
```rust
let core = DrasiServerCore::builder()
    .add_source(Source::mock("test").build())
    .build()
    .await?;  // Single-phase
core.start().await?;
```

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

DrasiServerCore is licensed under the [Apache License 2.0](LICENSE).

## Support

- **Issues**: [GitHub Issues](https://github.com/drasi-project/drasi-core/issues)
- **Discussions**: [GitHub Discussions](https://github.com/drasi-project/drasi-core/discussions)
- **Documentation**: [https://drasi.io](https://drasi.io)

## Related Projects

- [Drasi Platform](https://github.com/drasi-project/drasi-platform): Complete Drasi deployment platform
- [Drasi Core](https://github.com/drasi-project/drasi-core): Core continuous query engine
