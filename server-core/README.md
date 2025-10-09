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
anyhow = "1.0"
tokio = { version = "1", features = ["full"] }
serde_json = "1.0"
```

### Basic Example

Create a simple change detection pipeline:

```rust
use drasi_server_core::{
    DrasiServerCore, RuntimeConfig,
    config::{DrasiServerCoreSettings, SourceConfig, QueryConfig, ReactionConfig},
};
use std::collections::HashMap;
use std::sync::Arc;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure a change-driven pipeline
    let config = RuntimeConfig {
        server: DrasiServerCoreSettings {
            host: "127.0.0.1".to_string(),
            port: 8080,
            log_level: "info".to_string(),
            max_connections: 100,
            shutdown_timeout_seconds: 30,
            disable_persistence: true,
        },
        sources: vec![
            SourceConfig {
                id: "sensor-data".to_string(),
                source_type: "mock".to_string(),
                auto_start: true,
                properties: HashMap::from([
                    ("data_type".to_string(), json!("sensor")),
                    ("interval_ms".to_string(), json!(1000)),
                ]),
            }
        ],
        queries: vec![
            QueryConfig {
                id: "anomaly-detection".to_string(),
                query: "MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s.id, s.temperature".to_string(),
                sources: vec!["sensor-data".to_string()],
                auto_start: true,
                properties: HashMap::new(),
                joins: None,
            }
        ],
        reactions: vec![
            ReactionConfig {
                id: "alert-handler".to_string(),
                reaction_type: "log".to_string(),
                queries: vec!["anomaly-detection".to_string()],
                auto_start: true,
                properties: HashMap::from([
                    ("log_level".to_string(), json!("info")),
                ]),
            }
        ],
    };

    // Create and start the change detection engine
    let mut core = DrasiServerCore::new(Arc::new(config));
    core.initialize().await?;
    core.start().await?;

    // Run for 10 seconds
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Shutdown
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

### Programmatic Configuration

Build configurations in code for maximum control:

```rust
use drasi_server_core::config::{SourceConfig, QueryConfig, ReactionConfig};
use std::collections::HashMap;
use serde_json::json;

// Configure a PostgreSQL source for change data capture
let pg_source = SourceConfig {
    id: "orders-db".to_string(),
    source_type: "postgres".to_string(),
    auto_start: true,
    properties: HashMap::from([
        ("host".to_string(), json!("localhost")),
        ("port".to_string(), json!(5432)),
        ("database".to_string(), json!("orders")),
        ("user".to_string(), json!("postgres")),
        ("password".to_string(), json!("secret")),
        ("tables".to_string(), json!(["orders", "order_items"])),
    ]),
};

// Define a continuous query to detect business exceptions
let query = QueryConfig {
    id: "delayed-orders".to_string(),
    query: r#"
        MATCH (o:Order)-[:CONTAINS]->(i:OrderItem)
        WHERE o.status = 'pending' 
          AND duration.between(o.created_at, datetime()) > duration('PT24H')
        RETURN o.id, o.customer_id, sum(i.quantity * i.price) as total
    "#.to_string(),
    sources: vec!["orders-db".to_string()],
    auto_start: true,
    properties: HashMap::new(),
    joins: None,
};
```

### YAML Configuration

Load change detection pipelines from YAML or JSON files:

```rust
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig, RuntimeConfig};
use std::sync::Arc;

// Load configuration from YAML file
let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;

// Convert to RuntimeConfig and create DrasiServerCore
let runtime_config: RuntimeConfig = config.into();
let mut core = DrasiServerCore::new(Arc::new(runtime_config));

core.initialize().await?;
core.start().await?;
```

Example YAML configuration (`config.yaml`):

```yaml
server:
  host: 0.0.0.0
  port: 8080
  log_level: info
  max_connections: 1000
  disable_persistence: false

sources:
  - id: infrastructure-metrics
    source_type: postgres
    auto_start: true
    properties:
      host: metrics.example.com
      port: 5432
      database: telemetry
      replication_slot: drasi_slot
      publication: drasi_pub

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
SourceConfig {
    id: "test-source".to_string(),
    source_type: "mock".to_string(),
    properties: HashMap::from([
        ("data_type".to_string(), json!("sensor")), // sensor|counter|random
        ("interval_ms".to_string(), json!(500)),
    ]),
}
```

### HTTP Source

Receive changes via REST API:

```rust
SourceConfig {
    id: "http-ingestion".to_string(),
    source_type: "http".to_string(),
    properties: HashMap::from([
        ("host".to_string(), json!("0.0.0.0")),
        ("port".to_string(), json!(9000)),
        ("path".to_string(), json!("/changes")),
    ]),
}
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
SourceConfig {
    id: "grpc-stream".to_string(),
    source_type: "grpc".to_string(),
    properties: HashMap::from([
        ("host".to_string(), json!("0.0.0.0")),
        ("port".to_string(), json!(50051)),
        ("max_message_size".to_string(), json!(4194304)),
    ]),
}
```

### PostgreSQL Replication Source

Stream changes directly from PostgreSQL using logical replication:

```rust
SourceConfig {
    id: "postgres-cdc".to_string(),
    source_type: "postgres_replication".to_string(),
    properties: HashMap::from([
        ("host".to_string(), json!("localhost")),
        ("port".to_string(), json!(5432)),
        ("database".to_string(), json!("production")),
        ("user".to_string(), json!("replication_user")),
        ("password".to_string(), json!("secret")),
        ("slot_name".to_string(), json!("drasi_slot")),
        ("publication".to_string(), json!("drasi_publication")),
        ("tables".to_string(), json!(["public.orders", "public.inventory"])),
    ]),
}
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
use drasi_server_core::sources::ApplicationSource;
use drasi_core::models::{SourceChange, Element};

let app_source = ApplicationSource::new("app-events");
let sender = app_source.get_sender();

// In your business logic
async fn process_order(order: Order) {
    // Your existing logic...
    
    // Emit change for detection
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("app", &order.id),
                labels: Arc::new([Arc::from("Order")]),
                effective_from: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
            },
            properties: ElementPropertyMap::from(json!({
                "customer_id": order.customer_id,
                "total": order.total,
                "status": order.status,
            })),
        },
    };
    
    sender.send(change).await.unwrap();
}
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
```cypher
MATCH (vm:VM)-[:HOSTS]->(app:Application)
WHERE vm.cpu > 80 AND app.tier = 'production'
  AND duration.between(vm.high_cpu_start, datetime()) > duration('PT5M')
RETURN vm.id, vm.name, app.name, vm.cpu, vm.high_cpu_start
```

**Track Inventory Depletion**:
```cypher
MATCH (p:Product)-[:STORED_IN]->(w:Warehouse)
WHERE p.quantity < p.reorder_point 
  AND NOT EXISTS((p)<-[:PENDING]-(:PurchaseOrder))
RETURN p.sku, p.name, w.location, p.quantity, p.reorder_point
```

**Monitor Service Dependencies**:
```cypher
MATCH path = (s1:Service)-[:DEPENDS_ON*1..3]->(s2:Service)
WHERE s2.status = 'unhealthy'
RETURN s1.name as affected_service, 
       [n IN nodes(path) | n.name] as dependency_chain,
       s2.name as failed_service
```

**Detect Business Exceptions**:
```cypher
MATCH (c:Customer)-[:PLACED]->(o:Order)-[:CONTAINS]->(i:OrderItem)
WHERE o.promised_date < datetime() AND o.status <> 'delivered'
RETURN c.email, o.id, o.promised_date, 
       sum(i.quantity * i.price) as order_value
```

### Query Joins

Correlate changes from multiple sources:

```rust
QueryConfig {
    id: "order-fulfillment".to_string(),
    query: "MATCH (o:Order), (i:Inventory) WHERE o.product_id = i.sku RETURN o, i".to_string(),
    sources: vec!["orders-db".to_string(), "inventory-db".to_string()],
    joins: Some(vec![
        JoinConfig {
            keys: vec![
                JoinKey {
                    label: "Order".to_string(),
                    property: "product_id".to_string(),
                },
                JoinKey {
                    label: "Inventory".to_string(),
                    property: "sku".to_string(),
                },
            ],
        },
    ]),
    ..Default::default()
}
```

## Reactions

Reactions respond to detected changes by triggering downstream actions:

### Log Reaction

Debug and monitor detected changes:

```rust
ReactionConfig {
    id: "change-logger".to_string(),
    reaction_type: "log".to_string(),
    properties: HashMap::from([
        ("log_level".to_string(), json!("info")),
        ("format".to_string(), json!("json")),
    ]),
}
```

### HTTP Reaction

Trigger webhooks and REST APIs:

```rust
ReactionConfig {
    id: "alert-webhook".to_string(),
    reaction_type: "http".to_string(),
    properties: HashMap::from([
        ("endpoint".to_string(), json!("https://api.example.com/alerts")),
        ("method".to_string(), json!("POST")),
        ("headers".to_string(), json!({
            "Content-Type": "application/json",
            "X-API-Key": "secret"
        })),
        ("batch_size".to_string(), json!(100)),
        ("timeout_ms".to_string(), json!(5000)),
    ]),
}
```

### SSE Reaction

Stream changes to web applications:

```rust
ReactionConfig {
    id: "live-updates".to_string(),
    reaction_type: "sse".to_string(),
    properties: HashMap::from([
        ("host".to_string(), json!("0.0.0.0")),
        ("port".to_string(), json!(8081)),
        ("path".to_string(), json!("/changes")),
        ("keepalive_seconds".to_string(), json!(30)),
    ]),
}
```

Client connection:
```javascript
const changes = new EventSource('http://localhost:8081/changes');
changes.onmessage = (event) => {
    const change = JSON.parse(event.data);
    // React to detected change
    updateUI(change);
};
```

### gRPC Reaction

High-performance change streaming:

```rust
ReactionConfig {
    id: "grpc-notifications".to_string(),
    reaction_type: "grpc".to_string(),
    properties: HashMap::from([
        ("host".to_string(), json!("notifications.example.com")),
        ("port".to_string(), json!(50052)),
        ("service".to_string(), json!("ChangeNotificationService")),
    ]),
}
```

### Application Reaction

Process detected changes in-process:

```rust
use drasi_server_core::reactions::ApplicationReaction;

let app_reaction = ApplicationReaction::new("change-handler", |changes| {
    for change in changes {
        match change {
            QueryResult::Adding { after } => {
                // New result detected
                handle_new_detection(after);
            },
            QueryResult::Updating { before, after } => {
                // Result changed
                handle_change(before, after);
            },
            QueryResult::Removing { before } => {
                // Result no longer matches
                handle_removal(before);
            },
        }
    }
});
```

## Advanced Features

### Persistence

Enable persistence for production deployments:

```rust
let config = RuntimeConfig {
    server: DrasiServerCoreSettings {
        disable_persistence: false,  // Enable persistence
        ..Default::default()
    },
    ..Default::default()
};
```

### Custom Sources and Reactions

Extend DrasiServerCore with custom adapters:

```rust
use drasi_server_core::sources::SourceTrait;
use async_trait::async_trait;

struct CustomSource {
    // Your implementation
}

#[async_trait]
impl SourceTrait for CustomSource {
    async fn start(&mut self) -> Result<()> {
        // Begin emitting changes
    }
    
    async fn stop(&mut self) -> Result<()> {
        // Cleanup
    }
}
```

### Error Handling

Handle errors gracefully:

```rust
use drasi_server_core::error::DrasiError;

match core.start().await {
    Ok(_) => println!("Change detection started"),
    Err(DrasiError::Configuration(msg)) => eprintln!("Config error: {}", msg),
    Err(DrasiError::Component(msg)) => eprintln!("Component error: {}", msg),
    Err(e) => eprintln!("Unexpected error: {}", e),
}
```

## Building from Source

### Prerequisites

- Rust 1.75 or later
- Protocol Buffers compiler (`protoc`)

### Build Steps

```bash
# Clone the repository
git clone https://github.com/drasi-project/drasi-server-core
cd drasi-server-core

# Initialize and update the Drasi Core submodule
# This is required as the project depends on drasi-core as a Git submodule
git submodule init
git submodule update --recursive
# Alternative: Initialize and update in one command
# git submodule update --init --recursive

# Verify the submodule is properly initialized
# The drasi-core directory should contain the source files
ls -la drasi-core/

# Build the library
cargo build --release

# Run tests
cargo test

# Build documentation
cargo doc --open
```

### Feature Flags

```toml
[dependencies]
drasi-server-core = { 
    version = "0.1.0",
    features = ["postgres", "grpc", "persistence"]
}
```

Available features:
- `postgres`: PostgreSQL change capture support
- `grpc`: gRPC source/reaction support
- `persistence`: Enable persistent query state
- `metrics`: Prometheus metrics
- `tracing`: Distributed tracing

## Performance Optimization

### Query Optimization

1. **Use Specific Patterns**: Be precise in your MATCH clauses
2. **Filter Early**: Apply WHERE conditions as early as possible
3. **Limit Scope**: Use LIMIT for queries that don't need all results
4. **Index Properties**: Ensure frequently queried properties are indexed

### Source Tuning

1. **Batch Changes**: Group related changes together
2. **Buffer Sizing**: Configure appropriate buffer sizes for throughput
3. **Connection Pooling**: Use connection pools for database sources

### Reaction Optimization

1. **Enable Batching**: Group change notifications for efficiency
2. **Async Processing**: Use async reactions for better throughput
3. **Timeout Configuration**: Set appropriate timeouts for reliability

## Examples

The `examples/` directory contains complete working examples:

- `infrastructure_monitoring.rs`: Track VM and container health
- `order_processing.rs`: Detect business exceptions in order flow
- `iot_sensor_network.rs`: Monitor sensor readings for anomalies
- `compliance_tracking.rs`: Audit sensitive data changes

Run examples:
```bash
cargo run --example infrastructure_monitoring
```

## Testing

Run the test suite:

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run specific test
cargo test test_continuous_query
```

## Troubleshooting

### Common Issues

**Port Already in Use**:
```rust
// Use a different port
config.server.port = 8081;
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
std::env::set_var("RUST_LOG", "drasi_server_core=trace");
env_logger::init();
```

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

DrasiServerCore is licensed under the [Apache License 2.0](LICENSE).

## Support

- **Issues**: [GitHub Issues](https://github.com/drasi-project/drasi-server-core/issues)
- **Discussions**: [GitHub Discussions](https://github.com/drasi-project/drasi-server-core/discussions)
- **Documentation**: [https://drasi.io](https://drasi.io)

## Related Projects

- [Drasi Platform](https://github.com/drasi-project/drasi-platform): Complete Drasi deployment platform
- [Drasi Core](https://github.com/drasi-project/drasi-core): Core continuous query engine
- [Drasi Examples](https://github.com/drasi-project/drasi-examples): Example applications demonstrating change-driven solutions