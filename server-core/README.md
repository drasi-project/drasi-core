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

## Bootstrap Process

Before continuous queries can detect changes, they need an initial data snapshot. This is **bootstrap** - loading the starting state so queries know what "changed" means.

### Why Bootstrap Matters

Without bootstrap, your query has no context:
- A query for "temperature > 75" can't know if a sensor reading of 80° is new or existing
- Bootstrap provides the baseline; streaming changes update it

### When Bootstrap Happens

Bootstrap runs automatically when you call `core.start()`:
1. Sources bootstrap first (load initial data)
2. Queries receive bootstrap data and build initial result sets
3. Streaming begins - sources emit live changes

### Bootstrap Providers

Different sources use different providers:

**NoOp** (default): No initial data
```rust
Source::mock("test").build()  // No bootstrap
```

**ScriptFile**: Load from JSONL files (great for testing)
```rust
Source::mock("test")
    .with_bootstrap_script("data/initial.jsonl")
    .build()
```

**PostgreSQL**: Snapshot of current database state
```rust
Source::postgres("db")
    .with_postgres_bootstrap()  // Snapshot at startup
    .build()
```

**Application**: Replay stored events
```rust
Source::application("app")
    .with_application_bootstrap()
    .build()
```

**Platform**: Load from remote Drasi Query API
```rust
Source::platform("external")
    .with_platform_bootstrap("http://api:8080")
    .build()
```

### Timing Considerations

- `core.start()` is **async but non-blocking** for bootstrap
- Bootstrap runs in background; queries start processing as data arrives
- Use component status to monitor bootstrap completion
- Streaming begins immediately after bootstrap for each source

## Sources

Sources are adapters that capture changes from your existing systems and convert them into the graph model.

> **Detailed Documentation:** Each source has comprehensive documentation in its respective directory with configuration properties, examples, input/output formats, and troubleshooting guides.
>
> See: `src/sources/{source-name}/README.md`

### Available Sources

**Mock Source** - Generate test data for development and testing.

**HTTP Source** - Accept changes via REST API for webhook-based integrations.

**gRPC Source** - High-performance bidirectional streaming for high-throughput scenarios.

**PostgreSQL Source** - Stream changes from PostgreSQL using logical replication for real-time change data capture.

**Application Source** - Embed change detection directly in your application for in-process data integration.

**Platform Source** - Consume events from external Drasi Platform sources via Redis Streams.

### Example: HTTP Source

Accept changes via REST API:

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

Send changes via HTTP POST:
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
        "created_at": "2024-01-15T10:00:00Z"
      }
    }
  }'
```

For detailed configuration, data formats, and examples, see:
- **[Mock Source README](src/sources/mock/README.md)**
- **[HTTP Source README](src/sources/http/README.md)**
- **[gRPC Source README](src/sources/grpc/README.md)**
- **[PostgreSQL Source README](src/sources/postgres/README.md)**
- **[Application Source README](src/sources/application/README.md)**
- **[Platform Source README](src/sources/platform/README.md)**

## Continuous Queries

Continuous queries are the heart of DrasiServerCore's change detection capabilities. Written in openCypher, they declaratively specify what changes matter to your application.

### Understanding Query Results

When your application reaction receives changes, it gets a `QueryResult` containing:

```rust
pub enum QueryResult {
    Adding { after: HashMap<String, Value> },      // New row matched query
    Updating { before: HashMap<String, Value>,     // Row changed
               after: HashMap<String, Value> },
    Removing { before: HashMap<String, Value> },   // Row no longer matches
}
```

**How RETURN clauses map to results:**
- Each field in your RETURN becomes a key in the HashMap
- Values are JSON types (String, Number, Bool, Object, Array, Null)

**Example:**
```cypher
MATCH (s:Sensor) WHERE s.temperature > 75
RETURN s.id AS sensor_id, s.temperature AS temp
```

If sensor-123 changes from 70°→80°, you receive:
```rust
QueryResult::Adding {
    after: {
        "sensor_id": "123",
        "temp": 80
    }
}
```

If it changes 80°→85°:
```rust
QueryResult::Updating {
    before: { "sensor_id": "123", "temp": 80 },
    after: { "sensor_id": "123", "temp": 85 }
}
```

If it drops to 70°:
```rust
QueryResult::Removing {
    before: { "sensor_id": "123", "temp": 80 }
}
```

**Key concept:** Continuous queries emit **diffs**, not full result sets. You only receive changes when:
- A new row starts matching the query (Adding)
- A matching row's properties change (Updating)
- A row stops matching the query (Removing)

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

Reactions respond to detected changes by triggering downstream actions.

> **Detailed Documentation:** Each reaction has comprehensive documentation in its respective directory with configuration properties, examples, input/output formats, and troubleshooting guides.
>
> See: `src/reactions/{reaction-name}/README.md`

### Available Reactions

**Log Reaction** - Debug and monitor detected changes by logging to stdout.

**HTTP Reaction** - Trigger webhooks and REST APIs with detected changes.

**HTTP Adaptive Reaction** - HTTP reaction with adaptive batching for high-volume scenarios.

**gRPC Reaction** - High-performance change streaming via gRPC.

**gRPC Adaptive Reaction** - gRPC reaction with adaptive batching for high-volume scenarios.

**SSE Reaction** - Stream changes to web applications via Server-Sent Events.

**Application Reaction** - Process detected changes in-process within your application.

**Platform Reaction** - Publish changes to Redis Streams for Drasi Platform integration.

**Profiler Reaction** - Collect and report performance statistics for the query processing pipeline.

### Example: HTTP Reaction

Trigger webhooks when changes are detected:

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

### Example: Application Reaction

Process changes in-process:

```rust
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

For detailed configuration, data formats, and examples, see:
- **[Log Reaction README](src/reactions/log/README.md)**
- **[HTTP Reaction README](src/reactions/http/README.md)**
- **[HTTP Adaptive Reaction README](src/reactions/http_adaptive/README.md)**
- **[gRPC Reaction README](src/reactions/grpc/README.md)**
- **[gRPC Adaptive Reaction README](src/reactions/grpc_adaptive/README.md)**
- **[SSE Reaction README](src/reactions/sse/README.md)**
- **[Application Reaction README](src/reactions/application/README.md)**
- **[Platform Reaction README](src/reactions/platform/README.md)**
- **[Profiler Reaction README](src/reactions/profiler/README.md)**

## Component Management API

DrasiServerCore provides a comprehensive API for complete runtime control over sources, queries, and reactions. You can list, inspect, start, stop, add, and remove components dynamically.

### Component Lifecycle Control

Start and stop individual components without affecting others:

```rust
// Create components with auto_start disabled
let core = DrasiServerCore::builder()
    .add_source(Source::mock("source1").auto_start(false).build())
    .add_query(
        Query::cypher("query1")
            .query("MATCH (n) RETURN n")
            .from_source("source1")
            .auto_start(false)
            .build()
    )
    .add_reaction(
        Reaction::log("reaction1")
            .subscribe_to("query1")
            .auto_start(false)
            .build()
    )
    .build()
    .await?;

// Start components manually when needed
core.start_source("source1").await?;
core.start_query("query1").await?;
core.start_reaction("reaction1").await?;

// Stop components independently
core.stop_reaction("reaction1").await?;
core.stop_query("query1").await?;
core.stop_source("source1").await?;
```

**Available lifecycle methods:**
- `start_source(id: &str)` - Start a stopped source
- `stop_source(id: &str)` - Stop a running source
- `start_query(id: &str)` - Start a stopped query
- `stop_query(id: &str)` - Stop a running query
- `start_reaction(id: &str)` - Start a stopped reaction
- `stop_reaction(id: &str)` - Stop a running reaction

### Listing and Inspection

Get information about all components or specific ones:

```rust
// List all components with their status
let sources = core.list_sources().await?;
for (id, status) in sources {
    println!("Source {}: {:?}", id, status);
}

let queries = core.list_queries().await?;
let reactions = core.list_reactions().await?;

// Get detailed information about specific components
let source_info = core.get_source_info("my-source").await?;
println!("Type: {}", source_info.source_type);
println!("Status: {:?}", source_info.status);
println!("Properties: {:?}", source_info.properties);

let query_info = core.get_query_info("my-query").await?;
println!("Query: {}", query_info.query);
println!("Sources: {:?}", query_info.sources);

let reaction_info = core.get_reaction_info("my-reaction").await?;
println!("Type: {}", reaction_info.reaction_type);
println!("Queries: {:?}", reaction_info.queries);

// Check component status
let status = core.get_source_status("my-source").await?;
let status = core.get_query_status("my-query").await?;
let status = core.get_reaction_status("my-reaction").await?;

// Get current query results
let results = core.get_query_results("my-query").await?;
println!("Current result set: {} items", results.len());
```

### Dynamic Runtime Configuration

Add or remove components while the server is running:

```rust
// Add components at runtime
core.add_source_runtime(
    Source::postgres("new-db")
        .with_properties(
            Properties::new()
                .with_string("host", "localhost")
                .with_string("database", "newdb")
        )
        .auto_start(true)
        .build()
).await?;

core.add_query_runtime(
    Query::cypher("new-query")
        .query("MATCH (n) RETURN n")
        .from_source("new-db")
        .auto_start(true)
        .build()
).await?;

core.add_reaction_runtime(
    Reaction::http("new-webhook")
        .subscribe_to("new-query")
        .with_property("endpoint", json!("https://api.example.com"))
        .auto_start(true)
        .build()
).await?;

// Remove components when no longer needed
// Note: Remove in dependency order (reactions -> queries -> sources)
core.remove_reaction("new-webhook").await?;
core.remove_query("new-query").await?;
core.remove_source("new-db").await?;
```

### Component States

Each source, query, and reaction has a status:

```rust
pub enum ComponentStatus {
    Starting,   // Initializing, bootstrapping
    Running,    // Active and processing
    Stopping,   // Shutting down
    Stopped,    // Halted
    Error,      // Failed with error
}
```

### Server Lifecycle

```
Created → build().await → Initialized → start() → Running → stop() → Stopped
```

**Key methods:**
- `build().await` - Creates and initializes server (single phase)
- `start().await` - Starts all auto_start components
- `stop().await` - Gracefully stops all components
- `is_running()` - Check if server is running

### Use Cases

**Selective Component Activation:**
```rust
// Start only specific components based on configuration
let active_queries = vec!["query1", "query3"];

for query_id in active_queries {
    core.start_query(query_id).await?;
}
```

**Health Monitoring and Recovery:**
```rust
// Monitor and restart failed components
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        // Check component health
        if let Ok(status) = core.get_query_status("critical-query").await {
            if matches!(status, ComponentStatus::Error) {
                println!("Query failed, restarting...");
                let _ = core.stop_query("critical-query").await;
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = core.start_query("critical-query").await;
            }
        }
    }
});
```

**Dynamic Monitoring:**
```rust
// Add monitoring for new services dynamically
async fn monitor_service(core: &DrasiServerCore, service_id: &str) -> Result<()> {
    let query_id = format!("monitor-{}", service_id);

    // Add query to detect issues
    core.add_query_runtime(
        Query::cypher(&query_id)
            .query(format!("MATCH (s:Service {{id: '{}'}}) WHERE s.status = 'down' RETURN s", service_id))
            .from_source("services")
            .auto_start(true)
            .build()
    ).await?;

    // Add reaction to alert on issues
    core.add_reaction_runtime(
        Reaction::http(&format!("alert-{}", service_id))
            .subscribe_to(&query_id)
            .with_property("endpoint", json!("https://alerts.example.com"))
            .auto_start(true)
            .build()
    ).await?;

    Ok(())
}
```

### Examples

See working examples:
```bash
# Complete lifecycle control demo
cargo run --example component_lifecycle

# Listing and inspection demo
cargo run --example component_inspection
```

For complete API documentation, see [COMPONENT_MANAGEMENT_API.md](COMPONENT_MANAGEMENT_API.md).

## Async Integration Patterns

DrasiServerCore is built on Tokio and requires proper async runtime setup.

### Tokio Runtime Requirements

**Minimum:** Multi-threaded runtime with all features
```rust
#[tokio::main]
async fn main() -> Result<()> {
    // Your code here
}
```

Or manually:
```rust
fn main() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async {
            // Your code here
        })
}
```

### Concurrent Operations

Handle operations are Send + Clone - use them freely across tasks:

```rust
let source = core.source_handle("my-source")?;

// Send changes from multiple tasks
let source_clone = source.clone();
tokio::spawn(async move {
    source_clone.send_node_insert(...).await?;
    Ok::<_, anyhow::Error>(())
});

// Original still works
source.send_node_insert(...).await?;
```

### Graceful Shutdown

Use signals for clean shutdown:

```rust
use tokio::signal;

let core = DrasiServerCore::builder().build().await?;
core.start().await?;

// Wait for Ctrl+C
signal::ctrl_c().await?;

println!("Shutting down...");
core.stop().await?;
```

### Processing Results Asynchronously

```rust
let reaction = core.reaction_handle("my-reaction")?;

tokio::spawn(async move {
    let mut stream = reaction.as_stream().await.unwrap();
    while let Some(result) = stream.next().await {
        // Process asynchronously
        process_result(result).await;
    }
});
```

### Timeouts and Backpressure

```rust
use tokio::time::{timeout, Duration};

// Set timeout for operations
match timeout(Duration::from_secs(5), source.send_node_insert(...)).await {
    Ok(Ok(_)) => println!("Sent successfully"),
    Ok(Err(e)) => eprintln!("Send error: {}", e),
    Err(_) => eprintln!("Timeout"),
}
```

### Example: Complete Async Integration

See [`examples/bidirectional/`](examples/bidirectional/) for full bidirectional integration showing:
- Spawning concurrent receivers
- Async result processing
- Clean shutdown handling

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

### Common Error Scenarios

**Component Not Found:**
```rust
// Ensure IDs match exactly
let source = core.source_handle("my-source")?;  // Must match Source::application("my-source")
```

**Channel Closed:**
```rust
// Reaction handle consumed - can only call once
let stream = reaction.as_stream().await;  // First call - OK
let stream2 = reaction.as_stream().await; // Returns None - receiver already taken
```

**Invalid State:**
```rust
// Can't get handles before starting
core.start().await?;
let handle = core.source_handle("app-source")?;  // OK now
```

### Recovery Patterns

```rust
// Retry with backoff
let mut attempts = 0;
loop {
    match source.send_node_insert(...).await {
        Ok(_) => break,
        Err(e) if attempts < 3 => {
            attempts += 1;
            tokio::time::sleep(Duration::from_millis(100 * attempts)).await;
        }
        Err(e) => return Err(e),
    }
}
```

## Runtime Configuration

Add or remove components dynamically while the server is running.

### Adding Components at Runtime

```rust
use drasi_server_core::config::{SourceConfig, QueryConfig, ReactionConfig};

// Add new source
core.add_source_runtime(SourceConfig {
    id: "new-source".to_string(),
    source_type: "mock".to_string(),
    auto_start: true,
    properties: HashMap::new(),
    bootstrap_provider: None,
}).await?;

// Add new query
core.add_query_runtime(QueryConfig {
    id: "new-query".to_string(),
    query: "MATCH (n:Node) RETURN n".to_string(),
    sources: vec!["new-source".to_string()],
    auto_start: true,
}).await?;

// Add new reaction
core.add_reaction_runtime(ReactionConfig {
    id: "new-reaction".to_string(),
    reaction_type: "log".to_string(),
    queries: vec!["new-query".to_string()],
    auto_start: true,
    properties: HashMap::new(),
}).await?;
```

### Removing Components

```rust
// Remove reaction first (depends on query)
core.remove_reaction("new-reaction").await?;

// Remove query (depends on source)
core.remove_query("new-query").await?;

// Finally remove source
core.remove_source("new-source").await?;
```

### Dependency Management

**Important:** Respect component dependencies:

1. **Remove order:** Reactions → Queries → Sources
2. **Add order:** Sources → Queries → Reactions

```rust
// WRONG - will fail
core.remove_source("db").await?;  // Still has queries using it!

// RIGHT
core.remove_reaction("alerts").await?;   // Remove dependents first
core.remove_query("high-cpu").await?;    // Then query
core.remove_source("db").await?;         // Finally source
```

### Use Cases

**Dynamic monitoring:**
```rust
// Add monitoring for new service
async fn monitor_service(core: &DrasiServerCore, service_id: &str) -> Result<()> {
    let query_id = format!("monitor-{}", service_id);

    core.add_query_runtime(QueryConfig {
        id: query_id.clone(),
        query: format!("MATCH (s:Service {{id: '{}'}}) WHERE s.status = 'down' RETURN s", service_id),
        sources: vec!["services".to_string()],
        auto_start: true,
    }).await?;

    core.add_reaction_runtime(ReactionConfig {
        id: format!("alert-{}", service_id),
        reaction_type: "http".to_string(),
        queries: vec![query_id],
        auto_start: true,
        properties: HashMap::from([(
            "endpoint".to_string(),
            json!("https://alerts.example.com")
        )]),
    }).await?;

    Ok(())
}
```

**Limitations:**
- Cannot modify existing components (remove and re-add instead)
- Components must have unique IDs
- Cannot remove components with active dependencies

## Query Limitations

DrasiServerCore continuous queries don't support certain stateful Cypher operations:

### Unsupported Features

**ORDER BY, LIMIT, SKIP, TOP** - Not supported
```cypher
-- ❌ Won't work
MATCH (n:Node) RETURN n ORDER BY n.value LIMIT 10

-- ✅ Alternative: Order client-side
MATCH (n:Node) RETURN n
// Sort results in your application
```

**Why?** Continuous queries emit diffs. Ordering is a stateful operation that requires the full result set. With continuous queries, you receive changes as they happen, not the complete ordered set.

### Workarounds

**Client-side ranking:**
```rust
let mut stream = reaction.as_stream().await.unwrap();
let mut items = Vec::new();

while let Some(result) = stream.next().await {
    // Update in-memory collection
    update_items(&mut items, result);

    // Sort when needed
    items.sort_by_key(|item| item.value);

    // Take top 10
    let top_10 = items.iter().take(10);
}
```

**Separate aggregation query:**
```cypher
// Instead of ordering individuals, track aggregates
MATCH (n:Node)
RETURN max(n.value) as max_value, min(n.value) as min_value, avg(n.value) as avg_value
```

### Best Practices

1. **Filter early** - Use WHERE to reduce data
2. **Aggregate in query** - Use COUNT, SUM, MAX, MIN, AVG
3. **Sort in application** - Maintain sorted collections client-side
4. **Use relationships** - Express ranking as graph structure

## Performance Profiling

DrasiServerCore includes built-in profiling capabilities to track end-to-end latency through the Source → Query → Reaction pipeline.

### Quick Start

Enable profiling for any source and add a Profiler reaction:

```rust
let core = DrasiServerCore::builder()
    .add_source(
        Source::mock("profiled-source")
            .with_properties(
                Properties::new()
                    .with_bool("enable_profiling", true)  // Enable profiling
            )
            .build()
    )
    .add_query(
        Query::cypher("my-query")
            .query("MATCH (n:Node) RETURN n")
            .from_source("profiled-source")
            .build()
    )
    .add_reaction(
        Reaction::profiler("profiler")  // Automatic statistics collection
            .subscribe_to("my-query")
            .with_properties(
                Properties::new()
                    .with_int("window_size", 1000)
                    .with_int("report_interval_secs", 30)
            )
            .build()
    )
    .build()
    .await?;
```

The Profiler reaction automatically logs detailed statistics:

```
Profiling Statistics (1000 samples):

Source → Query Latency:
  count: 1000, mean: 245.3 μs, std dev: 78.2 μs
  min: 120.5 μs, max: 892.1 μs
  p50: 235.0 μs, p95: 387.5 μs, p99: 524.8 μs

Query Processing Time:
  count: 1000, mean: 1.2 ms, std dev: 0.4 ms
  min: 0.5 ms, max: 3.8 ms
  p50: 1.1 ms, p95: 1.9 ms, p99: 2.4 ms

Total End-to-End:
  count: 1000, mean: 2.5 ms, std dev: 0.6 ms
  min: 1.2 ms, max: 5.3 ms
  p50: 2.4 ms, p95: 3.6 ms, p99: 4.2 ms
```

### Profiling Documentation

For comprehensive profiling documentation, see:

- **[Profiler Reaction README](src/reactions/profiler/README.md)** - Complete profiler reaction documentation
  - Configuration properties and examples
  - Statistical methods (Welford's algorithm, percentiles)
  - Interpreting results and identifying bottlenecks
  - Performance tuning and troubleshooting

- **[PROFILING.md](docs/PROFILING.md)** - Overall profiling system architecture
  - Profiling metadata structure and flow
  - Timestamp capture points (9 stages)
  - Source configuration for profiling
  - Manual profiling in custom components
  - Performance impact analysis

### Key Features

- **9 Timestamp Points**: Captures latency at every stage of processing
- **5 Key Metrics**: Source→Query, Query Processing, Query→Reaction, Reaction Processing, Total End-to-End
- **Statistical Analysis**: Mean, standard deviation, percentiles (p50, p95, p99)
- **Sliding Window**: Configurable sample window for accurate percentile calculation
- **Periodic Reports**: Automatic logging at configurable intervals

### Best Practices

1. **Enable Selectively**: Profiling adds ~100ns overhead per event - enable only when needed
2. **Monitor p99**: The 99th percentile reveals tail latency issues
3. **Compare Metrics**: Identify bottlenecks by comparing latencies across pipeline stages
4. **Use for SLOs**: Set Service Level Objectives based on measured percentiles

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

The `examples/` directory contains working demonstrations:

### Basic Integration
Builder API and config file usage:
```bash
# Run with fluent builder API
cd examples/basic_integration
cargo run -- builder

# Run with YAML config
cargo run -- config config/basic.yaml
```

### Bidirectional Integration
Complete ApplicationSource and ApplicationReaction example:
```bash
# Shows sending data and receiving query results
cd examples/bidirectional
cargo run
```

This example demonstrates:
- Getting source and reaction handles
- Sending node inserts, updates, and deletes
- Receiving query results asynchronously
- Understanding Adding/Updating/Removing semantics

### Configuration Examples
See `examples/configs/` for YAML configuration templates:
- `basic-mock-source/` - Mock source with log reaction
- `multi-source-pipeline/` - Multiple sources feeding one query
- `file-bootstrap-source/` - Using ScriptFile bootstrap provider

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

1. **Enable Batching**: Use adaptive reactions for high-volume scenarios
2. **Async Processing**: Use async reactions for better throughput
3. **Timeout Configuration**: Set appropriate timeouts for reliability

## Testing Patterns

### Unit Testing with Mock Sources

Use mock sources to test query logic without external dependencies:

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};

#[tokio::test]
async fn test_temperature_alert() {
    // Setup: Create server with mock source
    let core = DrasiServerCore::builder()
        .add_source(Source::mock("test-sensors").build())
        .add_query(
            Query::cypher("high-temp")
                .query("MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s.id")
                .from_source("test-sensors")
                .build()
        )
        .add_reaction(
            Reaction::application("test-output")
                .subscribe_to("high-temp")
                .build()
        )
        .build()
        .await
        .unwrap();

    core.start().await.unwrap();

    // Get handles
    let source = core.source_handle("test-sensors").unwrap();
    let reaction = core.reaction_handle("test-output").unwrap();

    // Execute: Send test data
    source.send_node_insert(
        "sensor-1",
        vec!["Sensor"],
        PropertyMapBuilder::new()
            .with_string("id", "sensor-1")
            .with_integer("temperature", 80)
            .build()
    ).await.unwrap();

    // Verify: Check results
    let mut stream = reaction.as_stream().await.unwrap();
    let result = stream.next().await.unwrap();

    assert_eq!(result.query_id, "high-temp");
    assert_eq!(result.results.len(), 1);

    core.stop().await.unwrap();
}
```

### Integration Testing with ScriptFile Bootstrap

Load test data from files:

```rust
#[tokio::test]
async fn test_with_bootstrap_data() {
    let core = DrasiServerCore::builder()
        .add_source(
            Source::mock("test-data")
                .with_bootstrap_script("tests/data/initial.jsonl")
                .build()
        )
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n:TestNode) RETURN n")
                .from_source("test-data")
                .build()
        )
        .build()
        .await
        .unwrap();

    core.start().await.unwrap();

    // Query has initial data from script file
    // Test your logic here

    core.stop().await.unwrap();
}
```

**Script file format (tests/data/initial.jsonl):**
```json
{"type":"header","version":"1.0"}
{"type":"node","id":"n1","labels":["TestNode"],"properties":{"value":42}}
{"type":"node","id":"n2","labels":["TestNode"],"properties":{"value":100}}
```

### Testing Error Conditions

```rust
#[tokio::test]
async fn test_component_not_found() {
    let core = DrasiServerCore::builder().build().await.unwrap();

    // Should return error for non-existent component
    let result = core.source_handle("nonexistent");
    assert!(result.is_err());
}

#[tokio::test]
async fn test_invalid_query() {
    let result = DrasiServerCore::builder()
        .add_source(Source::mock("test").build())
        .add_query(
            Query::cypher("bad-query")
                .query("INVALID CYPHER")
                .from_source("test")
                .build()
        )
        .build()
        .await;

    assert!(result.is_err());
}
```

### Testing Async Behavior

```rust
use tokio::time::{timeout, Duration};

#[tokio::test]
async fn test_result_timing() {
    let core = /* setup */;
    let reaction = core.reaction_handle("test-output").unwrap();
    let mut stream = reaction.as_stream().await.unwrap();

    // Send data
    source.send_node_insert(/*...*/).await.unwrap();

    // Verify result arrives within timeout
    let result = timeout(Duration::from_secs(1), stream.next())
        .await
        .expect("Timeout waiting for result")
        .expect("Stream closed");

    assert!(result.results.len() > 0);
}
```

### Test Fixtures and Helpers

```rust
// tests/common/mod.rs
pub fn create_test_server() -> DrasiServerCore {
    DrasiServerCore::builder()
        .add_source(Source::mock("test").build())
        .build()
        .await
        .unwrap()
}

pub fn create_test_properties() -> ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("test", "value")
        .build()
}

// tests/my_test.rs
mod common;

#[tokio::test]
async fn my_test() {
    let core = common::create_test_server();
    // ...
}
```

## Integration Testing

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
