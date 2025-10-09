# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

DrasiServerCore is a Rust library for real-time data change processing that implements a reactive event-driven architecture. It processes data changes from various sources through Cypher queries and delivers results to reactions. This is a library-only project that can be used as a dependency by other applications.

## Key Architecture Components

### Core Abstractions
- **Sources**: Data ingestion points (PostgreSQL WAL, HTTP, gRPC, Mock, Platform via Redis Streams)
- **Queries**: Cypher-based continuous queries that process data changes
- **Reactions**: Output destinations (HTTP, gRPC, SSE, Log)
- **Routers**: Handle event routing between components (DataRouter, SubscriptionRouter, BootstrapRouter)
- **Channels**: Async communication between components using Tokio channels
- **Bootstrap Providers**: Pluggable components for initial data delivery

### Bootstrap Provider Architecture
DrasiServerCore features a pluggable bootstrap provider system that separates bootstrap concerns from source streaming logic:

#### Bootstrap Provider Types
- **PostgreSQL Provider**: Handles PostgreSQL snapshot-based bootstrap with LSN coordination
- **Application Provider**: Replays stored insert events for application sources
- **Script File Provider**: Reads structured bootstrap data from JSONL script files with support for nodes, relations, and multi-file processing - use this for testing and development
- **Platform Provider**: Bootstraps data from a Query API service running in a remote Drasi environment via HTTP streaming
- **No-Op Provider**: Default provider that returns no data

#### Bootstrap Configuration Example
```yaml
sources:
  - id: my_postgres_source
    source_type: postgres
    bootstrap_provider:
      type: postgres
    properties:
      host: localhost
      database: mydb
      # ... other postgres config

  - id: my_script_source
    source_type: mock
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/path/to/script1.jsonl"
        - "/path/to/script2.jsonl"
    properties:
      # ... other mock config

  - id: my_platform_source
    source_type: platform
    bootstrap_provider:
      type: platform
      query_api_url: "http://my-source-query-api:8080"  # URL of Query API service
      timeout_seconds: 300  # Optional, defaults to 300
    properties:
      redis_url: "redis://localhost:6379"
      stream_key: "external-source:changes"
      consumer_group: "drasi-core"
      consumer_name: "consumer-1"

  # Alternative: query_api_url can be in source properties instead
  - id: my_platform_source_alt
    source_type: platform
    bootstrap_provider:
      type: platform  # Will use query_api_url from properties
    properties:
      redis_url: "redis://localhost:6379"
      stream_key: "external-source:changes"
      consumer_group: "drasi-core"
      consumer_name: "consumer-1"
      query_api_url: "http://my-source-query-api:8080"
```

**Script File Format**: JSONL (JSON Lines) with record types: Header (required first), Node, Relation, Comment (filtered), Label (checkpoint), and Finish (optional end). Supports multi-file reading in sequence.

### Library Usage
The codebase is designed as a library for embedding in applications:
- **Core Component**: Use `DrasiServerCore` directly in your application
- **Configuration**: Use `DrasiServerCoreConfig` for configuration

### Component Lifecycle
All components (sources, queries, reactions) follow a consistent lifecycle:
1. Create (configuration)
2. Start (begin processing)
3. Stop (pause processing)
4. Delete (cleanup)

Bootstrap providers are automatically registered during source creation and handle initial data delivery independently from streaming operations.

## Essential Development Commands

### Building
```bash
# Build library
cargo build --release

# Build with specific features
cargo build --features internal-source
```

### Testing
```bash
# Run all tests
cargo test

# Run specific test category
cargo test bootstrap
cargo test query

# Run with logging
RUST_LOG=debug cargo test -- --nocapture

# Run main test suite
./tests/run_working_tests.sh
```

### Code Quality
```bash
# Format code
cargo fmt

# Run linter
cargo clippy

# Check compilation
cargo check
```

## Key Implementation Patterns

### Internal Sources/Reactions
Internal components use application handles for direct integration:
- Sources implement trait from `sources::application`
- Reactions implement trait from `reactions::internal::application`
- Use `PropertyMapBuilder` for data transformation
- Use `SubscriptionOptions` for query subscriptions

### Configuration Management
- YAML-based configuration in `config/` module
- Runtime configuration persisted optionally
- Config schema defined with serde structs
- Main config type is `DrasiServerCoreConfig`

### Error Handling
- Use `anyhow::Result` for fallible operations
- Custom `DrasiError` type in `error.rs`

### Testing Approach
- Unit tests in module files (`#[cfg(test)]` blocks)
- Integration tests in `tests/` directory
- Use `test_support` module for test utilities
- Mock implementations for testing (e.g., `sources::mock`)

## Protocol Buffer Integration
The project uses gRPC with protocol buffers defined in `proto/drasi/v1/`:
- Built automatically via `build.rs`
- Generated code available in build output
- Used by gRPC sources and reactions

## PostgreSQL Integration
Special support for PostgreSQL replication:
- WAL decoding in `sources::postgres`
- SCRAM authentication implementation
- Replication protocol handling
- Bootstrap support for initial data load

## Platform Source (Redis Streams Integration)
The platform source enables integration with external Drasi Platform sources via Redis Streams:
- Consumes events from Redis Streams using consumer groups
- Transforms platform SDK event format to drasi-core SourceChange format
- Provides exactly-once delivery semantics through consumer group acknowledgments
- Supports horizontal scaling via multiple consumers in the same group
- Implements automatic reconnection and error handling

Configuration example:
```yaml
sources:
  - id: platform_source
    source_type: platform
    properties:
      redis_url: "redis://localhost:6379"
      stream_key: "sensor-data:changes"  # Stream to read from
      consumer_group: "drasi-core"       # Consumer group name
      consumer_name: "consumer-1"        # Unique consumer name
      batch_size: 10                     # Events per read (optional)
      block_ms: 5000                     # Block timeout (optional)
```

## Important Directories
- `src/sources/` - Source implementations (postgres, http, grpc, mock, application, platform)
- `src/reactions/internal/` - Internal reaction implementations
- `src/bootstrap/` - Bootstrap provider system
- `src/bootstrap/providers/` - Bootstrap provider implementations
- `src/routers/` - Event routing components
- `src/channels/` - Channel definitions and types
- `tests/` - Test suite
- `proto/` - Protocol buffer definitions
- `sdks/rust/` - Rust SDK for extensions

## Library Integration
To use DrasiServerCore as a dependency:
```rust
use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig, RuntimeConfig};

// Create configuration
let config = Arc::new(RuntimeConfig::from(config));

// Create and initialize core
let mut core = DrasiServerCore::new(config);
core.initialize().await?;
core.start().await?;
```
- Drasi Core does not support cypher and gql queries with ORDER BY, TOP, and LIMIT clauses