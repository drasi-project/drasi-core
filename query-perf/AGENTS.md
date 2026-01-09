# AGENTS.md: `query-perf` Crate

## Architectural Intent
A **standalone benchmarking and performance testing tool** for the Drasi engine. It is designed to measure the throughput and latency of continuous queries under controlled scenarios 
      using different storage backends.

## Architectural Rules
*   **Standalone Execution**: This crate is a binary application (`main.rs`), not a library. It is executed via the command line.
*   **Scenario-Based Testing**: Benchmarks are defined as **Scenarios**. A scenario encapsulates:
    1.  A specific Cypher query.
    2.  A configuration (iterations, seed).
    3.  A generator for `SourceChange` events (Bootstrap stream & Run stream).
*   **Pluggable Backends**: The tool MUST support configuring different `ElementIndex` and `ResultIndex` implementations (Memory, Redis, RocksDB) at runtime via CLI arguments.

## Core Abstractions
*   **`PerformanceTestScenario`**: The trait implemented by all benchmarks. It provides the query string and data streams.
*   **`SourceChangeGenerator`**: A trait for deterministically generating a stream of `SourceChange` events for the test.
*   **`TestRunResult`**: Collects and calculates metrics (throughput, latency, duration) for the test run.

## Usage
*   **CLI Arguments**:
    *   `--scenario`: Name of the scenario to run (e.g., `single_node_property_projection`).
    *   `--element-index`: Backend for element storage (`memory`, `redis`, `rocksdb`).
    *   `--result-index`: Backend for result storage (`memory`, `redis`, `rocksdb`).
    *   `--iterations`: Number of events to process.
    *   `--seed`: (Optional) Random seed for deterministic test runs.
