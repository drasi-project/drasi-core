# DrasiServerCore Developer Guide

## Table of Contents

1. [Introduction](#introduction)
2. [What is DrasiServerCore?](#what-is-drasiServercore)
3. [Core Concepts](#core-concepts)
4. [Architecture Overview](#architecture-overview)
5. [Getting Started](#getting-started)
6. [Building Change-Driven Solutions](#building-change-driven-solutions)
7. [Sources - Data Ingestion](#sources---data-ingestion)
8. [Queries - Change Detection](#queries---change-detection)
9. [Reactions - Response Actions](#reactions---response-actions)
10. [Bootstrap System](#bootstrap-system)
11. [Data Flow and Transformations](#data-flow-and-transformations)
12. [Advanced Topics](#advanced-topics)
13. [API Reference](#api-reference)
14. [Best Practices](#best-practices)
15. [Troubleshooting](#troubleshooting)

## Introduction

This guide helps developers understand and use DrasiServerCore to build change-driven solutions. DrasiServerCore is a Rust library that enables you to detect and react to precise data changes across your systems with minimal complexity.

### Target Audience

- Rust developers building reactive applications
- System architects designing event-driven architectures
- Engineers working with change data capture (CDC)
- Teams building real-time monitoring and alerting systems

### Prerequisites

- Rust 1.75 or later
- Basic understanding of async Rust and Tokio
- Familiarity with graph concepts (nodes and relationships)
- Knowledge of Cypher query language (helpful but not required)

## What is DrasiServerCore?

DrasiServerCore is a library for building **change-driven solutions** - applications that automatically detect and respond to meaningful data changes in real-time. Unlike traditional event-driven systems that process streams of generic events, DrasiServerCore uses continuous queries to maintain live result sets that update automatically as your data changes.

### Key Differentiators

| Traditional Event Systems | DrasiServerCore |
|--------------------------|-----------------|
| Process all events, filter later | Query-first: only process relevant changes |
| Complex state management | Automatic state tracking via continuous queries |
| Hard-coded change detection | Declarative change detection with Cypher |
| Event parsing and transformation | Graph model with semantic relationships |
| Manual correlation across events | Built-in join support across sources |

### When to Use DrasiServerCore

✅ **Perfect for:**
- Real-time monitoring and alerting
- Business exception detection
- Infrastructure health tracking
- Compliance and audit trails
- Change data capture (CDC) pipelines
- IoT sensor monitoring
- Resource optimization

❌ **Not ideal for:**
- Simple CRUD applications
- Batch processing workloads
- Static data analysis
- Applications without real-time requirements

## Core Concepts

### The Source → Query → Reaction Pattern

```
┌─────────────┐      ┌─────────────────┐      ┌──────────────┐
│   Sources   │ ───> │ Continuous      │ ───> │  Reactions   │
│             │      │ Queries         │      │              │
├─────────────┤      ├─────────────────┤      ├──────────────┤
│ • PostgreSQL│      │ • Cypher-based  │      │ • Webhooks   │
│ • HTTP API  │      │ • Live results  │      │ • gRPC       │
│ • gRPC      │      │ • Incremental   │      │ • SSE        │
│ • In-app    │      │ • Multi-source  │      │ • In-app     │
└─────────────┘      └─────────────────┘      └──────────────┘
     ↓                      ↓                        ↓
  Change Events        Change Detection        Change Actions
```

### Graph Data Model

DrasiServerCore uses a labeled property graph model:

```
Node Example:
(:Server {id: "srv-01", cpu: 85, memory: 4096})
     ↑        ↑
   Label   Properties

Relationship Example:
(:Application)-[:RUNS_ON]->(:Server)
                    ↑
              Relationship Type
```

This model naturally represents:
- **Entities**: Nodes with labels and properties
- **Relationships**: Connections providing context
- **Changes**: Insert, Update, Delete operations

### Change Semantics

DrasiServerCore tracks three types of changes in query results:

```rust
pub enum QueryResult {
    Adding { after: HashMap<String, Value> },      // New result row
    Updating { before: HashMap<String, Value>,     // Changed result
               after: HashMap<String, Value> },
    Removing { before: HashMap<String, Value> },   // Removed result
}
```

## Architecture Overview

### Component Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    DrasiServerCore                       │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │   Sources    │  │   Queries    │  │  Reactions   │  │
│  ├──────────────┤  ├──────────────┤  ├──────────────┤  │
│  │ PostgreSQL   │  │ Continuous   │  │ HTTP         │  │
│  │ HTTP         │  │ Query Engine │  │ gRPC         │  │
│  │ gRPC         │  │              │  │ SSE          │  │
│  │ Application  │  │ Multi-Source │  │ Application  │  │
│  │ Platform     │  │ Joins        │  │ Log          │  │
│  │ Mock         │  │              │  │ Profiler     │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  │
│         ↓                 ↓                 ↓           │
│  ┌──────────────────────────────────────────────────┐  │
│  │              Event Routing Layer                 │  │
│  ├──────────────────────────────────────────────────┤  │
│  │ • DataRouter: Source → Query routing             │  │
│  │ • SubscriptionRouter: Query → Reaction routing   │  │
│  │ • BootstrapRouter: Initial data delivery         │  │
│  │ • Broadcast channels for fan-out                 │  │
│  └──────────────────────────────────────────────────┘  │
│                                                          │
│  ┌──────────────────────────────────────────────────┐  │
│  │            Bootstrap Provider System             │  │
│  ├──────────────────────────────────────────────────┤  │
│  │ • PostgreSQL: Database snapshots                 │  │
│  │ • ScriptFile: JSONL test data                   │  │
│  │ • Application: Stored events                    │  │
│  │ • Platform: Remote Drasi Query API              │  │
│  │ • NoOp: No initial data                         │  │
│  └──────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

### Component Lifecycle

```
┌────────┐     ┌────────────┐     ┌─────────┐     ┌─────────┐
│ Create │ ──> │ Initialize │ ──> │  Start  │ ──> │ Running │
└────────┘     └────────────┘     └─────────┘     └─────────┘
                      │                                  ↓
               ┌──────────────┐                   ┌─────────┐
               │  Bootstrap   │                   │  Stop   │
               │  (if needed) │                   └─────────┘
               └──────────────┘                         ↓
                                                  ┌─────────┐
                                                  │ Stopped │
                                                  └─────────┘
```

## Getting Started

### Installation

Add DrasiServerCore to your `Cargo.toml`:

```toml
[dependencies]
drasi-server-core = "0.1.0"
tokio = { version = "1", features = ["full"] }
serde_json = "1.0"
anyhow = "1.0"
```

### Your First Change-Driven Solution

Here's a complete example that monitors temperature sensors:

```rust
use drasi_server_core::{DrasiServerCore, Source, Query, Reaction, Properties};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Build the server configuration
    let core = DrasiServerCore::builder()
        .with_id("temperature-monitor")

        // Add a mock source for testing
        .add_source(
            Source::mock("sensors")
                .with_properties(
                    Properties::new()
                        .with_string("data_type", "sensor")
                        .with_int("interval_ms", 2000)
                )
                .build()
        )

        // Add a query to detect high temperatures
        .add_query(
            Query::cypher("high-temp-alert")
                .query(r#"
                    MATCH (s:Sensor)
                    WHERE s.temperature > 75
                    RETURN s.id, s.location, s.temperature
                "#)
                .from_source("sensors")
                .build()
        )

        // Add a reaction to log alerts
        .add_reaction(
            Reaction::log("alert-logger")
                .subscribe_to("high-temp-alert")
                .with_property("log_level", json!("warn"))
                .build()
        )

        .build()
        .await?;

    // Start processing
    core.start().await?;

    println!("Monitoring temperatures... Press Ctrl+C to stop");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;

    // Graceful shutdown
    core.stop().await?;

    Ok(())
}
```

### Understanding the Example

1. **Source**: Generates mock sensor data every 2 seconds
2. **Query**: Continuously monitors for sensors with temperature > 75
3. **Reaction**: Logs alerts when high temperatures are detected
4. **Change Detection**: Automatic - you only see changes when:
   - A sensor crosses the 75° threshold (Adding)
   - A high-temp sensor's reading changes (Updating)
   - A sensor drops below 75° (Removing)

## Building Change-Driven Solutions

### Configuration Approaches

DrasiServerCore offers three ways to configure your solution:

#### 1. Fluent Builder API (Recommended)

Type-safe, ergonomic configuration in code:

```rust
let core = DrasiServerCore::builder()
    .with_id("my-server")
    .add_source(Source::postgres("db").build())
    .add_query(Query::cypher("q1").build())
    .add_reaction(Reaction::http("webhook").build())
    .build()
    .await?;
```

#### 2. YAML Configuration

Declarative configuration in files:

```yaml
server_core:
  id: my-server

sources:
  - id: db
    source_type: postgres
    properties:
      host: localhost
      database: mydb

queries:
  - id: q1
    query: "MATCH (n) RETURN n"
    sources: [db]

reactions:
  - id: webhook
    reaction_type: http
    queries: [q1]
    properties:
      endpoint: https://api.example.com
```

Load with:
```rust
let core = DrasiServerCore::from_config_file("config.yaml").await?;
```

#### 3. Dynamic Runtime Management

Add/remove components while running:

```rust
// Add a new source at runtime
core.add_source_runtime(
    Source::http("new-api")
        .with_property("port", json!(9000))
        .auto_start(true)
        .build()
).await?;

// Later, remove it
core.remove_source("new-api").await?;
```

### Design Patterns

#### Pattern 1: Multi-Stage Processing

Chain queries for complex logic:

```rust
// Stage 1: Detect anomalies
Query::cypher("anomalies")
    .query("MATCH (s:Sensor) WHERE s.value > s.threshold RETURN s")
    .from_source("sensors")

// Stage 2: Correlate with location
Query::cypher("location-alerts")
    .query(r#"
        MATCH (s:Sensor)-[:LOCATED_IN]->(l:Location)
        WHERE s.anomaly = true AND l.critical = true
        RETURN s, l
    "#)
    .from_source("anomalies")  // Chain from previous query
```

#### Pattern 2: Fan-Out Processing

Multiple reactions for different purposes:

```rust
let core = DrasiServerCore::builder()
    .add_query(Query::cypher("changes").build())

    // Fan out to multiple reactions
    .add_reaction(
        Reaction::http("webhook")
            .subscribe_to("changes")
            .build()
    )
    .add_reaction(
        Reaction::application("in-app")
            .subscribe_to("changes")
            .build()
    )
    .add_reaction(
        Reaction::log("audit")
            .subscribe_to("changes")
            .build()
    )
    .build()
    .await?;
```

#### Pattern 3: Programmatic Data Injection

Use ApplicationSource for testing or integration:

```rust
let core = DrasiServerCore::builder()
    .add_source(Source::application("app").build())
    .build()
    .await?;

core.start().await?;

// Get handle for injection
let source = core.source_handle("app")?;

// Inject test data
source.send_node_insert(
    "test-1",
    vec!["TestNode"],
    PropertyMapBuilder::new()
        .with_string("name", "Test")
        .with_integer("value", 42)
        .build()
).await?;
```

## Sources - Data Ingestion

Sources adapt external data into DrasiServerCore's graph model. Each source handles:
- **Bootstrap**: Loading initial data snapshot
- **Streaming**: Emitting live changes
- **Transformation**: Converting to graph elements

### Available Sources

| Source | Use Case | Bootstrap Support | Key Features |
|--------|----------|-------------------|--------------|
| PostgreSQL | Database CDC | ✅ Native + All | Logical replication, WAL decoding |
| HTTP | Webhooks/REST | ✅ All providers | Adaptive batching, async handling |
| gRPC | High-throughput | ✅ All providers | Bidirectional streaming, protobuf |
| Application | In-process | ✅ Internal + All | Direct API, testing |
| Platform | Drasi integration | ✅ All providers | Redis Streams, horizontal scaling |
| Mock | Testing/Demo | ✅ All providers | Configurable data generation |

### PostgreSQL Source

Real-time change data capture from PostgreSQL:

```rust
Source::postgres("orders-db")
    .with_properties(
        Properties::new()
            .with_string("host", "db.example.com")
            .with_int("port", 5432)
            .with_string("database", "orders")
            .with_string("user", "drasi")
            .with_string("password", "secret")
            .with_array("tables", json!(["orders", "customers"]))
            .with_object("table_keys", json!({
                "orders": {"key_columns": ["order_id"]},
                "customers": {"key_columns": ["customer_id"]}
            }))
    )
    .with_postgres_bootstrap()  // Snapshot existing data
    .build()
```

**Database Setup:**
```sql
-- Enable logical replication
ALTER SYSTEM SET wal_level = logical;

-- Create publication
CREATE PUBLICATION drasi_pub FOR TABLE orders, customers;

-- Grant replication
ALTER USER drasi REPLICATION;
```

### HTTP Source

Accept changes via REST API:

```rust
Source::http("webhook-receiver")
    .with_properties(
        Properties::new()
            .with_string("host", "0.0.0.0")
            .with_int("port", 9000)
            .with_string("path", "/changes")
            .with_bool("adaptive_batching", true)
            .with_int("max_batch_size", 100)
    )
    .build()
```

**Send changes:**
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
        "total": 99.99,
        "status": "pending"
      }
    }
  }'
```

### Application Source

Direct programmatic injection:

```rust
// Setup
let core = DrasiServerCore::builder()
    .add_source(Source::application("app-events").build())
    .build()
    .await?;

core.start().await?;

// Get handle
let source = core.source_handle("app-events")?;

// Insert node
source.send_node_insert(
    "customer-1",
    vec!["Customer"],
    PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .with_string("tier", "gold")
        .build()
).await?;

// Update node
source.send_node_update(
    "customer-1",
    vec!["Customer"],
    PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .with_string("tier", "platinum")  // Changed
        .build()
).await?;

// Insert relationship
source.send_relationship_insert(
    "rel-1",
    "customer-1",
    "order-1",
    vec!["PLACED"],
    PropertyMapBuilder::new()
        .with_string("date", "2024-01-01")
        .build()
).await?;

// Delete node
source.send_node_delete("customer-1", vec!["Customer"]).await?;
```

### Source Configuration Reference

All sources support:
- `auto_start`: Whether to start automatically (default: true)
- `bootstrap_provider`: Initial data configuration
- `enable_profiling`: Performance tracking (default: false)
- `dispatch_buffer_capacity`: Event buffer size

## Queries - Change Detection

Queries are the intelligence layer - they define what changes matter to your application using Cypher.

### Cypher Query Basics

```cypher
-- Pattern matching
MATCH (n:Label)-[r:TYPE]->(m:Label2)

-- Filtering
WHERE n.property > 100

-- Returning results
RETURN n.id, m.name, r.weight
```

### Query Examples

#### Simple Property Filtering

```rust
Query::cypher("high-cpu-vms")
    .query(r#"
        MATCH (vm:VM)
        WHERE vm.cpu_usage > 80
        RETURN vm.id, vm.name, vm.cpu_usage
    "#)
    .from_source("infrastructure")
    .build()
```

#### Relationship Patterns

```rust
Query::cypher("critical-dependencies")
    .query(r#"
        MATCH (app:Application)-[:DEPENDS_ON]->(db:Database)
        WHERE app.tier = 'production' AND db.status = 'down'
        RETURN app.name, db.name, db.error_message
    "#)
    .from_source("service-mesh")
    .build()
```

#### Temporal Conditions

```rust
Query::cypher("long-running-tasks")
    .query(r#"
        MATCH (t:Task)
        WHERE t.status = 'running'
          AND duration.between(t.started_at, datetime()) > duration('PT1H')
        RETURN t.id, t.name, t.started_at
    "#)
    .from_source("task-queue")
    .build()
```

#### Aggregations

```rust
Query::cypher("order-totals")
    .query(r#"
        MATCH (c:Customer)-[:PLACED]->(o:Order)
        WHERE o.created_at > datetime('2024-01-01')
        RETURN c.id, c.name, count(o) as order_count, sum(o.total) as total_spent
    "#)
    .from_source("sales-db")
    .build()
```

### Multi-Source Queries and Joins

Query across multiple data sources with synthetic joins:

```rust
// Define how to join data from different sources
let join = QueryJoinConfig {
    id: "ASSIGNED_TO".to_string(),
    keys: vec![
        QueryJoinKeyConfig {
            label: "Ticket".to_string(),
            property: "assignee_id".to_string(),
        },
        QueryJoinKeyConfig {
            label: "Employee".to_string(),
            property: "employee_id".to_string(),
        },
    ],
};

Query::cypher("ticket-assignments")
    .query(r#"
        MATCH (t:Ticket)-[:ASSIGNED_TO]->(e:Employee)
        WHERE t.priority = 'high' AND e.available = true
        RETURN t.id, t.title, e.name, e.skills
    "#)
    .from_sources(vec!["ticketing-system", "hr-database"])
    .with_join(join)
    .build()
```

### Understanding Query Results

Results arrive as diffs with semantic meaning:

```rust
// Process results from ApplicationReaction
let mut stream = reaction.as_stream().await.unwrap();

while let Some(result) = stream.next().await {
    match result.results[0] {
        QueryResult::Adding { after } => {
            // New row matches query
            println!("New match: {:?}", after);
        }
        QueryResult::Updating { before, after } => {
            // Existing row changed
            println!("Changed from {:?} to {:?}", before, after);
        }
        QueryResult::Removing { before } => {
            // Row no longer matches
            println!("No longer matches: {:?}", before);
        }
    }
}
```

### Query Limitations

Continuous queries don't support:
- `ORDER BY` - Results arrive as changes occur
- `LIMIT/SKIP` - All matching changes are emitted
- `TOP` - Maintain top-N client-side

**Workaround for Top-N:**
```rust
// Client-side top-10 tracking
let mut items = BTreeMap::new();

while let Some(result) = stream.next().await {
    // Update local state
    update_items(&mut items, result);

    // Get top 10
    let top_10: Vec<_> = items
        .iter()
        .sorted_by_key(|(_, v)| v.score)
        .take(10)
        .collect();
}
```

## Reactions - Response Actions

Reactions respond to detected changes by triggering actions.

### Available Reactions

| Reaction | Use Case | Batching | Key Features |
|----------|----------|----------|--------------|
| HTTP | Webhooks, REST APIs | ✅ Adaptive | Connection pooling, retries |
| gRPC | High-throughput streaming | ✅ Adaptive | Bidirectional, protobuf |
| SSE | Browser push | ❌ | Real-time web updates |
| Application | In-process handling | ❌ | Direct API, async streams |
| Log | Debugging, audit | ❌ | Configurable levels |
| Platform | Drasi integration | ❌ | Redis Streams output |
| Profiler | Performance analysis | ❌ | Latency metrics, percentiles |

### HTTP Reaction

Trigger webhooks with detected changes:

```rust
Reaction::http("order-webhook")
    .subscribe_to("delayed-orders")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "https://api.example.com/alerts")
            .with_string("method", "POST")
            .with_object("headers", json!({
                "Authorization": "Bearer token",
                "X-Source": "drasi"
            }))
            .with_int("timeout_ms", 5000)
            .with_int("retry_count", 3)
            .with_int("retry_delay_ms", 1000)
    )
    .build()
```

**Adaptive batching for high volume:**
```rust
Reaction::http("high-volume-webhook")
    .subscribe_to("all-changes")
    .with_properties(
        Properties::new()
            .with_string("endpoint", "https://api.example.com/batch")
            .with_bool("adaptive_batching", true)
            .with_int("max_batch_size", 100)
            .with_int("target_latency_ms", 100)
            .with_float("adjustment_factor", 0.1)
    )
    .build()
```

### Application Reaction

Process changes in your application:

```rust
// Setup reaction
let core = DrasiServerCore::builder()
    .add_reaction(
        Reaction::application("processor")
            .subscribe_to("my-query")
            .build()
    )
    .build()
    .await?;

core.start().await?;

// Get handle
let reaction = core.reaction_handle("processor")?;

// Process changes asynchronously
let processor = tokio::spawn(async move {
    let mut stream = reaction.as_stream().await.unwrap();

    while let Some(change) = stream.next().await {
        println!("Query: {}", change.query_id);
        println!("Timestamp: {}", change.timestamp);

        for result in change.results {
            match result {
                QueryResult::Adding { after } => {
                    handle_new_match(after).await;
                }
                QueryResult::Updating { before, after } => {
                    handle_update(before, after).await;
                }
                QueryResult::Removing { before } => {
                    handle_removal(before).await;
                }
            }
        }
    }
});
```

### SSE Reaction

Stream to web browsers:

```rust
Reaction::custom("sse", "sse")
    .subscribe_to("live-updates")
    .with_properties(
        Properties::new()
            .with_string("host", "0.0.0.0")
            .with_int("port", 8080)
            .with_string("path", "/events")
            .with_object("cors", json!({
                "allowed_origins": ["https://app.example.com"],
                "allowed_methods": ["GET"],
                "allowed_headers": ["Content-Type"]
            }))
    )
    .build()
```

**Client-side JavaScript:**
```javascript
const events = new EventSource('http://localhost:8080/events');

events.onmessage = (event) => {
    const change = JSON.parse(event.data);
    console.log('Change detected:', change);
    updateUI(change);
};
```

### Profiler Reaction

Analyze pipeline performance:

```rust
Reaction::custom("profiler", "profiler")
    .subscribe_to("my-query")
    .with_properties(
        Properties::new()
            .with_int("window_size", 1000)
            .with_int("report_interval_secs", 60)
    )
    .build()
```

**Output:**
```
Profiling Statistics (1000 samples):

Source → Query Latency:
  mean: 245.3 μs, p50: 235.0 μs, p95: 387.5 μs, p99: 524.8 μs

Query Processing Time:
  mean: 1.2 ms, p50: 1.1 ms, p95: 1.9 ms, p99: 2.4 ms

Total End-to-End:
  mean: 2.5 ms, p50: 2.4 ms, p95: 3.6 ms, p99: 4.2 ms
```

## Bootstrap System

Bootstrap provides initial data before streaming begins. This is critical for continuous queries to establish their initial result set.

### Universal Bootstrap Architecture

**Key Innovation**: ANY source can use ANY bootstrap provider. This separation enables powerful patterns:

```yaml
sources:
  # HTTP source with PostgreSQL bootstrap
  - id: api-with-db-bootstrap
    source_type: http
    bootstrap_provider:
      type: postgres  # Bootstrap from database
    properties:
      # HTTP properties for streaming
      port: 9000
      # PostgreSQL properties for bootstrap
      database: mydb
      host: db.example.com

  # Mock source with file bootstrap (testing)
  - id: test-source
    source_type: mock
    bootstrap_provider:
      type: scriptfile
      file_paths: ["test-data.jsonl"]
```

### Bootstrap Providers

#### ScriptFile Provider (Testing/Development)

JSONL format for test data:

```json
{"type":"header","version":"1.0"}
{"type":"node","id":"n1","labels":["Person"],"properties":{"name":"Alice","age":30}}
{"type":"node","id":"n2","labels":["Person"],"properties":{"name":"Bob","age":25}}
{"type":"relation","id":"r1","startNode":"n1","endNode":"n2","type":"KNOWS","properties":{}}
```

Use in code:
```rust
Source::mock("test")
    .with_bootstrap_script("test-data.jsonl")
    .build()
```

#### PostgreSQL Provider

Snapshot database state:

```rust
Source::postgres("db")
    .with_postgres_bootstrap()  // Use native snapshot
    .build()
```

#### Platform Provider

Bootstrap from remote Drasi:

```rust
Source::platform("external")
    .with_platform_bootstrap("http://drasi.example.com:8080", Some(300))
    .build()
```

### Bootstrap Timing

```
core.start() called
     ↓
Bootstrap Phase (async, non-blocking)
     ├─ Sources load initial data
     ├─ Queries build initial result sets
     └─ Bootstrap complete events fired
     ↓
Streaming Phase
     ├─ Sources emit live changes
     └─ Queries update incrementally
```

Monitor bootstrap:
```rust
// Check if bootstrap is complete
let status = core.get_source_status("my-source").await?;
match status {
    ComponentStatus::Starting => println!("Still bootstrapping..."),
    ComponentStatus::Running => println!("Bootstrap complete, streaming"),
    _ => {}
}
```

## Data Flow and Transformations

### Data Flow Sequence

```
1. Source Event Generation
   └─> SourceChange { op, element, timestamp }

2. Router Distribution
   └─> Broadcast to subscribed queries

3. Query Processing
   └─> Continuous query evaluation
   └─> Result diff generation

4. Result Distribution
   └─> QueryResultEvent { results, timestamp }

5. Reaction Processing
   └─> Transform and deliver to destination
```

### Schema at Each Stage

#### Stage 1: Source Change

```rust
pub struct SourceChange {
    pub op: SourceOperation,      // Insert, Update, Delete
    pub element: Element,         // Node or Relation
    pub timestamp: DateTime<Utc>,
}

pub enum Element {
    Node {
        id: String,
        labels: Vec<String>,
        properties: HashMap<String, Value>,
    },
    Relation {
        id: String,
        start_node: String,
        end_node: String,
        labels: Vec<String>,
        properties: HashMap<String, Value>,
    },
}
```

#### Stage 2: Query Input

Changes are grouped and ordered:
```rust
pub struct QueryInput {
    pub changes: Vec<SourceChange>,
    pub source_id: String,
    pub sequence: u64,
}
```

#### Stage 3: Query Result

```rust
pub struct QueryResultEvent {
    pub query_id: String,
    pub results: Vec<QueryResult>,
    pub timestamp: DateTime<Utc>,
}

pub enum QueryResult {
    Adding {
        after: HashMap<String, Value>
    },
    Updating {
        before: HashMap<String, Value>,
        after: HashMap<String, Value>
    },
    Removing {
        before: HashMap<String, Value>
    },
}
```

#### Stage 4: Reaction Output

Format depends on reaction type:

**HTTP/gRPC**: JSON serialization
```json
{
  "query_id": "high-temp",
  "timestamp": "2024-01-01T12:00:00Z",
  "results": [
    {
      "type": "Adding",
      "after": {
        "sensor_id": "s1",
        "temperature": 80
      }
    }
  ]
}
```

**Application**: Direct struct access
```rust
QueryResultEvent {
    query_id: String,
    results: Vec<QueryResult>,
    timestamp: DateTime<Utc>,
}
```

### Transformation Examples

#### Source to Graph

PostgreSQL row to node:
```sql
-- Table: customers (id, name, tier)
-- Row: (1, 'Alice', 'gold')
```

Transforms to:
```rust
Element::Node {
    id: "customer-1",
    labels: vec!["Customer"],
    properties: {
        "id": 1,
        "name": "Alice",
        "tier": "gold"
    }
}
```

#### Query Result Mapping

Cypher RETURN to result:
```cypher
RETURN s.id AS sensor, s.temp AS temperature
```

Maps to:
```rust
QueryResult::Adding {
    after: {
        "sensor": "s1",
        "temperature": 80
    }
}
```

## Advanced Topics

### Performance Optimization

#### Priority Queue Tuning

Configure queue capacity based on workload:

```yaml
server_core:
  priority_queue_capacity: 50000  # Global default

queries:
  - id: high-volume
    priority_queue_capacity: 100000  # Override for this query

  - id: normal-volume
    # Uses global default (50000)
```

Sizing guidelines:
- Steady production: 10,000 (default)
- High throughput: 50,000-100,000
- Bulk migration: 100,000-1,000,000
- IoT/Edge: 1,000-5,000

#### Adaptive Batching

For high-volume scenarios:

```rust
Reaction::http("batch-processor")
    .with_properties(
        Properties::new()
            .with_bool("adaptive_batching", true)
            .with_int("target_latency_ms", 100)
            .with_int("max_batch_size", 500)
            .with_float("adjustment_factor", 0.2)
    )
    .build()
```

The system automatically adjusts batch size to maintain target latency.

### Component Management

#### Dynamic Component Lifecycle

```rust
// Add source at runtime
core.add_source_runtime(
    Source::postgres("new-db")
        .auto_start(false)  // Don't start immediately
        .build()
).await?;

// Start when ready
core.start_source("new-db").await?;

// Stop temporarily
core.stop_source("new-db").await?;

// Remove completely
core.remove_source("new-db").await?;
```

#### Health Monitoring

```rust
// Monitor component health
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let sources = core.list_sources().await?;
        for (id, status) in sources {
            if matches!(status, ComponentStatus::Error) {
                log::error!("Source {} failed", id);

                // Attempt recovery
                core.stop_source(&id).await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
                core.start_source(&id).await?;
            }
        }
    }
});
```

### Testing Strategies

#### Unit Testing with Mock Sources

```rust
#[tokio::test]
async fn test_alert_generation() {
    let core = DrasiServerCore::builder()
        .add_source(Source::mock("test").build())
        .add_query(
            Query::cypher("alerts")
                .query("MATCH (n:Alert) WHERE n.severity = 'high' RETURN n")
                .from_source("test")
                .build()
        )
        .add_reaction(
            Reaction::application("results")
                .subscribe_to("alerts")
                .build()
        )
        .build()
        .await
        .unwrap();

    core.start().await.unwrap();

    let source = core.source_handle("test").unwrap();
    let reaction = core.reaction_handle("results").unwrap();
    let mut stream = reaction.as_stream().await.unwrap();

    // Send test data
    source.send_node_insert(
        "alert-1",
        vec!["Alert"],
        PropertyMapBuilder::new()
            .with_string("severity", "high")
            .build()
    ).await.unwrap();

    // Verify result
    let result = stream.next().await.unwrap();
    assert_eq!(result.results.len(), 1);

    core.stop().await.unwrap();
}
```

#### Integration Testing with Bootstrap

```rust
#[tokio::test]
async fn test_with_initial_data() {
    // Create test data file
    let test_data = r#"
{"type":"header","version":"1.0"}
{"type":"node","id":"n1","labels":["TestNode"],"properties":{"value":100}}
{"type":"node","id":"n2","labels":["TestNode"],"properties":{"value":200}}
"#;

    std::fs::write("test.jsonl", test_data).unwrap();

    let core = DrasiServerCore::builder()
        .add_source(
            Source::mock("test")
                .with_bootstrap_script("test.jsonl")
                .build()
        )
        .add_query(
            Query::cypher("sum")
                .query("MATCH (n:TestNode) RETURN sum(n.value) as total")
                .from_source("test")
                .build()
        )
        .build()
        .await
        .unwrap();

    core.start().await.unwrap();

    // Query should have initial sum of 300
    let results = core.get_query_results("sum").await.unwrap();
    assert_eq!(results[0]["total"], 300);

    core.stop().await.unwrap();
}
```

### Error Handling

#### Typed Errors

```rust
use drasi_server_core::{DrasiError, Result};

match core.source_handle("nonexistent") {
    Ok(handle) => { /* use handle */ }
    Err(DrasiError::ComponentNotFound { kind, id }) => {
        eprintln!("{} '{}' not found", kind, id);
    }
    Err(e) => eprintln!("Unexpected error: {}", e),
}
```

#### Retry Logic

```rust
async fn send_with_retry(
    source: &ApplicationSourceHandle,
    max_retries: u32,
) -> Result<()> {
    let mut attempts = 0;

    loop {
        match source.send_node_insert(...).await {
            Ok(_) => return Ok(()),
            Err(e) if attempts < max_retries => {
                attempts += 1;
                let delay = Duration::from_millis(100 * attempts as u64);
                tokio::time::sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}
```

## API Reference

### Core Types

For detailed API documentation, see:
- [`src/lib.rs`](../src/lib.rs) - Public API exports
- [`src/config.rs`](../src/config.rs) - Configuration types
- [`src/error.rs`](../src/error.rs) - Error types

### Builder API

Key builder methods:

```rust
DrasiServerCore::builder()
    .with_id(&str)
    .add_source(SourceConfig)
    .add_query(QueryConfig)
    .add_reaction(ReactionConfig)
    .with_property(key, value)
    .build() -> Future<DrasiServerCore>
```

### Runtime Management API

```rust
// Lifecycle control
core.start() -> Future<Result>
core.stop() -> Future<Result>
core.start_source(&str) -> Future<Result>
core.stop_source(&str) -> Future<Result>

// Component management
core.add_source_runtime(SourceConfig) -> Future<Result>
core.remove_source(&str) -> Future<Result>
core.list_sources() -> Future<Vec<(String, ComponentStatus)>>
core.get_source_info(&str) -> Future<SourceInfo>

// Handles
core.source_handle(&str) -> Result<ApplicationSourceHandle>
core.reaction_handle(&str) -> Result<ApplicationReactionHandle>
```

## Best Practices

### 1. Query Design

✅ **DO:**
- Be specific in patterns
- Filter early with WHERE
- Return only needed fields
- Use meaningful aliases

❌ **DON'T:**
- Use `MATCH (n) RETURN n` in production
- Rely on ORDER BY (not supported)
- Create overly complex patterns

### 2. Source Selection

Choose sources based on:
- **PostgreSQL**: When you need CDC from existing database
- **HTTP/gRPC**: For service integration
- **Application**: For testing and in-process events
- **Platform**: For Drasi Platform integration

### 3. Bootstrap Strategy

- **Always bootstrap** for queries that need complete state
- **Skip bootstrap** for event-only processing
- **Use ScriptFile** for consistent test data
- **Mix providers** for complex scenarios

### 4. Performance

- **Profile first**: Use Profiler reaction to find bottlenecks
- **Tune queues**: Adjust capacity based on metrics
- **Enable batching**: For high-volume reactions
- **Monitor memory**: Larger queues = more memory

### 5. Error Handling

- **Use Result types**: Don't unwrap in production
- **Implement retries**: For transient failures
- **Monitor health**: Check component status regularly
- **Log errors**: With context for debugging

## Troubleshooting

### Common Issues

#### "Component not found"

```rust
// Check exact ID match
let handle = core.source_handle("my-source")?;
// ID must match: Source::application("my-source")
```

#### PostgreSQL replication not working

1. Check WAL level: `SHOW wal_level;` (should be 'logical')
2. Verify publication exists: `\dRp`
3. Check user permissions: needs REPLICATION role
4. Review logs for connection errors

#### Query not detecting changes

1. Verify query syntax with online Cypher validator
2. Check source is emitting expected labels
3. Enable debug logging: `RUST_LOG=debug`
4. Use Profiler reaction to trace flow

#### High memory usage

1. Reduce priority queue capacity
2. Lower dispatch buffer capacity
3. Enable adaptive batching for reactions
4. Monitor with memory profiler

### Debug Logging

```rust
// Enable detailed logging
std::env::set_var("RUST_LOG", "drasi_server_core=debug");
env_logger::init();

// Log specific modules
std::env::set_var("RUST_LOG", "drasi_server_core::sources=trace");
```

### Performance Debugging

Use built-in profiling:

```rust
Source::mock("test")
    .with_property("enable_profiling", json!(true))
    .build()

// Add profiler reaction
Reaction::custom("profiler", "profiler")
    .subscribe_to("my-query")
    .build()
```

## Next Steps

1. **Run Examples**: Start with `examples/basic_integration/`
2. **Read Source Docs**: Each source has detailed README
3. **Experiment**: Use ApplicationSource for testing
4. **Profile**: Understand your performance characteristics
5. **Build**: Create your change-driven solution!

For more information:
- [Examples](../examples/) - Working code examples
- [Tests](../tests/) - Integration test patterns
- [API Docs](https://docs.rs/drasi-server-core) - Generated documentation

---

*Built with DrasiServerCore - Precise change detection for modern applications*