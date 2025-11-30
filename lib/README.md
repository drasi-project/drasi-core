# DrasiLib

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange.svg)](https://www.rust-lang.org/)

DrasiLib is a Rust library for building change-driven solutions that detect and react to data changes with precision.

## Overview

DrasiLib enables real-time change detection using **continuous queries** that maintain live result sets. Unlike traditional event-driven systems, you declare *what changes matter* using Cypher queries, and DrasiLib handles the complexity of tracking state and detecting meaningful transitions.

```
Sources → Continuous Queries → Reactions
   ↓              ↓                 ↓
Data In    Change Detection    Actions Out
```

**Key Benefits:**
- **Declarative**: Define change detection with Cypher queries, not custom code
- **Precise**: Get before/after states for every change, not just raw events
- **Pluggable**: Use existing source/reaction plugins or build your own
- **Scalable**: Built-in backpressure, priority queues, and dispatch modes

## Quick Start

### Installation

```toml
[dependencies]
drasi-lib = { path = "path/to/drasi-lib" }
tokio = { version = "1", features = ["full"] }
```

### Basic Example

```rust
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create source and reaction plugin instances (ownership transferred)
    let my_source = MySource::new("sensors", config)?;
    let my_reaction = MyReaction::new("alerts", vec!["high-temp".into()]);

    // Build and start DrasiLib
    let core = DrasiLib::builder()
        .with_id("my-app")
        .with_source(my_source)      // Ownership transferred, wrapped in Arc internally
        .with_reaction(my_reaction)  // Ownership transferred, wrapped in Arc internally
        .add_query(
            QueryConfig {
                id: "high-temp".into(),
                query: "MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s".into(),
                source_subscriptions: vec![SourceSubscription { source_id: "sensors".into(), ..Default::default() }],
                ..Default::default()
            }
        )
        .build()
        .await?;

    core.start().await?;

    // Run until shutdown
    tokio::signal::ctrl_c().await?;
    core.stop().await?;

    Ok(())
}
```

### Using Configuration Files

DrasiLib can load query configurations from YAML:

```rust
let core = DrasiLib::from_config_file("config.yaml").await?;

// Add source and reaction instances (ownership transferred)
let my_source = MySource::new("sensors", config)?;
let my_reaction = MyReaction::new("alerts", vec!["query1".into()]);
core.add_source(my_source).await?;
core.add_reaction(my_reaction).await?;

core.start().await?;
```

Example `config.yaml`:

```yaml
server_core:
  id: my-app

queries:
  - id: high-temp
    query: "MATCH (s:Sensor) WHERE s.temperature > 75 RETURN s"
    source_subscriptions:
      - source_id: sensors
    enableBootstrap: true
```

**Note:** Sources and reactions are always added programmatically as plugin instances.

## Core Concepts

### Sources

Sources ingest data from external systems and emit graph elements (nodes and relationships). DrasiLib provides a trait-based plugin architecture - you can use existing plugins or create your own.

**Available Source Plugins:** See `components/sources/` for PostgreSQL, HTTP, gRPC, Mock, Platform, and Application sources.

### Queries

Queries define what changes matter using Cypher:

```cypher
MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
WHERE o.status = 'pending' AND o.created_at < datetime() - duration('P1D')
RETURN o.id, c.email
```

Queries track **three types of results:**
- `Adding` - New rows matching the query
- `Updating` - Existing rows with changed values
- `Removing` - Rows that no longer match

**Limitation:** ORDER BY, TOP, and LIMIT are not supported in continuous queries.

### Reactions

Reactions respond to query results by triggering actions (webhooks, logging, etc.). Like sources, reactions are plugins that implement the `Reaction` trait.

**Available Reaction Plugins:** See `components/reactions/` for HTTP, gRPC, SSE, Log, and Application reactions.

### Bootstrap Providers

Bootstrap providers deliver initial data to queries before streaming begins. Any source can use any bootstrap provider, enabling patterns like "bootstrap from PostgreSQL, stream from HTTP."

## Query Configuration

| Option | Description | Default |
|--------|-------------|---------|
| `id` | Unique identifier | Required |
| `query` | Cypher query string | Required |
| `source_subscriptions` | Sources to subscribe to | Required |
| `enableBootstrap` | Load initial data | `true` |
| `auto_start` | Start with server | `true` |
| `joins` | Synthetic join definitions | None |
| `middleware` | Data transformation pipelines | `[]` |

### Multi-Source Joins

Query across multiple sources with synthetic joins:

```yaml
queries:
  - id: ticket-assignments
    query: |
      MATCH (t:Ticket)-[:ASSIGNED_TO]->(e:Employee)
      RETURN t.id, e.name
    source_subscriptions:
      - source_id: tickets
      - source_id: employees
    joins:
      - id: ASSIGNED_TO
        keys:
          - label: Ticket
            property: assignee_id
          - label: Employee
            property: id
```

## Runtime Management

```rust
// Component lifecycle
core.start_source("my-source").await?;
core.stop_query("my-query").await?;

// Inspection
let sources = core.list_sources().await?;
let queries = core.list_queries().await?;
let reactions = core.list_reactions().await?;

// Status checking
let status = core.get_source_status("my-source").await?;
```

## Dispatch Modes

DrasiLib supports two dispatch modes for event routing:

| Mode | Backpressure | Message Loss | Best For |
|------|--------------|--------------|----------|
| **Channel** (default) | Yes | None | Different subscriber speeds, critical data |
| **Broadcast** | No | Possible | High fanout (10+ subscribers), uniform speeds |

## Developer Guides

For detailed information on building plugins:

- **[Source Developer Guide](../components/sources/README.md)** - Creating custom data sources
- **[Reaction Developer Guide](../components/reactions/README.md)** - Creating custom reactions
- **[Bootstrap Provider Guide](../components/bootstrap/README.md)** - Creating bootstrap providers

## API Reference

### Core Types

| Type | Description | Location |
|------|-------------|----------|
| `DrasiLib` | Main library entry point | `src/lib_core.rs` |
| `DrasiLibConfig` | Configuration schema | `src/config/schema.rs` |
| `QueryConfig` | Query configuration | `src/config/schema.rs` |
| `ComponentStatus` | Component lifecycle status | `src/channels/events.rs` |

### Plugin Traits

| Trait | Description | Location |
|-------|-------------|----------|
| `Source` | Data ingestion plugin | `src/plugin_core/source.rs` |
| `Reaction` | Output action plugin | `src/plugin_core/reaction.rs` |
| `BootstrapProvider` | Initial data provider | `src/bootstrap/mod.rs` |

### Builder API

```rust
DrasiLib::builder()
    .with_id(&str)
    .with_source(impl Source + 'static)     // Takes ownership, wraps in Arc internally
    .with_reaction(impl Reaction + 'static) // Takes ownership, wraps in Arc internally
    .add_query(QueryConfig)
    .build() -> Future<DrasiLib>
```

## Troubleshooting

### "Source/Reaction not found"

Ensure you've added the instance to DrasiLib:

```rust
let source = MySource::new("my-source", config)?;
let core = DrasiLib::builder()
    .with_source(source)  // Don't forget this! Ownership transferred.
    .build()
    .await?;
```

### Events not being received

- **Broadcast mode**: May lose events if receivers are slow
- **Channel mode**: Provides backpressure but may block senders
- Check `dispatch_buffer_capacity` settings

### Debug Logging

```rust
std::env::set_var("RUST_LOG", "drasi_lib=debug");
env_logger::init();
```

## License

Apache License 2.0

## Related Projects

- [Drasi Platform](https://github.com/drasi-project/drasi-platform) - Complete deployment platform
- [Drasi Core](https://github.com/drasi-project/drasi-core) - Continuous query engine
