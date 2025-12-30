# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

DrasiLib is a Rust library for real-time data change processing that implements a reactive event-driven architecture. It processes data changes from various sources through Cypher queries and delivers results to reactions. This is a library-only project that can be used as a dependency by other applications.

## Key Architecture Components

### Core Abstractions
- **Sources**: Data ingestion points (PostgreSQL WAL, HTTP, gRPC, Mock, Platform via Redis Streams)
- **Queries**: Cypher-based continuous queries that process data changes
- **Reactions**: Output destinations (HTTP, gRPC, SSE, Log)
- **Routers**: Handle event routing between components (DataRouter, SubscriptionRouter, BootstrapRouter)
- **Channels**: Async communication between components using Tokio channels
- **Priority Queues**: Timestamp-ordered event queues with backpressure support
- **Bootstrap Providers**: Pluggable components for initial data delivery

### Channels and Backpressure

DrasiLib uses two dispatch modes for event routing:

#### Dispatch Modes

**Channel Mode (Default)** - Recommended for most use cases
- Creates **isolated MPSC channels per subscriber** (query or reaction)
- Provides **backpressure** when subscribers are slow - sources wait instead of dropping events
- **Zero message loss** with blocking enqueue to priority queues
- Slow subscribers don't affect fast ones
- Use this when: queries have different processing speeds, message loss is unacceptable

**Broadcast Mode**
- Uses **single shared broadcast channel** for all subscribers
- **No backpressure** - fast send, receivers can lag
- **Messages may be lost** when receivers fall behind
- Lower memory usage (one channel vs N channels)
- Use this when: all subscribers process at similar speeds, high fanout (10+ subscribers), can tolerate message loss

#### Priority Queue Backpressure

Priority queues support two enqueue strategies based on dispatch mode:

**Blocking Enqueue (`enqueue_wait()`)** - Used with Channel Mode
- Waits until space is available in the queue
- Never drops events - provides end-to-end backpressure
- Backpressure flows: Query Priority Queue → Channel Buffer → Source
- **Safe for Channel mode** (isolated channels)
- **Never use with Broadcast mode** (causes deadlock)

**Non-blocking Enqueue (`enqueue()`)** - Used with Broadcast Mode
- Returns immediately, drops events when queue is full
- Prevents deadlock in broadcast scenarios
- Metrics track `drops_due_to_capacity`

**Configuration:**
```yaml
# Channel mode (default) - backpressure enabled, zero message loss
sources:
  - id: my_source
    dispatch_mode: channel  # Default, no need to specify
    dispatch_buffer_capacity: 1000  # Per-subscriber channel buffer

queries:
  - id: my_query
    priority_queue_capacity: 10000  # Events queue before backpressure
    dispatch_mode: channel  # For query → reaction routing

# Broadcast mode - lower memory, possible message loss
sources:
  - id: high_fanout_source
    dispatch_mode: broadcast
    dispatch_buffer_capacity: 100000  # Large shared buffer
```

**Metrics:**
- `blocked_enqueue_count`: Times backpressure caused blocking (channel mode)
- `drops_due_to_capacity`: Events dropped (broadcast mode or overload)
- `current_depth`, `max_depth_seen`: Queue utilization
- `total_enqueued`, `total_dequeued`: Throughput tracking

#### Adaptive Batcher Channel Capacity

Adaptive reactions (HTTP, gRPC) and the HTTP source use an internal channel between the receiver task and the `AdaptiveBatcher`. This channel capacity **automatically scales** with the `max_batch_size` configuration:

**Automatic Scaling**: `channel_capacity = max_batch_size × 5`

This 5x multiplier provides:
- **Pipeline parallelism**: Next batch accumulates while current batch is being sent
- **Burst handling**: Absorbs temporary traffic spikes without backpressure
- **Throughput smoothing**: Reduces blocking on channel sends

**Scaling Examples**:
| max_batch_size | Channel Capacity | Memory (1KB/event) |
|----------------|------------------|---------------------|
| 100            | 500              | ~500 KB            |
| 1,000 (default)| 5,000            | ~5 MB              |
| 5,000          | 25,000           | ~25 MB             |

**Implementation**: See `AdaptiveBatchConfig::recommended_channel_capacity()` in `/Users/allenjones/dev/agentofreality/drasi/drasi-core/lib/src/utils/adaptive_batcher.rs`

### Bootstrap Provider Architecture
DrasiLib features a **universal pluggable bootstrap provider system** where ALL sources support configurable bootstrap providers, completely separating bootstrap (initial data delivery) from source streaming logic.

**Key Architectural Principle**: Bootstrap providers are independent from sources. Any source can use any bootstrap provider, enabling powerful use cases like "bootstrap from database, stream changes from HTTP endpoint."

#### All Sources Support Bootstrap Providers
- **PostgresReplicationSource**: ✅ Delegates to configured provider
- **HttpSource (Adaptive)**: ✅ Delegates to configured provider
- **GrpcSource**: ✅ Delegates to configured provider
- **MockSource**: ✅ Delegates to configured provider
- **PlatformSource**: ✅ Delegates to configured provider
- **ApplicationSource**: ✅ Delegates to configured provider (falls back to internal if no provider configured)

#### Bootstrap Provider Types
- **PostgreSQL Provider**: Handles PostgreSQL snapshot-based bootstrap with LSN coordination
- **Application Provider**: Replays stored insert events for application sources
- **Script File Provider**: Reads structured bootstrap data from JSONL script files with support for nodes, relations, and multi-file processing - use this for testing and development
- **Platform Provider**: Bootstraps data from a Query API service running in a remote Drasi environment via HTTP streaming
- **No-Op Provider**: Default provider that returns no data

#### Bootstrap Configuration Examples

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
      # ... other postgres config
```

**Mix-and-Match Configuration** (any source with any provider):
```yaml
sources:
  # HTTP source with PostgreSQL bootstrap - bootstrap 1M records from DB, stream changes via HTTP
  - id: http_with_postgres_bootstrap
    source_type: http
    bootstrap_provider:
      type: postgres  # Bootstrap from PostgreSQL
      # provider uses source properties for connection details
    properties:
      host: localhost
      port: 9000
      database: mydb  # Used by postgres bootstrap provider
      user: dbuser
      password: dbpass
      tables: ["stocks", "portfolio"]
      table_keys:
        - table: stocks
          key_columns: ["symbol"]

  # gRPC source with ScriptFile bootstrap - load test data from file, stream changes via gRPC
  - id: grpc_with_file_bootstrap
    source_type: grpc
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/path/to/initial_data.jsonl"
    properties:
      # gRPC properties here

  # Mock source with ScriptFile bootstrap - for testing
  - id: test_source
    source_type: mock
    bootstrap_provider:
      type: scriptfile
      file_paths:
        - "/path/to/test_data.jsonl"
    properties: {}

  # Platform source with Platform bootstrap
  - id: platform_source
    source_type: platform
    bootstrap_provider:
      type: platform
      query_api_url: "http://remote-drasi:8080"
    properties:
      redis_url: "redis://localhost:6379"
      stream_key: "external-source:changes"
      consumer_group: "drasi-core"
```

**Script File Format**: JSONL (JSON Lines) with record types: Header (required first), Node, Relation, Comment (filtered), Label (checkpoint), and Finish (optional end). Supports multi-file reading in sequence.

## Server Core Architecture

### Modular Design
The `DrasiLib` implementation has been refactored from a monolithic structure into specialized modules, each responsible for a specific concern:

- **`state_guard.rs`** - Centralized initialization checking that eliminates 27 duplicate state validation patterns throughout the codebase. Provides consistent guards for ensuring the server is in the correct state before executing operations.

- **`component_ops.rs`** - Generic component operation helpers that provide unified error mapping and status checking across components. Handles common patterns for source, query, and reaction operations with consistent error reporting.

- **`inspection.rs`** - Contains all inspection and listing API methods (15 methods total). Provides query capabilities for inspecting sources, queries, reactions, router state, and system metrics. Centralizes all read-only inspection functionality.

- **`lifecycle.rs`** - Orchestrates component lifecycle management including creation, starting, stopping, and deletion of sources, queries, and reactions. Manages the state transitions and dependencies between components.

- **`tests/`** - Comprehensive unit test suite organized by category, ensuring quality and maintainability of core components. Tests are grouped by module to improve clarity and reduce maintenance overhead.

### Delegation Pattern
`DrasiLib` maintains a clean public API by delegating specialized operations to these focused modules. The main struct acts as a facade that coordinates between:
- State management through `state_guard`
- Component operations through `component_ops`
- Inspection capabilities through `inspection`
- Lifecycle orchestration through `lifecycle`

This separation ensures that each module has a single responsibility while maintaining a unified, coherent public interface that clients interact with.

### Benefits of the Refactoring
This architectural refactoring achieved significant improvements:

- **Code Size Reduction**: Main server_core.rs file reduced from 3,052 lines to 1,430 lines (53% reduction), making it easier to understand and navigate.

- **Eliminated Duplication**: Removed 90% of code duplication by consolidating 27 repeated state validation patterns into `state_guard.rs`.

- **Improved Maintainability**: Focused modules make it easier to locate, understand, and modify related functionality. Changes to one concern don't affect unrelated code.

- **Enhanced Testability**: Tests organized by module with clear responsibilities make it easier to write, understand, and maintain unit tests for specific functionality.

- **API Compatibility**: 100% backward compatible - all public APIs remain unchanged, ensuring seamless integration with existing code using DrasiLib.

### Library Usage
The codebase is designed as a library for embedding in applications:
- **Core Component**: Use `DrasiLib` directly in your application
- **Configuration**: Use `DrasiLibConfig` for configuration

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

### Plugin Runtime Context

Sources and Reactions receive a runtime context during initialization that provides access to DrasiLib-provided services. This uses a context-based dependency injection pattern.

**SourceRuntimeContext** (provided to Sources):
- `source_id`: Unique identifier for this source instance
- `status_tx`: Channel for reporting component status events (Starting, Running, Stopped, Error)
- `state_store`: Optional persistent state storage (if configured)

**ReactionRuntimeContext** (provided to Reactions):
- `reaction_id`: Unique identifier for this reaction instance
- `status_tx`: Channel for reporting component status events (Starting, Running, Stopped, Error)
- `state_store`: Optional persistent state storage (if configured)
- `query_subscriber`: Access to query instances for subscription

**Usage Pattern**:
```rust
// For Sources
#[async_trait]
impl Source for MySource {
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;  // SourceBase stores context
    }
}

// For Reactions
#[async_trait]
impl Reaction for MyReaction {
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;  // ReactionBase stores context
    }
}
```

**Context Module**: See `src/context/mod.rs` for context type definitions.

### Internal Sources/Reactions
Internal components use application handles for direct integration:
- Sources implement trait from `sources::application`
- Reactions implement trait from `reactions::internal::application`
- Use `PropertyMapBuilder` for data transformation
- Use `SubscriptionOptions` for query subscriptions

### Configuration Management

DrasiLib uses a **decentralized configuration architecture** where config types live alongside their implementation modules.

#### Configuration Architecture

**Decentralized Organization**:
- Each source has its config in `sources/{module}/config.rs` (e.g., `PostgresSourceConfig` in `sources/postgres/config.rs`)
- Each reaction has its config in `reactions/{module}/config.rs` (e.g., `HttpReactionConfig` in `reactions/http/config.rs`)
- Common config types (LogLevel, SslMode, TableKeyConfig) in `config/common.rs`
- Discriminated union enums (SourceSpecificConfig, ReactionSpecificConfig) in `config/enums.rs`

**Convenience Re-exports**:
- All config types are re-exported from `config/mod.rs` for easy access
- Use `use crate::config::PostgresSourceConfig;` instead of `use crate::sources::postgres::PostgresSourceConfig;`
- Enums are the source of truth: `config::enums::{SourceSpecificConfig, ReactionSpecificConfig}`

**Configuration Files**:
- YAML-based configuration supported via `config/schema.rs`
- Runtime configuration persisted optionally
- Main config type is `DrasiLibConfig`

**Best Practices**:
- Config structs live in the same module as their implementation
- Use convenience re-exports from `config/mod.rs` for cleaner imports
- Common types (like LogLevel, SslMode) are centralized in `config/common.rs`

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
- `src/reactions/` - Reaction implementations (log, http, grpc, sse, profiler, storedproc-postgres)
- `src/context/` - Runtime context types for plugin service injection (SourceRuntimeContext, ReactionRuntimeContext)
- `src/bootstrap/` - Bootstrap provider system
- `src/bootstrap/providers/` - Bootstrap provider implementations
- `src/routers/` - Event routing components
- `src/channels/` - Channel definitions and types
- `src/state_store/` - State store provider interfaces and memory implementation
- `tests/` - Test suite
- `proto/` - Protocol buffer definitions

## Library Integration
To use DrasiLib as a dependency:
```rust
use drasi_server_core::{DrasiLib, DrasiLibConfig, RuntimeConfig};

// Create configuration
let config = Arc::new(RuntimeConfig::from(config));

// Create and initialize core
let mut core = DrasiLib::new(config);
core.initialize().await?;
core.start().await?;
```
- Drasi Core does not support cypher and gql queries with ORDER BY, TOP, and LIMIT clauses