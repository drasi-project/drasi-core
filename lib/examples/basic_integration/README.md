# Basic Drasi Lib Integration Example

This example demonstrates the simplest possible way to integrate Drasi Lib into a Rust application. It showcases the core concepts of Sources, Queries, and Reactions in both configuration-based and programmatic approaches.

## What This Example Demonstrates

- **Easy Integration**: How to embed Drasi Lib in your application with just a few lines of code
- **Configuration Loading**: Both YAML and JSON configuration support
- **Programmatic Setup**: Creating configurations entirely in code
- **Core Components**: Source → Query → Reaction data pipeline
- **Mock Data Sources**: Self-contained examples that don't require external systems

## Quick Start

1. **Build the example:**
   ```bash
   cargo build
   ```

2. **Run with configuration files (default):**
   ```bash
   # Uses config/basic.yaml by default
   cargo run
   
   # Or specify a configuration file
   cargo run -- config config/basic.json
   ```

3. **Run programmatically:**
   ```bash
   cargo run -- programmatic
   ```

## Example Modes

### Configuration Mode (Default)

Uses YAML or JSON configuration files to define sources, queries, and reactions.

**Basic Example (`config/basic.yaml`):**
- Mock counter source generating data every 2 seconds
- Cypher query filtering values > 5
- Log reaction for easy output viewing

**Configuration Files:**
- `config/basic.yaml` - Well-commented YAML example
- `config/basic.json` - Equivalent JSON configuration

**Additional Example Variants:**
- `config/sensor_example.yaml` - Sensor data with temperature/humidity filtering
- `config/multi_source_example.yaml` - Multiple sources with different intervals
- `config/generic_data_example.yaml` - Generic data type with property selection

**Try the variants:**
```bash
# Sensor data example
cargo run -- config config/sensor_example.yaml

# Multi-source example  
cargo run -- config config/multi_source_example.yaml

# Generic data example
cargo run -- config config/generic_data_example.yaml
```

### Programmatic Mode

Creates the entire configuration in Rust code, showing how to build configurations programmatically.

**Features:**
- Mock sensor source generating data every 1 second  
- Cypher query filtering temperature > 20
- Demonstrates different data types and queries

## Core Components Explained

### 1. **Sources** - Data Input
Sources provide data to the system. This example uses the built-in mock source:

```yaml
sources:
  - id: "counter-source"
    source_type: "mock"
    properties:
      data_type: "counter"    # Generate incrementing counter data
      interval_ms: 2000       # Every 2 seconds
```

### 2. **Queries** - Data Processing
Queries are continuous Cypher statements that filter and transform data:

```yaml
queries:
  - id: "filter-query"
    query: "MATCH (n:Counter) WHERE n.value > 5 RETURN n"
    sources: ["counter-source"]
```

### 3. **Reactions** - Output Destinations
Reactions receive query results and do something with them:

```yaml
reactions:
  - id: "log-reaction"
    reaction_type: "log"
    queries: ["filter-query"]
    properties:
      log_level: "info"
```

## Expected Output

When you run the example, you should see:

```bash
=== Configuration-Based Integration Example ===
Loading configuration from: config/basic.yaml
Configuration loaded successfully
Initializing Drasi Lib...
Starting Drasi Lib...
Drasi Lib is running!
The mock source will generate counter data every 2 seconds
The query will filter values > 5 and log the results
You should see data processing logs below...
(Set RUST_LOG=debug to see detailed processing information)
Running for 30 seconds...
Stopping Drasi Lib...
Example completed successfully!
```

### Seeing the Data Flow

**Basic Run** (`cargo run`): The example will show startup and shutdown messages, indicating successful operation. The system is processing data internally even if you don't see the details.

**To see detailed data processing**, run with debug logging:
```bash
RUST_LOG=debug cargo run
```

This will show:
- Data generation every 2 seconds
- Query processing: `Query 'filter-query' processing change from source 'counter-source'`
- Results: `Query 'filter-query' received 1 results` with counter values 6, 7, 8, 9, 10...
- Reactions: `Reaction 'log-reaction' processing result from query filter-query: 1 results`

**To see processing activity without debug noise**, run with info logging:
```bash  
RUST_LOG=info cargo run
```

This shows the core processing activity including `Reaction 'log-reaction' processing result` messages that confirm the system is working.

## Integration Patterns

### Basic Library Usage

```rust
use drasi_server_core::{DrasiLib, DrasiLibConfig, RuntimeConfig};
use std::sync::Arc;

// Load configuration
let config = DrasiLibConfig::load_from_file("config.yaml")?;
let runtime_config = Arc::new(RuntimeConfig::from(config));

// Create and start the core
let mut core = DrasiLib::new(runtime_config);
core.initialize().await?;
core.start().await?;

// Your application logic here...

// Graceful shutdown
core.stop().await?;
```

### Programmatic Configuration

```rust
use drasi_server_core::config::{DrasiLibConfig, SourceConfig, QueryConfig, ReactionConfig};

// Build configuration in code
let config = DrasiLibConfig {
    server: /* server settings */,
    sources: vec![/* source configs */],
    queries: vec![/* query configs */], 
    reactions: vec![/* reaction configs */],
};
```

## Configuration Reference

### Server Settings
- `host`: Server bind address (default: "0.0.0.0")
- `port`: Server port (default: 8080)  
- `log_level`: Logging verbosity (trace, debug, info, warn, error)
- `max_connections`: Maximum concurrent connections
- `shutdown_timeout_seconds`: Graceful shutdown timeout

### Source Types
- `mock`: Built-in mock data generator
  - `data_type`: "counter", "sensor", "generic"
  - `interval_ms`: Generation interval in milliseconds

### Example Variant Details

**Sensor Example Features:**
- Multiple query patterns on same data source
- Different log levels for different conditions
- Temperature and humidity filtering

**Multi-Source Example Features:**
- Multiple sources with different intervals
- Single reaction consuming multiple queries
- Fast vs. slow data generation patterns

**Generic Data Example Features:**
- Generic mock data type with random properties
- Property selection in queries
- Debug-level logging for detailed inspection

### Query Languages
- `Cypher` (default): Neo4j-compatible graph query language
- `GQL`: SQL-like graph query language

### Reaction Types
- `log`: Logs query results to console
  - `log_level`: Log level for output

## Troubleshooting

### Build Issues
- Make sure you have Rust 1.70+ installed
- Run `cargo clean` if you encounter caching issues

### Runtime Issues
- Check that configuration files exist and are valid YAML/JSON
- Verify log output for connection or query errors
- Ensure mock sources are generating expected data

### No Output
- Increase log level to "debug" for more verbose output
- Check that query filters match your mock data
- Verify reaction configuration is correct

## Next Steps

After running this example, you can:

1. **Modify Configuration**: Change query filters, data types, or intervals
2. **Add Real Sources**: Replace mock sources with PostgreSQL, HTTP, or gRPC sources
3. **Custom Reactions**: Add webhook, SSE, or custom reaction types
4. **Complex Queries**: Try joins, aggregations, or multi-node patterns
5. **Multiple Instances**: Run multiple queries and reactions simultaneously

## Additional Resources

- [Drasi Lib Documentation](../../README.md)
- [Configuration Schema Reference](../../src/config/schema.rs)
- [Source Types](../../src/sources/internal/)
- [Reaction Types](../../src/reactions/internal/)

## File Structure

```
examples/basic_integration/
├── Cargo.toml              # Project dependencies
├── README.md               # This documentation
├── src/
│   └── main.rs            # Main integration code
└── config/
    ├── basic.yaml         # YAML configuration example
    └── basic.json         # JSON configuration example
```

This example is designed to be:
- **Self-contained**: No external dependencies or setup required
- **Educational**: Clear demonstration of core integration patterns
- **Extensible**: Easy to modify and build upon for real applications
- **Minimal**: Under 200 lines of code total