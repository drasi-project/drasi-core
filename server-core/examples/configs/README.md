# Drasi Server Core Configuration Examples

This directory contains example configurations that demonstrate how to configure various features of Drasi Server Core. Each example folder contains three equivalent configuration formats:

- `config.yaml` - YAML configuration format
- `config.json` - JSON configuration format
- `example.rs` - Rust code configuration using the library API

## Examples

### basic-mock-source/
Demonstrates the simplest possible configuration with:
- Mock sensor data source with script_file bootstrap provider
- Single Cypher query monitoring sensor readings
- Log reaction for output
- Bootstrap provider loads 10 initial sensor readings from JSONL file

**Key Features:**
- Script file bootstrap provider for initial data
- Basic source configuration
- Simple query and reaction setup

### file-bootstrap-source/
Shows how to use script_file bootstrap providers:
- Mock person data source with script_file bootstrap
- Query filtering users by age
- Enhanced logging with metadata
- Bootstrap provider loads data from `/data/users.jsonl` (JSONL format)

**Key Features:**
- Script file bootstrap provider with JSONL format
- Query filtering capabilities
- Metadata inclusion in reactions

### multi-source-pipeline/
Complex pipeline demonstrating multiple sources and bootstrap providers:
- Three different sources (sensor, user, event data)
- Mixed bootstrap providers (script_file, none)
- Multiple queries with different patterns
- Multiple reactions with different configurations

**Key Features:**
- Multiple source types with different bootstrap strategies
- Cross-source queries and relationships
- Aggregation and filtering queries
- Multiple reaction outputs

## Bootstrap Provider Types

### Script File Provider
```yaml
bootstrap_provider:
  type: scriptfile
  file_paths:
    - "/data/users.jsonl"
```

Loads initial data from JSONL (JSON Lines) script files with support for multiple sequential files.

### No Provider (Noop)
```yaml
# No bootstrap_provider specified
```

When no bootstrap provider is configured, the noop provider is used which returns no initial data.

## Configuration Formats

### YAML Configuration
- Human-readable format
- Supports comments and documentation
- Best for configuration files checked into version control

### JSON Configuration
- Machine-readable format
- Compact and widely supported
- Good for dynamic configuration generation

### Rust Code Configuration
- Type-safe configuration
- Compile-time validation
- Best for embedding Drasi Server Core as a library

## Running Examples

### From YAML/JSON Configuration
```bash
# Using the main binary (when available)
drasi-server-core --config examples/configs/basic-mock-source/config.yaml
```

### From Rust Code
```bash
# Build and run the example
cd examples/configs/basic-mock-source/
cargo run --bin example
```

## Data File Examples

Some examples reference external data files. Here are sample formats:

### users.jsonl (JSONL format for script_file provider)
```jsonl
{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"User data"}
{"kind":"Node","id":"1","labels":["Person"],"properties":{"name":"Alice Smith","age":30,"department":"Engineering"}}
{"kind":"Node","id":"2","labels":["Person"],"properties":{"name":"Bob Johnson","age":25,"department":"Sales"}}
{"kind":"Finish","description":"End of user data"}
```

JSONL format uses one JSON object per line. Script files require:
- First record must be a `Header` with start_time and description
- `Node` records have id, labels array, and properties object
- `Relation` records connect nodes with start_id and end_id
- Optional `Finish` record marks end of data (auto-generated if missing)

## Common Patterns

### Server Core Configuration
- `id`: Unique identifier for the server instance
- `priority_queue_capacity`: Global default priority queue capacity (default: 10000)
  - Applies to all queries and reactions unless overridden
  - Controls internal event buffering capacity

### Source Configuration
- `id`: Unique identifier for the source
- `source_type`: Type of source (e.g., "mock")
- `auto_start`: Whether to start automatically
- `properties`: Source-specific configuration
- `bootstrap_provider`: Optional initial data provider

### Query Configuration
- `id`: Unique identifier for the query
- `query`: Cypher query string
- `query_language`: "cypher" or "gql"
- `sources`: List of source IDs to query
- `auto_start`: Whether to start automatically
- `priority_queue_capacity`: Optional override for this query's priority queue capacity

### Reaction Configuration
- `id`: Unique identifier for the reaction
- `reaction_type`: Type of reaction (e.g., "log")
- `queries`: List of query IDs to react to
- `auto_start`: Whether to start automatically
- `properties`: Reaction-specific configuration
- `priority_queue_capacity`: Optional override for this reaction's priority queue capacity

## Advanced Features

### Priority Queue Capacity Tuning

Priority queues maintain timestamp-ordered event processing for queries and reactions. You can configure capacity at three levels:

**1. Global Default** (applies to all components):
```yaml
server_core:
  id: my-server
  priority_queue_capacity: 50000  # Default: 10000
```

**2. Per-Query Override**:
```yaml
queries:
  - id: high-volume-query
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
    priority_queue_capacity: 100000  # Overrides global default
```

**3. Per-Reaction Override**:
```yaml
reactions:
  - id: critical-reaction
    reaction_type: log
    queries: ["high-volume-query"]
    priority_queue_capacity: 150000  # Overrides global default
```

**When to Adjust:**
- **Increase** (50k-1M): High-volume scenarios, data migrations, burst traffic
- **Decrease** (1k-5k): Memory-constrained environments, testing
- **Default (10k)**: Most production workloads

**Memory Impact:**
- Each event ≈ 1-2 KB
- 10,000 capacity ≈ 10-20 MB per component
- 100,000 capacity ≈ 100-200 MB per component

See the `multi-source-pipeline/config.yaml` example for a complete demonstration.

### Query Joins
Multi-source queries automatically handle joins when sources are specified in the `sources` array.

### Property Access
Bootstrap providers can access source properties through the `BootstrapContext`, enabling configuration-driven bootstrap behavior.

### Component Lifecycle
All components support start/stop lifecycle management for dynamic pipeline reconfiguration.