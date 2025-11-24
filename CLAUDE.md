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
- **Cypher**: Primary query language for graph traversal and pattern matching
- **GraphQL**: Alternative query interface with GraphQL-to-Cypher compilation

### Storage Architecture
- Default: In-memory indexes for development and testing
- Production: Persistent storage via RocksDB or Redis/Garnet backends
- Index management in `core/src/in_memory_index/` and separate storage crates

## Testing Infrastructure
- Redis service required for some tests (configured in CI)
- Performance benchmarks in `query-perf/` crate
- Shared test utilities in `shared-tests/` for cross-crate testing