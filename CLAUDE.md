# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Drasi-core is a Rust workspace library that implements continuous queries for graph data using Cypher Query Language. It provides the foundational components for the [Drasi platform](https://github.com/drasi-project/drasi-platform) and can also be used standalone for embedded scenarios.

## Project Structure

This is a Cargo workspace with the following key components:

- **core** - Main library with query evaluation engine, in-memory indexing, and continuous query processing
- **query-ast** - Abstract syntax tree for query parsing
- **query-cypher** - Cypher query language parser and compiler  
- **query-gql** - GraphQL query support
- **functions-cypher** - Built-in Cypher functions
- **functions-gql** - Built-in GraphQL functions
- **index-rocksdb** - RocksDB persistent storage backend
- **index-garnet** - Garnet (Redis-compatible) storage backend
- **middleware** - Query middleware components
- **examples** - Usage examples and demonstrations
- **query-perf** - Performance testing utilities
- **lib** - Library that lets you run Drasi as a fully functional embedded library with sources, queries, reactions, and bootstrap providers
- **shared-tests** - Common test utilities

## Development Commands

### Building
```bash
cargo build                    # Build all workspace members
cargo build --release         # Release build with optimizations
cargo build -p drasi-core      # Build specific package
```

### Testing
```bash
cargo test                     # Run all tests
cargo test -p drasi-core       # Run tests for specific package
```

For performance tests:
```bash
cd query-perf
cargo run -- -s all -e memory -r memory -i 100000
```

### Linting and Formatting
```bash
cargo clippy --all-targets --all-features  # Run clippy lints
cargo fmt                                  # Format code
cargo fmt -- --check                       # Check formatting
```

The project uses specific clippy configuration in `clippy.toml` that allows print/unwrap in tests.

### Rust Toolchain
- Uses Rust 1.83.0 for CI (see `.github/workflows/ci-lint.yml`)
- Formatting uses nightly toolchain

## Architecture

### Core Query Engine
The main query evaluation logic is in `core/src/evaluation/` which processes continuous queries against graph data. Key concepts:

- **Continuous Queries**: Unlike traditional queries, these run continuously and emit diffs when data changes
- **Graph Model**: Uses labeled property graphs with nodes and relationships  
- **Storage Backends**: Supports in-memory, RocksDB, and Redis/Garnet for persistent indexing

### Query Languages
- **GraphQL**: Default query interface, compiled to Cypher under the hood
- **Cypher**: Supported query language for graph traversal and pattern matching

### Storage Architecture
- Default: In-memory indexes for development and testing
- Production: Persistent storage via RocksDB or Redis/Garnet backends
- Index management in `core/src/in_memory_index/` and separate storage crates

## Plugin Publishing and Signing

- The `publish-plugins` xtask command supports `--sign` for cosign keyless signing of published plugins
- The CI workflow (`publish-plugins.yml`) signs all published plugins using cosign keyless mode via GitHub Actions OIDC
- `PluginMetadata` includes `git_commit` and `build_timestamp` fields populated at build time

## Testing Infrastructure
- Redis service required for some tests (configured in CI)
- Performance benchmarks in `query-perf/` crate
- Shared test utilities in `shared-tests/` for cross-crate testing

## Error Handling in drasi-lib

The `lib/` crate (`drasi-lib`) uses a three-layer error handling pattern following idiomatic Rust conventions (matching `sqlx`, `reqwest`, `tonic`):

### Layer 1: Public API → `DrasiError` (structured, pattern-matchable)
All methods on `DrasiLib` and the `*_ops` extension methods (source/query/reaction CRUD), `InspectionAPI`, and `component_ops` **must** return `crate::error::Result<T>` which uses `DrasiError` — a `thiserror` enum with variants like `ComponentNotFound`, `InvalidState`, `OperationFailed`, etc. This enables callers to pattern-match on error types.

### Layer 2: Internal modules → `anyhow::Result` (opaque, context-rich)
Internal orchestration modules (`lifecycle`, `component_graph`, `sources/manager`, `reactions/manager`, `queries/base`) use `anyhow::Result` with `.context()` to build rich error chains. These modules are `#[doc(hidden)]` and not part of the external API.

### Layer 3: Plugin traits → `anyhow::Result` (implementor-friendly)
Traits that plugin authors implement (`Source`, `Reaction`, `BootstrapProvider`) return `anyhow::Result`. Plugin authors use `anyhow::context()` for error chains, and the framework wraps these into `DrasiError::OperationFailed` or `DrasiError::Internal` at the public API boundary.

### Bridge mechanism
`DrasiError::Internal(#[from] anyhow::Error)` auto-converts internal `anyhow` errors to structured errors at the boundary via the `?` operator. Use `DrasiError::invalid_state()`, `DrasiError::operation_failed()`, etc. for errors with known semantics.

### Rules for contributors and agents
- **Never** return `anyhow::Result` from a public API method on `DrasiLib` or the `*_ops` modules
- **Never** use `anyhow!()` to construct errors in public API methods — use `DrasiError` variants
- **Always** use `anyhow::Result` with `.context("description")` in internal modules
- See `lib/src/error.rs` for the full `DrasiError` enum and constructor helpers