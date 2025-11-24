# DrasiLib Test Suite

This directory contains the integration test suite for the DrasiLib library, organized following Rust testing best practices.

## Test Organization

```
tests/
├── integration.rs                    # Library integration tests
├── config_validation.rs              # Config file validation tests
├── example_compilation_test.rs       # Example code validation tests
├── bootstrap_platform_provider.rs    # Platform bootstrap provider tests
├── dispatch_buffer_capacity_hierarchy_test.rs  # Dispatch buffer tests
├── platform_reaction_integration.rs  # Platform reaction tests
├── platform_source_integration.rs    # Platform source tests
├── priority_queue_hierarchy_test.rs  # Priority queue tests
├── multi_reaction_subscription_test.rs  # Multi-reaction tests
├── multi_source_join_test.rs         # Multi-source join tests
├── profiling_integration_test.rs     # Profiling tests
├── fixtures/                         # Test data and resources
│   └── bootstrap_scripts/           # Bootstrap script files
└── README.md                         # This file
```

## Test Structure

The test suite follows Rust conventions with three categories:

### Unit Tests (in `src/`)
Unit tests are located alongside the source code in `src/` using `#[cfg(test)]` modules. These test individual components in isolation.

**Current coverage: 73 unit tests**

### Integration Tests (in `tests/`)
Integration tests verify that the library works correctly as a whole from an external consumer's perspective.

#### Core Integration Tests (`tests/integration.rs`)
**Coverage: 6 integration tests**

- `test_library_initialization` - Verifies basic initialization
- `test_start_stop_lifecycle` - Tests server start/stop lifecycle
- `test_with_mock_source` - Tests with a mock data source
- `test_with_query_and_reaction` - Tests complete pipeline (source → query → reaction)
- `test_restart_capability` - Verifies server can restart after stopping
- `test_multiple_sources_and_queries` - Tests multiple component configuration

#### Configuration Validation Tests (`tests/config_validation.rs`)
Validates that all example configuration files (JSON and YAML) can be parsed successfully and contain valid data.

**What it tests:**
- All JSON/YAML config files in `examples/` directory
- Server configuration has required fields (ID, etc.)
- Sources have valid types and properties
- Queries reference valid sources
- Reactions reference valid queries

**Running:**
```bash
cargo test --test config_validation
```

#### Example Code Validation Tests (`tests/example_compilation_test.rs`)
Validates that all example.rs files in the configs directory exist and are syntactically correct.

**What it tests:**
- All example.rs files exist in `examples/configs/`
- Each example directory has corresponding config files (YAML/JSON)
- Example code structure and file organization

**Running:**
```bash
cargo test --test example_compilation_test
```

**Note:** The example.rs files are code examples demonstrating the builder API. They are not standalone projects, so they don't have individual Cargo.toml files. The test ensures they exist and are properly organized. The actual syntax validation happens through the builder API usage in other integration tests.

#### Feature-Specific Integration Tests
- `bootstrap_platform_provider.rs` - Platform bootstrap provider functionality
- `dispatch_buffer_capacity_hierarchy_test.rs` - Dispatch buffer capacity configuration
- `platform_reaction_integration.rs` - Platform reaction (Redis Streams output)
- `platform_source_integration.rs` - Platform source (Redis Streams input)
- `priority_queue_hierarchy_test.rs` - Priority queue capacity configuration
- `multi_reaction_subscription_test.rs` - Multiple reactions subscribing to queries
- `multi_source_join_test.rs` - Multi-source query joins
- `profiling_integration_test.rs` - Performance profiling

## Running Tests

### Run All Tests
```bash
# Run all tests (unit and integration)
cargo test

# Run with output for debugging
cargo test -- --nocapture

# Run with specific log level
RUST_LOG=debug cargo test
```

### Run Specific Test Categories
```bash
# Run only unit tests
cargo test --lib

# Run only integration tests
cargo test --tests

# Run a specific integration test file
cargo test --test integration

# Run a specific test within an integration test file
cargo test --test integration test_start_stop_lifecycle

# Run tests matching a pattern
cargo test lifecycle

# Run config validation tests
cargo test --test config_validation

# Run example validation tests
cargo test --test example_compilation_test
```

### Run Tests with Coverage
```bash
# Install tarpaulin if not already installed
cargo install cargo-tarpaulin

# Run tests with coverage report
cargo tarpaulin --out Html
```

## Writing New Tests

### Integration Test Guidelines

When adding new integration tests, follow this structure:

```rust
#[tokio::test]
async fn test_feature_name() -> Result<()> {
    // 1. Create configuration
    let config = create_minimal_config();
    
    // 2. Initialize DrasiLib
    let mut core = DrasiLib::new(Arc::new(config));
    core.initialize().await?;
    
    // 3. Test the feature
    core.start().await?;
    assert!(core.is_running().await);
    
    // 4. Clean up
    core.stop().await?;
    Ok(())
}
```

### Best Practices

1. **Test Isolation**: Each test should be independent
2. **Clear Names**: Test names should describe what they test
3. **Proper Cleanup**: Always stop servers and clean up resources
4. **Timeouts**: Use appropriate timeouts to prevent hanging
5. **Error Messages**: Include descriptive assertion messages
6. **Documentation**: Add comments explaining complex test logic

## Test Coverage

Current test coverage includes:

### Core Functionality ✅
- Library initialization
- Server lifecycle (start/stop/restart)
- Configuration management

### Component Types ✅
- Mock sources (`mock`)
- Cypher queries
- Log reactions (`log`)

### Data Pipeline ✅
- Source → Query → Reaction flow
- Multiple sources and queries
- Auto-start functionality

## Debugging Tests

### Enable Verbose Output
```bash
# Run tests with full output
cargo test -- --nocapture --test-threads=1

# Enable debug logging for specific modules
RUST_LOG=drasi_server_core=debug cargo test

# Enable trace logging for everything
RUST_LOG=trace cargo test -- --nocapture
```

### Common Issues and Solutions

1. **Test Failures Due to Timing**
   - Increase sleep durations in tests
   - Use proper synchronization primitives

2. **Port Already in Use**
   - Tests use port 8080 by default
   - Ensure no other services are using this port

3. **Resource Cleanup**
   - Tests should always call `stop()` in cleanup
   - Use proper error handling to ensure cleanup runs

## Continuous Integration

Example GitHub Actions workflow for CI:

```yaml
name: Tests

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
        with:
          submodules: recursive
      
      - uses: dtolnay/rust-toolchain@stable
      
      - name: Run tests
        run: cargo test --all-features
      
      - name: Run clippy
        run: cargo clippy -- -D warnings
```

## Contributing

When contributing tests:

1. **Follow Conventions**: Match existing test patterns
2. **Add Documentation**: Document what your test verifies
3. **Test Locally**: Ensure all tests pass before submitting
4. **Update README**: Document new test categories if added

## Performance Testing

For performance testing, use release builds:

```bash
cargo test --release -- --nocapture
```

Consider using `criterion` for benchmarking:

```rust
// In benches/benchmark.rs
use criterion::{criterion_group, criterion_main, Criterion};

fn benchmark_initialization(c: &mut Criterion) {
    c.bench_function("core_init", |b| {
        b.iter(|| {
            // Benchmark code
        });
    });
}

criterion_group!(benches, benchmark_initialization);
criterion_main!(benches);
```