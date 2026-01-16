# NoOp Bootstrap Provider

A lightweight bootstrap provider plugin for Drasi that returns no data. This provider is useful when sources don't require bootstrap support or when you want to disable initial data loading while maintaining continuous change streaming.

## Overview

The NoOp Bootstrap Provider implements the `BootstrapProvider` trait but intentionally returns zero elements. This makes it ideal for scenarios where:

- **No historical data is needed**: Queries should only process new changes, not existing data
- **Source doesn't support bootstrap**: The data source only provides streaming updates without snapshot capabilities
- **Testing streaming logic**: You want to test continuous query processing without bootstrap complexity
- **Performance optimization**: Skipping bootstrap reduces startup time when historical data isn't required

## Key Capabilities

- **Zero-overhead implementation**: No data processing, minimal resource usage
- **Consistent logging**: Reports bootstrap completion with element count of 0
- **Builder pattern support**: Provides standard builder API for consistency with other providers
- **Thread-safe**: Can be safely shared across threads (implements `Send + Sync`)
- **Zero-sized type (ZST)**: No memory overhead when instantiated

## Configuration

### YAML Configuration

```yaml
sources:
  - id: my_source
    source_type: http  # Any source type
    bootstrap_provider:
      type: noop  # No additional configuration needed
    properties:
      # Source-specific properties
```

### Programmatic Configuration

The NoOp provider can be instantiated using either the constructor or builder pattern:

```rust
use drasi_bootstrap_noop::NoOpBootstrapProvider;

// Option 1: Direct instantiation
let provider = NoOpBootstrapProvider::new();

// Option 2: Builder pattern (for API consistency)
let provider = NoOpBootstrapProvider::builder()
    .build();
```

## Usage Examples

### Example 1: HTTP Source Without Bootstrap

Stream changes from an HTTP endpoint without loading historical data:

```rust
use drasi_lib::{DrasiLib, DrasiLibConfig, Query};
use drasi_bootstrap_noop::NoOpBootstrapProvider;
use drasi_source_http::HttpSourceBuilder;

let source = HttpSourceBuilder::new("api_source")
    .with_endpoint("http://api.example.com/events")
    .with_bootstrap_provider(Box::new(NoOpBootstrapProvider::new()))
    .build()
    .await?;

let mut drasi = DrasiLib::new(config);
drasi.initialize().await?;
drasi.add_source(source).await?;

// Query will only process new changes, no historical data
let query = Query::cypher("recent_changes")
    .query("MATCH (n:Event) WHERE n.timestamp > datetime() RETURN n")
    .from_source("api_source")
    .build();

drasi.add_query(query).await?;
drasi.start().await?;
```

### Example 2: Testing with Mock Source

Disable bootstrap when testing continuous query logic:

```rust
use drasi_lib::sources::mock::MockSource;
use drasi_bootstrap_noop::NoOpBootstrapProvider;

let mock_source = MockSource::builder()
    .with_id("test_source")
    .with_bootstrap_provider(Box::new(NoOpBootstrapProvider::new()))
    .build();

// Mock source will only process injected changes, no bootstrap
mock_source.inject_change(my_test_change).await?;
```

### Example 3: PostgreSQL Streaming Only

Stream PostgreSQL WAL changes without loading existing table data:

```rust
use drasi_source_postgres::PostgresReplicationSource;
use drasi_bootstrap_noop::NoOpBootstrapProvider;

let source = PostgresReplicationSource::builder()
    .with_id("pg_stream")
    .with_host("localhost")
    .with_database("mydb")
    .with_bootstrap_provider(Box::new(NoOpBootstrapProvider::new()))
    .build()
    .await?;

// Only new inserts/updates/deletes will be processed
// Existing rows in the database are ignored
```

### Example 4: Mix-and-Match Providers

Use different bootstrap providers for different sources:

```rust
use drasi_bootstrap_noop::NoOpBootstrapProvider;
use drasi_bootstrap_postgres::PostgresBootstrapProvider;

// Source 1: Full bootstrap from PostgreSQL
let source1 = build_source_with_bootstrap(
    Box::new(PostgresBootstrapProvider::new())
);

// Source 2: No bootstrap, streaming only
let source2 = build_source_with_bootstrap(
    Box::new(NoOpBootstrapProvider::new())
);

// Different bootstrap strategies for different sources
drasi.add_source(source1).await?;
drasi.add_source(source2).await?;
```

## API Reference

### NoOpBootstrapProvider

```rust
pub struct NoOpBootstrapProvider;
```

The main provider struct. This is a zero-sized type (ZST) with no fields.

#### Methods

```rust
impl NoOpBootstrapProvider {
    /// Create a new NoOp bootstrap provider
    pub fn new() -> Self;

    /// Create a builder for NoOpBootstrapProvider
    /// Returns NoOpBootstrapProviderBuilder for API consistency
    pub fn builder() -> NoOpBootstrapProviderBuilder;
}
```

#### Trait Implementations

```rust
impl BootstrapProvider for NoOpBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
    ) -> Result<usize>;
}
```

**Behavior**: Logs the bootstrap request for the query and immediately returns `Ok(0)` without sending any events.

**Returns**: `Result<usize>` - Always returns `Ok(0)` indicating zero elements were sent.

### NoOpBootstrapProviderBuilder

```rust
pub struct NoOpBootstrapProviderBuilder;
```

Builder for creating NoOpBootstrapProvider instances. Provided for API consistency with other bootstrap providers, though no configuration options are available.

#### Methods

```rust
impl NoOpBootstrapProviderBuilder {
    /// Create a new builder
    pub fn new() -> Self;

    /// Build the NoOpBootstrapProvider
    pub fn build(self) -> NoOpBootstrapProvider;
}
```

## Logging

The NoOp provider logs bootstrap operations at the `info` level:

```
INFO No-op bootstrap for query my_query: returning no data
```

This helps confirm that bootstrap was intentionally skipped rather than failed.

## Performance Characteristics

- **Memory**: Zero bytes (ZST optimization)
- **CPU**: Minimal (single log statement)
- **Latency**: Sub-microsecond bootstrap completion
- **Throughput**: Not applicable (no data processing)

## Comparison with Other Providers

| Provider | Use Case | Data Source | Performance |
|----------|----------|-------------|-------------|
| **NoOp** | No historical data needed | None | Instant |
| PostgreSQL | Bootstrap from database snapshot | PostgreSQL DB | Depends on table size |
| ScriptFile | Bootstrap from JSONL files | Local files | Depends on file size |
| Platform | Bootstrap from remote Drasi | HTTP API | Depends on network/data |
| Application | Bootstrap from in-memory storage | Application state | Depends on stored data |

## When to Use NoOp Provider

**Use the NoOp provider when:**
- You only care about new changes, not historical data
- The data source doesn't support snapshots
- You're testing continuous query logic
- You want to minimize startup time
- The source will be populated dynamically

**Don't use the NoOp provider when:**
- Queries need initial state to function correctly
- You're implementing aggregations that require historical data
- The source has existing data that should be processed
- You need to compare current state with changes

## Architecture Notes

### Universal Bootstrap Provider System

The NoOp provider is part of Drasi's universal pluggable bootstrap architecture where ALL sources support configurable bootstrap providers. This means:

- Any source can use the NoOp provider (PostgreSQL, HTTP, gRPC, Mock, etc.)
- Bootstrap logic is completely separate from source streaming logic
- You can mix and match providers across different sources
- Enables powerful use cases like "stream from HTTP, no bootstrap" or "stream from gRPC, bootstrap from file"

### Integration with Sources

Sources that integrate the NoOp provider:
- **PostgresReplicationSource**: Skips snapshot, only streams WAL changes
- **HttpSource**: Skips initial data fetch, only processes new events
- **GrpcSource**: Skips initial streaming, only processes new messages
- **MockSource**: Skips stored events, only processes new injections
- **PlatformSource**: Skips initial Redis stream replay, only processes new events
- **ApplicationSource**: Skips stored inserts, only processes new API calls

## Implementation Details

The NoOp provider is intentionally minimal:

```rust
#[async_trait]
impl BootstrapProvider for NoOpBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        _context: &BootstrapContext,
        _event_tx: BootstrapEventSender,
    ) -> Result<usize> {
        info!(
            "No-op bootstrap for query {}: returning no data",
            request.query_id
        );
        Ok(0)
    }
}
```

**Key design decisions:**
- No data processing: `_context` and `_event_tx` are unused (prefixed with `_`)
- Informative logging: Confirms bootstrap was intentional, not a failure
- Zero-sized type: No memory allocation overhead
- Builder pattern: Maintains API consistency despite no configuration

## Dependencies

The NoOp provider has minimal dependencies:

```toml
[dependencies]
drasi-lib = { path = "../../../lib" }
anyhow = "1.0"
async-trait = "0.1"
log = "0.4"
```

**Note**: The current `Cargo.toml` includes unnecessary dependencies (tokio-postgres, redis, etc.) that should be removed in a future cleanup. The provider only actually uses the four dependencies listed above.

## License

Licensed under the Apache License, Version 2.0. See LICENSE file for details.

## Contributing

Contributions are welcome! The NoOp provider is intentionally simple, but improvements could include:

- Additional documentation
- More usage examples
- Performance benchmarks
- Integration tests

Please see the main Drasi contributing guidelines for more information.

## See Also

- [Bootstrap Provider Architecture](../../../lib/src/bootstrap/README.md) - Overview of the bootstrap system
- [PostgreSQL Bootstrap Provider](../postgres/README.md) - Database snapshot bootstrap
- [ScriptFile Bootstrap Provider](../scriptfile/README.md) - File-based bootstrap
- [Platform Bootstrap Provider](../platform/README.md) - Remote Drasi bootstrap
- [Application Bootstrap Provider](../application/README.md) - In-memory bootstrap
