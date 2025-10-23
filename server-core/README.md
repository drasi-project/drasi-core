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

## Bootstrap System

Before continuous queries can detect changes, they need an initial data snapshot. This is **bootstrap** - loading the starting state so queries know what "changed" means.

### Universal Bootstrap Architecture

**Key Innovation:** DrasiServerCore features a **universal pluggable bootstrap provider system** where ALL sources support configurable bootstrap providers, completely separating bootstrap (initial data delivery) from source streaming logic.

**Architectural Principle**: Bootstrap providers are independent from sources. Any source can use any bootstrap provider, enabling powerful use cases like "bootstrap from PostgreSQL database, stream changes from HTTP endpoint."

### All Sources Support Bootstrap Providers

- **PostgresReplicationSource**: ✅ Delegates to configured provider
- **HttpSource**: ✅ Delegates to configured provider
- **GrpcSource**: ✅ Delegates to configured provider
- **MockSource**: ✅ Delegates to configured provider
- **PlatformSource**: ✅ Delegates to configured provider
- **ApplicationSource**: ✅ Delegates to configured provider (falls back to internal if no provider configured)

### Bootstrap Provider Types

**NoOp Provider** (default): No initial data
```rust
Source::mock("test").build()  // No bootstrap
```

**ScriptFile Provider**: Load from JSONL files (perfect for testing and development)
```rust
Source::mock("test")
    .with_bootstrap_script("data/initial.jsonl")
    .build()
```

**Script File Format**: JSONL (JSON Lines) with record types:
- **Header** (required first): `{"type":"header","version":"1.0"}`
- **Node**: `{"type":"node","id":"n1","labels":["Person"],"properties":{"name":"Alice"}}`
- **Relation**: `{"type":"relation","id":"r1","startNode":"n1","endNode":"n2","type":"KNOWS","properties":{}}`
- **Comment** (filtered): Lines starting with `#`
- **Label** (checkpoint): For restart support
- **Finish** (optional): Marks end

Supports multi-file reading in sequence.

**PostgreSQL Provider**: Snapshot of current database state
```rust
Source::postgres("db")
    .with_postgres_bootstrap()  // Snapshot at startup
    .build()
```

**Application Provider**: Replay stored events
```rust
Source::application("app")
    .with_application_bootstrap()
    .build()
```

**Platform Provider**: Load from remote Drasi Query API
```rust
Source::platform("external")
    .with_platform_bootstrap("http://api:8080", Some(300))
    .build()
```

### Mix-and-Match Bootstrap Configuration

**Standard Configuration** (source and provider match):
```yaml
sources:
  - id: my_postgres_source
    source_type: postgres
    bootstrap_provider:
      type: postgres
    properties:
      host: localhost
      database: mydb
```

**Mix-and-Match Configuration** (any source with any provider):
```yaml
sources:
  # HTTP source with PostgreSQL bootstrap - bootstrap 1M records from DB, stream changes via HTTP
  - id: http_with_postgres_bootstrap
    source_type: http
    bootstrap_provider:
      type: postgres  # Bootstrap from PostgreSQL
    properties:
      host: localhost
      port: 9000
      database: mydb  # Used by postgres bootstrap provider
      user: dbuser
      password: dbpass

  # gRPC source with ScriptFile bootstrap - load test data from file, stream changes via gRPC
  - id: grpc_with_file_bootstrap
    source_type: grpc
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/path/to/initial_data.jsonl"
    properties:
      endpoint: "0.0.0.0:50051"

  # Mock source with ScriptFile bootstrap - for testing
  - id: test_source
    source_type: mock
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/path/to/test_data.jsonl"
    properties:
      interval_ms: 1000
```

### When Bootstrap Happens

Bootstrap runs automatically when you call `core.start()`:
1. Sources bootstrap first (load initial data)
2. Queries receive bootstrap data and build initial result sets
3. Streaming begins - sources emit live changes

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

**HTTP Source** - Accept changes via REST API for webhook-based integrations with adaptive batching.

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

### Example: Application Source

Programmatically inject changes:

```rust
let core = DrasiServerCore::builder()
    .add_source(Source::application("app-source").build())
    .build()
    .await?;

core.start().await?;

// Get handle for programmatic injection
let source = core.source_handle("app-source")?;

// Insert node
source.send_node_insert(
    "customer-123",
    vec!["Customer"],
    PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .with_string("email", "alice@example.com")
        .build()
).await?;

// Update node
source.send_node_update(
    "customer-123",
    vec!["Customer"],
    PropertyMapBuilder::new()
        .with_string("name", "Alice Smith")
        .with_string("email", "alice@example.com")
        .build()
).await?;

// Delete node
source.send_node_delete("customer-123", vec!["Customer"]).await?;
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

### Multi-Source Queries and Joins

DrasiServerCore supports querying across multiple sources and creating **synthetic joins** - relationships between data from different sources based on matching properties.

#### Multi-Source Queries

Subscribe to multiple sources:

```rust
Query::cypher("multi-source-query")
    .query("MATCH (n) RETURN n")
    .from_source("source1")
    .from_source("source2")
    .from_source("source3")
    .build()
```

Or with array:
```rust
Query::cypher("multi-source-query")
    .query("MATCH (n) RETURN n")
    .from_sources(vec!["source1", "source2", "source3"])
    .build()
```

#### Synthetic Joins

Create relationships between nodes from different sources based on property values:

```rust
use drasi_server_core::config::{QueryJoinConfig, QueryJoinKeyConfig};

let join_config = QueryJoinConfig {
    id: "DRIVES".to_string(),  // Relationship type in query
    keys: vec![
        QueryJoinKeyConfig {
            label: "Driver".to_string(),
            property: "vehicle_id".to_string(),
        },
        QueryJoinKeyConfig {
            label: "Vehicle".to_string(),
            property: "id".to_string(),
        },
    ],
};

Query::cypher("driver-vehicle-query")
    .query(r#"
        MATCH (d:Driver)-[:DRIVES]->(v:Vehicle)
        WHERE v.status = 'available'
        RETURN d.name, v.license_plate
    "#)
    .from_source("driver-database")
    .from_source("vehicle-database")
    .with_join(join_config)
    .build()
```

**How Joins Work:**
- Join creates synthetic `[:DRIVES]` relationships
- When `Driver.vehicle_id` matches `Vehicle.id`, a relationship is created
- Query can then use this relationship in MATCH patterns
- Joins update automatically as data changes

**Multiple Joins Example:**
```rust
// Join customers to orders, orders to products
let customer_order_join = QueryJoinConfig {
    id: "PLACED".to_string(),
    keys: vec![
        QueryJoinKeyConfig {
            label: "Customer".to_string(),
            property: "customer_id".to_string(),
        },
        QueryJoinKeyConfig {
            label: "Order".to_string(),
            property: "customer_id".to_string(),
        },
    ],
};

let order_product_join = QueryJoinConfig {
    id: "CONTAINS".to_string(),
    keys: vec![
        QueryJoinKeyConfig {
            label: "Order".to_string(),
            property: "product_id".to_string(),
        },
        QueryJoinKeyConfig {
            label: "Product".to_string(),
            property: "product_id".to_string(),
        },
    ],
};

Query::cypher("customer-orders-query")
    .query(r#"
        MATCH (c:Customer)-[:PLACED]->(o:Order)-[:CONTAINS]->(p:Product)
        WHERE o.status = 'pending'
        RETURN c.name, o.id, p.name, o.total
    "#)
    .from_sources(vec!["customers", "orders", "products"])
    .with_joins(vec![customer_order_join, order_product_join])
    .build()
```

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

### Query Limitations

DrasiServerCore continuous queries don't support certain stateful Cypher operations:

**NOT SUPPORTED:**
- ORDER BY
- LIMIT/SKIP
- TOP

**Why?** Continuous queries emit diffs. Ordering is a stateful operation that requires the full result set. With continuous queries, you receive changes as they happen, not the complete ordered set.

**Workaround - Client-side ranking:**
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

### Query Configuration Options

```rust
Query::cypher("my-query")
    .query("MATCH (n) RETURN n")
    .from_source("source1")
    .auto_start(true)  // Auto-start on server start (default: true)
    .with_property("priority_queue_capacity", json!(50000))  // Optional override
    .build()
```

**Bootstrap Configuration:**
- Bootstrap is enabled by default for queries
- Initial data loaded before streaming begins
- Configurable via source bootstrap provider

**Priority Queue Configuration:**
- Each query has an internal priority queue for ordering events by timestamp
- Uses **three-level hierarchy**: component override → global default → hardcoded 10,000
- Configure globally via `server_core.priority_queue_capacity` in config
- Override per-query for high-volume scenarios
- Larger capacities handle burst traffic better but use more memory
- See [Priority Queue Configuration](#priority-queue-configuration) and [Priority Queue Tuning](#priority-queue-tuning) sections for complete details

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

### Adaptive Reactions

Adaptive reactions (HTTP and gRPC) feature **dynamic batch size optimization** for high-throughput scenarios:

**Key Features:**
- Monitors throughput and latency in real-time
- Dynamically adjusts batch size to meet target latency
- Uses Welford's algorithm for online variance calculation
- Priority queue for maintaining change order
- Connection pooling for efficiency

**Configuration:**
```rust
Reaction::http("adaptive-webhook")
    .subscribe_to("high-volume-query")
    .with_properties(
        Properties::new()
            .with_string("base_url", "http://api.example.com")
            .with_bool("batch_endpoints_enabled", true)
            .with_int("adaptive_max_batch_size", 100)
            .with_int("adaptive_min_batch_size", 1)
            .with_int("adaptive_target_latency_ms", 100)
            .with_float("adaptive_adjustment_factor", 0.1)
    )
    .build()
```

**How It Works:**
1. Reaction measures actual throughput and latency
2. Compares to target latency (default 100ms)
3. Adjusts batch size up or down dynamically
4. Optimizes for maximum throughput within latency budget

**Use Case:** Processing 10,000+ changes/second while maintaining sub-100ms latency.

### Example: HTTP Reaction

Trigger webhooks when changes are detected:

```rust
Reaction::http("alert-webhook")
    .subscribe_to("anomaly-detection")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "https://api.example.com/alerts")
            .with_string("method", "POST")
            .with_string("token", "Bearer secret-token")
            .with_int("timeout_ms", 5000)
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

## Performance Profiling

DrasiServerCore includes built-in profiling capabilities to track end-to-end latency through the Source → Query → Reaction pipeline.

### Profiler Reaction

The Profiler reaction provides comprehensive performance monitoring:

**9 Timestamp Capture Points:**
1. Source send
2. Query receive
3. Query core call
4. Query core return
5. Query send
6. Reaction receive
7. Reaction complete
8-9. Additional markers

**5 Key Latency Metrics:**
1. **Source → Query**: Time from source emit to query receive
2. **Query Processing**: Time spent in query evaluation
3. **Query → Reaction**: Time from query emit to reaction receive
4. **Reaction Processing**: Time spent in reaction
5. **Total End-to-End**: Complete pipeline latency

**Statistical Analysis:**
- Mean, variance, standard deviation
- Min/max values
- Percentiles: p50 (median), p95, p99
- Welford's algorithm for online variance
- Sliding window for accurate percentile calculation

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
        Reaction::custom("profiler", "profiler")
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

### Configuration

**Profiler Reaction Properties:**
- `window_size` - Sample window size for percentile calculation (default: 1000)
- `report_interval_secs` - How often to log statistics (default: 60)

**Source Configuration:**
- Set `enable_profiling: true` in source properties
- Adds ~100ns overhead per event

### Use Cases

- **Performance Tuning**: Identify bottlenecks in your pipeline
- **SLO Monitoring**: Set Service Level Objectives based on p95/p99 latency
- **Capacity Planning**: Understand throughput vs. latency tradeoffs
- **Regression Detection**: Monitor for performance degradation over time

### Best Practices

1. **Monitor p99**: The 99th percentile reveals tail latency issues
2. **Compare Metrics**: Identify bottlenecks by comparing latencies across pipeline stages
3. **Enable Selectively**: Profiling adds overhead - enable only when needed
4. **Sliding Window**: Use appropriate window size for your workload (larger for stable metrics, smaller for rapid changes)

For comprehensive profiling documentation, see:
- **[Profiler Reaction README](src/reactions/profiler/README.md)** - Complete profiler reaction documentation
- **[PROFILING.md](docs/PROFILING.md)** - Overall profiling system architecture

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

// Get complete configuration snapshot
let config = core.get_current_config().await?;
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
// Note: Remove in dependency order (reactions → queries → sources)
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
server_core:
  id: infrastructure-monitor
  priority_queue_capacity: 50000  # Global default for all queries and reactions

sources:
  - id: infrastructure-metrics
    source_type: postgres
    auto_start: true
    bootstrap_provider:
      type: postgres
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
    priority_queue_capacity: 100000  # Override for high-volume query

reactions:
  - id: ops-alerts
    reaction_type: http
    queries:
      - vm-health-monitor
    auto_start: true
    priority_queue_capacity: 75000  # Override for this reaction
    properties:
      endpoint: https://ops.example.com/alerts
      method: POST
```

#### Priority Queue Configuration

Priority queues maintain event ordering by timestamp before processing. DrasiServerCore uses a **three-level configuration hierarchy** that allows fine-grained control while providing sensible defaults:

**Level 1: Component-Specific Override** (highest priority)
```yaml
queries:
  - id: high-volume-query
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
    priority_queue_capacity: 100000  # This query uses 100,000

reactions:
  - id: critical-reaction
    reaction_type: log
    queries: ["high-volume-query"]
    priority_queue_capacity: 150000  # This reaction uses 150,000
```

**Level 2: Global Server Default**
```yaml
server_core:
  id: my-server
  priority_queue_capacity: 50000  # All queries/reactions without overrides use 50,000
```

**Level 3: Hardcoded Fallback** (lowest priority)
- If neither component nor global capacity is specified, the hardcoded default of **10,000** is used

**How the Hierarchy Works:**

The system resolves capacity using this precedence order:
1. If a query/reaction specifies `priority_queue_capacity` → use that value
2. Else if server_core specifies `priority_queue_capacity` → use that value
3. Else → use hardcoded default of 10,000

**Example with Mixed Configuration:**
```yaml
server_core:
  priority_queue_capacity: 30000  # Global default

queries:
  - id: query1
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
    priority_queue_capacity: 75000  # Uses 75,000 (component override)

  - id: query2
    query: "MATCH (m) RETURN m"
    sources: ["source1"]
    # No override → uses 30,000 (global default)

reactions:
  - id: reaction1
    reaction_type: log
    queries: ["query1"]
    priority_queue_capacity: 100000  # Uses 100,000 (component override)

  - id: reaction2
    reaction_type: log
    queries: ["query2"]
    # No override → uses 30,000 (global default)
```

**Builder API:**
```rust
DrasiServerCore::builder()
    // Component overrides take precedence
    .add_query(
        Query::cypher("high-volume")
            .query("MATCH (n) RETURN n")
            .from_source("source1")
            .with_property("priority_queue_capacity", json!(100000))
            .build()
    )
    .build()
    .await?
```

**When to Adjust:**
- **Increase** for burst traffic scenarios (e.g., 100,000+ for data migrations)
- **Decrease** for memory-constrained environments (e.g., 1,000 for IoT devices)
- **Default (10,000)** works well for most production workloads
- **Use global setting** to set a consistent baseline across all components
- **Use component overrides** for specific high-volume or critical paths

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

The API provides typed errors for better error handling:

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

## Testing Patterns

### Unit Testing with Mock Sources

Use mock sources to test query logic without external dependencies:

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, PropertyMapBuilder};

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

### Testing Multi-Source Joins

Test synthetic joins across multiple sources:

```rust
use drasi_server_core::config::{QueryJoinConfig, QueryJoinKeyConfig};

#[tokio::test]
async fn test_three_way_join() {
    let join1 = QueryJoinConfig {
        id: "CUSTOMER_ORDER".to_string(),
        keys: vec![
            QueryJoinKeyConfig {
                label: "Customer".to_string(),
                property: "customer_id".to_string(),
            },
            QueryJoinKeyConfig {
                label: "Order".to_string(),
                property: "customer_id".to_string(),
            },
        ],
    };

    let join2 = QueryJoinConfig {
        id: "ORDER_PRODUCT".to_string(),
        keys: vec![
            QueryJoinKeyConfig {
                label: "Order".to_string(),
                property: "product_id".to_string(),
            },
            QueryJoinKeyConfig {
                label: "Product".to_string(),
                property: "product_id".to_string(),
            },
        ],
    };

    let core = DrasiServerCore::builder()
        .add_source(Source::application("customers").build())
        .add_source(Source::application("orders").build())
        .add_source(Source::application("products").build())
        .add_query(
            Query::cypher("join-query")
                .query(r#"
                    MATCH (c:Customer)-[:CUSTOMER_ORDER]->(o:Order)-[:ORDER_PRODUCT]->(p:Product)
                    RETURN c.customer_id, o.order_id, p.product_name
                "#)
                .from_sources(vec!["customers", "orders", "products"])
                .with_joins(vec![join1, join2])
                .build()
        )
        .add_reaction(
            Reaction::application("results")
                .subscribe_to("join-query")
                .build()
        )
        .build()
        .await
        .unwrap();

    core.start().await.unwrap();

    // Inject data and verify joins
    // See tests/multi_source_join_test.rs for complete example

    core.stop().await.unwrap();
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

## Performance Optimization

### Query Optimization

1. **Use Specific Patterns**: Be precise in your MATCH clauses
2. **Filter Early**: Apply WHERE conditions as early as possible
3. **Index Properties**: Ensure frequently queried properties are indexed

### Source Tuning

1. **Batch Changes**: Group related changes together
2. **Buffer Sizing**: Configure appropriate buffer sizes for throughput
3. **Connection Pooling**: Use connection pools for database sources

### Priority Queue Tuning

Priority queues maintain timestamp-ordered event processing. DrasiServerCore uses a **three-level hierarchy** (component override → global default → hardcoded 10,000) for configuring capacity. See the [Priority Queue Configuration](#priority-queue-configuration) section for complete details.

**Symptoms of Undersized Queues:**
- Events dropped due to capacity (check metrics)
- Warning logs about queue capacity reached
- Missing data in query results during bursts

**Sizing Guidelines:**

| Scenario | Recommended Capacity | Configuration Level |
|----------|---------------------|---------------------|
| Steady-state production | 10,000 (default) | No configuration needed |
| High-throughput streaming | 50,000 - 100,000 | Global default |
| Bulk data migration | 100,000 - 1,000,000 | Per-query override |
| IoT / Edge devices | 1,000 - 5,000 | Global default |
| Testing / Development | 1,000 - 5,000 | Global default |

**Configuration Example (Using Hierarchy):**
```yaml
server_core:
  priority_queue_capacity: 50000  # Global default for all components

queries:
  - id: steady-state-query
    query: "MATCH (n:Regular) RETURN n"
    sources: ["standard"]
    # No override → uses 50,000 (global default)

  - id: data-migration-query
    query: "MATCH (n) RETURN n"
    sources: ["bulk-import"]
    priority_queue_capacity: 1000000  # Temporary override for migration

reactions:
  - id: standard-webhook
    reaction_type: http
    queries: ["steady-state-query"]
    # No override → uses 50,000 (global default)

  - id: critical-alerts
    reaction_type: http
    queries: ["high-priority-query"]
    priority_queue_capacity: 100000  # Override for critical path
```

**Memory Considerations:**
- Each event in queue: ~1-2 KB (varies by payload)
- 10,000 capacity ≈ 10-20 MB per component
- 100,000 capacity ≈ 100-200 MB per component
- Monitor actual memory usage with your data model

**Best Practices:**
- Set a **global default** that works for most of your workload
- Use **component overrides** only for outliers (very high volume or critical paths)
- Start with defaults and adjust based on observed metrics
- Temporarily increase capacity for known burst scenarios (migrations, batch imports)

### Reaction Optimization

1. **Enable Batching**: Use adaptive reactions for high-volume scenarios
2. **Async Processing**: Use async reactions for better throughput
3. **Timeout Configuration**: Set appropriate timeouts for reliability

### Profiling-Driven Optimization

1. **Enable Profiler**: Add Profiler reaction to identify bottlenecks
2. **Monitor p95/p99**: Focus on tail latency for SLO compliance
3. **Compare Stages**: Identify which pipeline stage is slowest
4. **Check Queue Metrics**: Monitor priority queue drops and depth
5. **Iterate**: Adjust configuration and measure impact

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

## Integration Testing

Run the test suite:

```bash
# Run all tests
cargo test

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run specific test
cargo test test_builder_api

# Run specific test module
cargo test --test multi_source_join_test
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

**Join Not Working**:
- Verify join key properties exist on both node types
- Check that labels match exactly
- Ensure sources are providing data

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
