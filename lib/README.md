# DrasiLib

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

DrasiLib is a Rust library for building change-driven solutions that detect and react to data changes with precision. It simplifies the complex task of tracking specific, contextual data transitions across distributed systems, enabling you to build responsive applications faster with less complexity.

## Table of Contents

1. [Why DrasiLib?](#why-drasilib)
2. [Key Use Cases](#key-use-cases)
3. [Core Concepts](#core-concepts)
4. [Architecture Overview](#architecture-overview)
5. [Plugin Architecture](#plugin-architecture)
6. [Getting Started](#getting-started)
7. [Building Change-Driven Solutions](#building-change-driven-solutions)
8. [Sources - Data Ingestion](#sources---data-ingestion)
9. [Queries - Change Detection](#queries---change-detection)
10. [Reactions - Response Actions](#reactions---response-actions)
11. [Bootstrap System](#bootstrap-system)
12. [Middleware Configuration](#middleware-configuration)
13. [Data Flow and Transformations](#data-flow-and-transformations)
14. [Advanced Topics](#advanced-topics)
15. [API Reference](#api-reference)
16. [Best Practices](#best-practices)
17. [Troubleshooting](#troubleshooting)
18. [Contributing](#contributing)
19. [License](#license)
20. [Support](#support)
21. [Related Projects](#related-projects)

## Why DrasiLib?

Traditional event-driven architectures often struggle with:
- **Event Overload**: Processing streams of generic events to find what matters
- **Complex Filtering**: Writing custom logic to parse event payloads and filter irrelevant changes
- **State Management**: Maintaining context across multiple events to detect meaningful patterns
- **Brittle Implementations**: Hard-coded change detection that breaks when systems evolve

DrasiLib solves these challenges by focusing on **precise change semantics** - detecting not just that data changed, but understanding exactly what changed and why it matters to your business logic.

Unlike traditional event-driven systems that require complex event parsing and state management, DrasiLib uses continuous queries to maintain live result sets that automatically update as your data changes. This transforms complex change detection into declarative, cost-effective implementations.

## Key Use Cases

- **Infrastructure Health Monitoring**: Track resource utilization across VMs, containers, and services to detect anomalies and optimize costs
- **Business Exception Detection**: Identify delayed orders, inventory shortages, or SLA violations as they occur
- **Building Management & IoT**: Monitor sensor networks for environmental changes that require immediate action
- **Resource Optimization**: Detect underutilized resources or capacity constraints before they impact operations
- **Compliance & Audit**: Track sensitive data changes with full context for regulatory requirements
- **Real-time Personalization**: Update user experiences instantly based on behavior changes

## Core Concepts

DrasiLib is a library for building **change-driven solutions** - applications that automatically detect and respond to meaningful data changes in real-time. Unlike traditional event-driven systems that process streams of generic events, DrasiLib uses continuous queries to maintain live result sets that update automatically as your data changes.

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

1. **Sources** emit changes as graph elements (nodes and relationships)
2. **Continuous Queries** maintain live result sets that update incrementally
3. **Reactions** receive precise change notifications with before/after states

### Graph Data Model

DrasiLib uses a labeled property graph model:

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

DrasiLib tracks three types of changes in query results:

```rust
pub enum QueryResult {
    Adding { after: HashMap<String, Value> },      // New result row
    Updating { before: HashMap<String, Value>,     // Changed result
               after: HashMap<String, Value> },
    Removing { before: HashMap<String, Value> },   // Removed result
}
```

### Key Differentiators

| Traditional Event Systems | DrasiLib |
|--------------------------|-----------------|
| Process all events, filter later | Query-first: only process relevant changes |
| Complex state management | Automatic state tracking via continuous queries |
| Hard-coded change detection | Declarative change detection with Cypher |
| Event parsing and transformation | Graph model with semantic relationships |
| Manual correlation across events | Built-in join support across sources |

### When to Use DrasiLib

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

## Architecture Overview

### Component Architecture

```
┌──────────────────────────────────────────────────────────┐
│                    DrasiLib Core                         │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ Plugin Core  │  │   Queries    │  │   Channels   │  │
│  ├──────────────┤  ├──────────────┤  ├──────────────┤  │
│  │ Source Trait │  │ Continuous   │  │ DataRouter   │  │
│  │ Reaction     │  │ Query Engine │  │ Subscription │  │
│  │   Trait      │  │              │  │   Router     │  │
│  │ Bootstrap    │  │ Multi-Source │  │ Bootstrap    │  │
│  │   Trait      │  │ Joins        │  │   Router     │  │
│  │ Registries   │  │              │  │              │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  │
│         ↓                 ↓                 ↓           │
│  ┌──────────────────────────────────────────────────┐  │
│  │              Plugin Registration                  │  │
│  ├──────────────────────────────────────────────────┤  │
│  │ • SourceRegistry: Register source plugins         │  │
│  │ • ReactionRegistry: Register reaction plugins     │  │
│  │ • BootstrapProviderFactory: Create providers      │  │
│  └──────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────┐
│              External Plugin Crates                      │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │   Sources    │  │  Reactions   │  │  Bootstrap   │  │
│  │   (Plugins)  │  │  (Plugins)   │  │  (Plugins)   │  │
│  ├──────────────┤  ├──────────────┤  ├──────────────┤  │
│  │ PostgreSQL   │  │ HTTP         │  │ PostgreSQL   │  │
│  │ HTTP         │  │ gRPC         │  │ ScriptFile   │  │
│  │ gRPC         │  │ SSE          │  │ Platform     │  │
│  │ Application  │  │ Application  │  │ Application  │  │
│  │ Platform     │  │ Log          │  │ NoOp         │  │
│  │ Mock         │  │ Profiler     │  │              │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  │
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

## Plugin Architecture

DrasiLib uses a **plugin-based architecture** where sources, reactions, and bootstrap providers are implemented as independent crates. This design provides:

- **Modularity**: Only include the plugins you need
- **Extensibility**: Easy to add new sources, reactions, or bootstrap providers
- **Testability**: Each plugin can be tested independently
- **Flexibility**: Mix and match plugins for your use case

### Core Traits

All plugins implement traits defined in the `plugin_core` module:

```rust
use drasi_lib::plugin_core::{Source, Reaction, BootstrapProvider};

// Source trait for data ingestion plugins
#[async_trait]
pub trait Source: Send + Sync {
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &SourceConfig;
    async fn subscribe(...) -> Result<SubscriptionResponse>;
}

// Reaction trait for output plugins
#[async_trait]
pub trait Reaction: Send + Sync {
    async fn start(&self, query_subscriber: Arc<dyn QuerySubscriber>) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &ReactionConfig;
}

// BootstrapProvider trait for initial data delivery
#[async_trait]
pub trait BootstrapProvider: Send + Sync {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize>;
}
```

### Base Implementations

DrasiLib provides base implementations with common functionality:

```rust
use drasi_lib::plugin_core::{SourceBase, ReactionBase};

pub struct MySource {
    base: SourceBase,  // Handles dispatching, subscriptions, lifecycle
    // custom fields...
}

pub struct MyReaction {
    base: ReactionBase,  // Handles priority queue, subscriptions, lifecycle
    // custom fields...
}
```

### Plugin Registry

Plugins are registered dynamically via the registry system:

```rust
use drasi_lib::plugin_core::{SourceRegistry, ReactionRegistry};

// Create registries
let mut source_registry = SourceRegistry::new();
let mut reaction_registry = ReactionRegistry::new();

// Register plugins
source_registry.register("postgres".to_string(), |config, event_tx| {
    Ok(Arc::new(PostgresSource::new(config, event_tx)?))
});

reaction_registry.register("http".to_string(), |config, event_tx| {
    Ok(Arc::new(HttpReaction::new(config, event_tx)?))
});

// Use in DrasiLib builder
let core = DrasiLib::builder()
    .with_source_registry(source_registry)
    .with_reaction_registry(reaction_registry)
    .build()
    .await?;
```

### Creating a Custom Plugin

Here's how to create a custom source plugin:

```rust
use drasi_lib::plugin_core::{Source, SourceBase};
use drasi_lib::config::SourceConfig;
use drasi_lib::channels::{ComponentEventSender, ComponentStatus, SubscriptionResponse};

pub struct MyCustomSource {
    base: SourceBase,
    // Custom fields for your source
}

impl MyCustomSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }
}

#[async_trait]
impl Source for MyCustomSource {
    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Starting).await;

        // Your startup logic here
        // When data changes, dispatch events:
        // self.base.dispatch_source_change(change).await?;

        self.base.set_status(ComponentStatus::Running).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &SourceConfig {
        &self.base.config
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(
            query_id,
            enable_bootstrap,
            node_labels,
            relation_labels,
            "my_custom",
        ).await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
```

## Getting Started

### Installation

Add DrasiLib to your `Cargo.toml`:

```toml
[dependencies]
drasi-lib = "0.1.0"
tokio = { version = "1", features = ["full"] }
serde_json = "1.0"
anyhow = "1.0"

# Add plugin crates as needed
# drasi-plugin-postgres-source = "0.1.0"
# drasi-plugin-http-reaction = "0.1.0"
```

### Your First Change-Driven Solution

Here's a complete example that monitors temperature sensors:

```rust
use drasi_lib::{DrasiLib, DrasiLibConfig};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load configuration from YAML
    let core = DrasiLib::from_config_file("config.yaml").await?;

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

Example `config.yaml`:

```yaml
server_core:
  id: temperature-monitor

sources:
  - id: sensors
    source_type: mock
    auto_start: true
    properties:
      data_type: sensor
      interval_ms: 2000

queries:
  - id: high-temp-alert
    query: |
      MATCH (s:Sensor)
      WHERE s.temperature > 75
      RETURN s.id, s.location, s.temperature
    sources: [sensors]

reactions:
  - id: alert-logger
    reaction_type: log
    queries: [high-temp-alert]
    properties:
      log_level: warn
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

DrasiLib offers three ways to configure your solution:

#### 1. Fluent Builder API (Recommended)

Type-safe, ergonomic configuration in code:

```rust
use drasi_lib::{DrasiLib, Source, Query, Reaction};

let core = DrasiLib::builder()
    .with_id("my-server")
    .add_source(Source::mock("sensors").build())
    .add_query(
        Query::cypher("high-temp")
            .query("MATCH (s:Sensor) WHERE s.temp > 75 RETURN s")
            .from_source("sensors")
            .build()
    )
    .add_reaction(
        Reaction::log("logger")
            .subscribe_to("high-temp")
            .build()
    )
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
let core = DrasiLib::from_config_file("config.yaml").await?;
```

#### 3. Dynamic Runtime Management

Add/remove components while running:

```rust
// Add a new source at runtime
core.add_source_runtime(
    Source::mock("new-source")
        .auto_start(true)
        .build()
).await?;

// Later, remove it
core.remove_source("new-source").await?;
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
let core = DrasiLib::builder()
    .add_query(Query::cypher("changes").build())

    // Fan out to multiple reactions
    .add_reaction(
        Reaction::http("webhook")
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

## Sources - Data Ingestion

Sources adapt external data into DrasiLib's graph model. Each source handles:
- **Bootstrap**: Loading initial data snapshot
- **Streaming**: Emitting live changes
- **Transformation**: Converting to graph elements

### Available Source Plugins

Sources are provided as independent plugin crates:

| Source Plugin | Use Case | Bootstrap Support |
|--------------|----------|-------------------|
| PostgreSQL | Database CDC | ✅ Native |
| HTTP | Webhooks/REST | ✅ Via providers |
| gRPC | High-throughput streaming | ✅ Via providers |
| Application | In-process testing | ✅ Internal |
| Platform | Drasi integration | ✅ Via providers |
| Mock | Testing/Demo | ✅ Via providers |

### Source Configuration

All sources support common configuration:
- `auto_start`: Whether to start automatically (default: true)
- `bootstrap_provider`: Initial data configuration
- `dispatch_mode`: Channel (default) or Broadcast
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

### Multi-Source Queries and Joins

Query across multiple data sources with synthetic joins:

```rust
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

### Query Limitations

Continuous queries don't support:
- `ORDER BY` - Results arrive as changes occur
- `LIMIT/SKIP` - All matching changes are emitted
- `TOP` - Maintain top-N client-side

## Reactions - Response Actions

Reactions respond to detected changes by triggering actions.

### Available Reaction Plugins

Reactions are provided as independent plugin crates:

| Reaction Plugin | Use Case | Batching |
|----------------|----------|----------|
| HTTP | Webhooks, REST APIs | ✅ Adaptive |
| gRPC | High-throughput streaming | ✅ Adaptive |
| SSE | Browser push | ❌ |
| Application | In-process handling | ❌ |
| Log | Debugging, audit | ❌ |
| Platform | Drasi integration | ❌ |
| Profiler | Performance analysis | ❌ |

### Reaction Configuration

All reactions support common configuration:
- `queries`: List of query IDs to subscribe to
- `priority_queue_capacity`: Queue size for ordered processing
- `dispatch_mode`: Channel (default) or Broadcast

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

### Bootstrap Provider Plugins

Bootstrap providers are implemented as independent plugin crates:

| Provider | Use Case | Plugin Crate |
|----------|----------|--------------|
| PostgreSQL | Database snapshots | `drasi-plugin-postgres-bootstrap` |
| ScriptFile | JSONL test data | `drasi-plugin-scriptfile-bootstrap` |
| Application | Stored events | `drasi-plugin-application-bootstrap` |
| Platform | Remote Drasi Query API | `drasi-plugin-platform-bootstrap` |
| NoOp | No initial data | `drasi-plugin-noop-bootstrap` |

### Bootstrap Provider Configuration

```yaml
# PostgreSQL bootstrap
bootstrap_provider:
  type: postgres

# ScriptFile bootstrap
bootstrap_provider:
  type: scriptfile
  file_paths:
    - "/data/initial_nodes.jsonl"
    - "/data/initial_relations.jsonl"

# Platform bootstrap (remote Drasi)
bootstrap_provider:
  type: platform
  query_api_url: "http://remote-drasi:8080"
  timeout_seconds: 600

# No-op bootstrap
bootstrap_provider:
  type: noop
```

### ScriptFile Format

JSONL (JSON Lines) with record types:

```json
{"type":"header","version":"1.0"}
{"type":"node","id":"n1","labels":["Person"],"properties":{"name":"Alice","age":30}}
{"type":"node","id":"n2","labels":["Person"],"properties":{"name":"Bob","age":25}}
{"type":"relation","id":"r1","startNode":"n1","endNode":"n2","type":"KNOWS","properties":{}}
```

## Middleware Configuration

Middleware provides powerful data transformation capabilities as data flows from sources to queries.

### Available Middleware Types

| Middleware | Purpose | Example Use Case |
|------------|---------|------------------|
| **JQ** | Transform with JQ expressions | Convert units, filter data |
| **Map** | Simple field mapping | Rename fields |
| **Unwind** | Flatten arrays | Process array elements |
| **Relabel** | Change labels | Normalize labels |
| **Decoder** | Decode values | Base64, URL decoding |
| **ParseJson** | Parse JSON strings | Extract structured data |
| **Promote** | Promote nested fields | Flatten nested objects |

### Quick Example

```yaml
queries:
  - id: temperature-monitor
    query: "MATCH (n:Sensor) WHERE n.temp_f > 80 RETURN n"

    middleware:
      - kind: jq
        name: celsius_to_fahrenheit
        config:
          Sensor:
            insert:
              - op: Insert
                label: "\"Sensor\""
                id: .id
                query: "{ id: .id, temp_f: (.temp_c * 9/5 + 32) }"

    source_subscriptions:
      - source_id: sensor_data
        pipeline: [celsius_to_fahrenheit]
```

## Advanced Topics

### Performance Optimization

#### Priority Queue Tuning

```yaml
server_core:
  priority_queue_capacity: 50000  # Global default

queries:
  - id: high-volume
    priority_queue_capacity: 100000  # Override for this query
```

Sizing guidelines:
- Steady production: 10,000 (default)
- High throughput: 50,000-100,000
- Bulk migration: 100,000-1,000,000
- IoT/Edge: 1,000-5,000

#### Dispatch Modes

**Channel Mode (Default)** - Backpressure enabled:
- Isolated channels per subscriber
- Zero message loss
- Use when: Different processing speeds, message loss unacceptable

**Broadcast Mode** - Lower memory:
- Single shared channel
- Messages may be lost when receivers lag
- Use when: Similar processing speeds, high fanout

### Component Management

```rust
// Add source at runtime
core.add_source_runtime(
    Source::mock("new-source")
        .auto_start(false)
        .build()
).await?;

// Start when ready
core.start_source("new-source").await?;

// Stop temporarily
core.stop_source("new-source").await?;

// Remove completely
core.remove_source("new-source").await?;
```

### Health Monitoring

```rust
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let sources = core.list_sources().await?;
        for (id, status) in sources {
            if matches!(status, ComponentStatus::Error) {
                log::error!("Source {} failed", id);

                // Attempt recovery
                core.stop_source(&id).await?;
                core.start_source(&id).await?;
            }
        }
    }
});
```

## API Reference

### Core Types

- [`DrasiLib`](src/server_core.rs) - Main server type
- [`DrasiLibConfig`](src/config/mod.rs) - Configuration types
- [`DrasiError`](src/error.rs) - Error types

### Plugin Core Types

- [`Source`](src/plugin_core/source.rs) - Source trait and SourceBase
- [`Reaction`](src/plugin_core/reaction.rs) - Reaction trait and ReactionBase
- [`BootstrapProvider`](src/bootstrap/mod.rs) - Bootstrap provider trait
- [`SourceRegistry`](src/plugin_core/registry.rs) - Source plugin registry
- [`ReactionRegistry`](src/plugin_core/registry.rs) - Reaction plugin registry

### Builder API

```rust
DrasiLib::builder()
    .with_id(&str)
    .add_source(SourceConfig)
    .add_query(QueryConfig)
    .add_reaction(ReactionConfig)
    .with_source_registry(SourceRegistry)
    .with_reaction_registry(ReactionRegistry)
    .build() -> Future<DrasiLib>
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
```

## Best Practices

### 1. Query Design

✅ **DO:**
- Be specific in patterns
- Filter early with WHERE
- Return only needed fields

❌ **DON'T:**
- Use `MATCH (n) RETURN n` in production
- Rely on ORDER BY (not supported)
- Create overly complex patterns

### 2. Plugin Selection

- **PostgreSQL source**: When you need CDC from existing database
- **HTTP/gRPC source**: For service integration
- **Application source**: For testing and in-process events
- **Platform source**: For Drasi Platform integration

### 3. Bootstrap Strategy

- **Always bootstrap** for queries that need complete state
- **Skip bootstrap** for event-only processing
- **Use ScriptFile** for consistent test data
- **Mix providers** for complex scenarios

### 4. Performance

- **Profile first**: Use Profiler reaction
- **Tune queues**: Adjust capacity based on metrics
- **Enable batching**: For high-volume reactions
- **Monitor memory**: Larger queues = more memory

## Troubleshooting

### Common Issues

#### "Unknown source type"

Ensure the source plugin is registered:
```rust
source_registry.register("my_type".to_string(), my_factory);
```

#### "Unknown reaction type"

Ensure the reaction plugin is registered:
```rust
reaction_registry.register("my_type".to_string(), my_factory);
```

#### Bootstrap provider not working

Bootstrap providers are in separate crates. Check the error message for the correct plugin crate to use.

### Debug Logging

```rust
std::env::set_var("RUST_LOG", "drasi_lib=debug");
env_logger::init();
```

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

## License

DrasiLib is licensed under the [Apache License 2.0](LICENSE).

## Support

- **Issues**: [GitHub Issues](https://github.com/drasi-project/drasi-core/issues)
- **Discussions**: [GitHub Discussions](https://github.com/drasi-project/drasi-core/discussions)
- **Documentation**: [https://drasi.io](https://drasi.io)

## Related Projects

- [Drasi Platform](https://github.com/drasi-project/drasi-platform): Complete Drasi deployment platform
- [Drasi Core](https://github.com/drasi-project/drasi-core): Core continuous query engine

---

*Built with DrasiLib - Precise change detection for modern applications*
