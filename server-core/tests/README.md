# DrasiServerCore Test Suite

This directory contains the integration test suite for the DrasiServerCore library, organized following Rust testing best practices.

## Test Organization

```
tests/
├── integration.rs      # Library integration tests
└── README.md          # This file
```

## Test Structure

The test suite follows Rust conventions with two categories:

### Unit Tests (in `src/`)
Unit tests are located alongside the source code in `src/` using `#[cfg(test)]` modules. These test individual components in isolation.

**Current coverage: 73 unit tests**

### Integration Tests (`tests/integration.rs`)
Integration tests verify that the library works correctly as a whole from an external consumer's perspective.

**Current coverage: 6 integration tests**

#### Available Integration Tests:
- `test_library_initialization` - Verifies basic initialization
- `test_start_stop_lifecycle` - Tests server start/stop lifecycle
- `test_with_mock_source` - Tests with a mock data source
- `test_with_query_and_reaction` - Tests complete pipeline (source → query → reaction)
- `test_restart_capability` - Verifies server can restart after stopping
- `test_multiple_sources_and_queries` - Tests multiple component configuration

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

# Run a specific integration test
cargo test --test integration test_start_stop_lifecycle

# Run tests matching a pattern
cargo test lifecycle
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
    
    // 2. Initialize DrasiServerCore
    let mut core = DrasiServerCore::new(Arc::new(config));
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