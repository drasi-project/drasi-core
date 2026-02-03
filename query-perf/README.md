# Introduction
The Component Performance Test Tool is designed to allow you to run large-scale / high-volume tests with minimal overhead in order to evaluate the raw performance of components of the query processing subsystem (e.g. query solver, element index, etc.). 

This allows us to establish a baseline understanding of raw component performance and provides a metric on which to compare different component implementations (i.e. rocks vs redis vs garnet indexes) and to evaluate the effect of code changes to the performance of these components.

The tool is structured around the concept of a Performance Test Scenario, which represents the combination of a specific Continuous Query against a data model. The same Performance Test Scenario with the same expected outcomes can then be run against different component configurations simply by specifying the desired component configuration using command-line parameters.

# Prerequisites

Before running these scenarios, ensure the following requirements are met.

## Rust Toolchain

- You need the latest stable release of Rust, which includes `cargo` (the package manager and build tool).
- Recommended version: latest stable
- Verify with:
  ```bash
  rustc --version
  cargo --version
  ```

## Redis server setup (if using redis backend)
If you plan to use Redis backend, you need a running Redis instance.

## RocksDB path configuration (if using RocksDB backend)
If you use the RocksDB backend, the tool creates a persistent database on disk.

#### Default Location
By default, the tool creates a directory named `test-data` in your current working directory.

#### Custom Location
You can change the storage location by setting the `ROCKS_PATH` environment variable:
```bash
ROCKS_PATH=/tmp/my-custom-rocksdb cargo run --release -- --scenario <SCENARIO> --element-index <INDEX> --result-index <INDEX>
```
*Note: `/tmp/my-custom-rocksdb` is the custom path.*

# Execution

You can run the Component Performance Test Tool either from a compiled binary or using Cargo.

## Using Compiled Binary
If you prefer to run the binary directly (e.g., in a production environment):

1.  **Build the project:**
    ```bash
    cargo build --release -p query-perf
    ```
2.  **Copy the build to current directory:**
    ```bash
    cp ../target/release/query-perf .
    ```
    *Note: The 'space dot' at the end is critical! It means "copy to here".*
3.  **Typical Command Structure:**
    ```bash
    ./query-perf --scenario <SCENARIO> --element-index <INDEX> --result-index <INDEX>
    ```

## Using Cargo
```bash
cargo run --release -- <ARGS>
```
*Note: We recommend using `--release` for accurate performance metrics.*

**Typical Command Structure:**
```bash
cargo run -p query-perf --release -- --scenario <SCENARIO> --element-index <INDEX> --result-index <INDEX>
```


# Command Line Arguments Description

| Argument | Description |
|----------|-------------|
| `-s <SCENARIO>`, `--scenario <SCENARIO>` | The Performance Test Scenario to run. To run all scenarios in sequence, use the value "all". |
| `-e <INDEX>`, `--element-index <INDEX>` | The element index implementation to use: `memory`, `redis`, or `rocksdb`. |
| `-r <INDEX>`, `--result-index <INDEX>` | The result index implementation to use: `memory`, `redis`, or `rocksdb`. |
| `-i <N>`, `--iterations <N>` | (Optional) The number of iterations to run, i.e. the number of source change events to simulate. If not provided, the default value defined in the Performance Test Scenario is used. |
| `--seed <SEED>` | (Optional) The random number seed to use. Using the same seed ensures that the exact same sequence of source change events will be generated in the Scenario. If not provided, a random seed is used, and results will differ across test runs. |
| `-h`, `--help` | Show help message |

# Environment Variables
The tool uses the following environment variables for configuration if the corresponding backends are selected:

| Variable | Description | Default |
| :--- | :--- | :--- |
| `REDIS_URL` | Connection URL for Redis server | `redis://127.0.0.1:6379` |
| `ROCKS_PATH` | Directory path for RocksDB storage | `test-data` |



# Scenarios
The currently defined Performance Test Scenarios are:

### `single_node_property_projection`

Path: `query-perf/src/scenario/building_comfort_scenarios/single_node_property_projection.rs`

Query:
```cypher
MATCH (r:Room)
RETURN
  r.name as RoomName,
  r.temperature AS Temperature,
  r.humidity AS Humidity,
  r.co2 AS CO2
```
**The Baseline.**
Monitors all `Room` nodes and projects their raw property values (`temperature`, `humidity`, `co2`).

**Query Explanation:** Matches every `Room` node and projects its identifier and environmental properties (`name`, `temperature`, `humidity`, `co2`).

**What it Tests:**
Establishes the baseline performance for direct node access and property retrieval. This scenario isolates the I/O and deserialization overhead of the element index without introducing complex filtering or transformation logic.

### `single_node_calculation_projection`

Path: `query-perf/src/scenario/building_comfort_scenarios/single_node_calculation.rs`

Query:
```cypher
MATCH (r:Room)
RETURN
  r.name AS RoomName,
  floor(
    50 + (r.temperature - 72) + (r.humidity - 42) +
    CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
  ) AS ComfortLevel
```
**The Calculator.**
Monitors `Room` nodes but computes a derived comfort level on the fly using arithmetic and conditional logic.

**Query Explanation:** Calculates a derived `ComfortLevel` for each `Room` using arithmetic operations on properties and classifies it using a conditional `CASE` expression.

**What it Tests:**
Evaluates the computational performance of the expression evaluation engine. This scenario measures the overhead of scalar arithmetic and conditional logic processing per event, independent of complex graph traversals.

### `single_path_averaging_projection`

Path: `query-perf/src/scenario/building_comfort_scenarios/single_path_averaging.rs`

Query:
```cypher
MATCH (r:Room)-[:PART_OF]->(f:Floor)
WITH f.name AS FloorName, floor(
    50 + (r.temperature - 72) + (r.humidity - 42) +
    CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
) AS RoomComfortLevel
RETURN FloorName, avg(RoomComfortLevel) AS ComfortLevel
```
**The Aggregator.**
Joins `Room` nodes to their parent `Floor` nodes and calculates the average comfort level per floor.

**Query Explanation:** Matches `Room` nodes connected to `Floor` nodes via the `PART_OF` relationship. Results are grouped by `Floor` to calculate the average comfort level of associated rooms.

**What it Tests:**
Assesses the performance of pattern matching (IO-bound) and aggregation (memory/CPU-bound). This scenario exercises the join algorithms for one-hop relationships and tests the efficiency of incremental state maintenance for aggregation functions.

### `single_path_no_change_averaging_projection`

Path: `query-perf/src/scenario/building_comfort_scenarios/single_path_no_change_averaging.rs`

Query: (Identical to `single_path_averaging_projection`)
```cypher
MATCH (r:Room)-[:PART_OF]->(f:Floor)
    WITH f.name AS FloorName, floor(
        50 + (r.temperature - 72) + (r.humidity - 42) +
        CASE WHEN r.co2 > 500 THEN (r.co2 - 500) / 25 ELSE 0 END
) AS RoomComfortLevel
RETURN FloorName, avg(RoomComfortLevel) AS ComfortLevel
```
**The Aggregator:**
Joins `Room` nodes to their parent `Floor` nodes and calculates the average comfort level per floor.

**Query Explanation:** Matches `Room` nodes connected to `Floor` nodes via the `PART_OF` relationship. Results are grouped by `Floor` to calculate the average comfort level of associated rooms.

**What it Tests:**
Uses an identical query to `single_path_averaging_projection`. Tests aggregation performance when many source changes do not affect the final aggregated result.

# Examples & Sample Output

### Run a scenario with Memory index (Using Compiled Binary)
```bash
./query-perf --scenario single_node_property_projection --element-index memory --result-index memory -i 1000
```

### Sample Output
```
--------------------------------
Drasi Query Component Perf Tests
--------------------------------
Test Run Config: 
TestRunConfig { scenario: "single_node_property_projection", element_index_type: Memory, result_index_type: Memory, iterations: Some(1000), seed: None }

--------------------------------
Scenario - single_node_property_projection
--------------------------------
 - Scenario Config: 
PerformanceTestScenarioConfig { name: "single_node_property_projection", query: "MATCH (r:Room) RETURN r.name as RoomName, r.temperature AS Temperature, r.humidity AS Humidity, r.co2 AS CO2", iterations: 1000, start_time_ms: 5000000, seed: 15920350805132563220 }

 - Initializing Scenario...
 - Bootstrapping Scenario...
 - Running Scenario... 
 - Result: TestRunResult {
    _scenario_name: "single_node_property_projection",
    _element_index_type: Memory,
    _result_index_type: Memory,
    bootstrap_start_time: 1770128382658,
    bootstrap_end_time: 1770128382733,
    bootstrap_duration_ms: 75,
    bootstrap_events: 4210,
    last_bootstrap_event_start_time: 1770128382733,
    min_bootstrap_duration_ms: 0,
    max_bootstrap_duration_ms: 1,
    avg_bootstrap_duration_ms: 0.017814726840855107,
    avg_bootstrap_events_per_sec: 56133.333333333336,
    run_start_time: 1770128382733,
    run_end_time: 1770128382796,
    run_duration_ms: 63,
    run_events: 1000,
    last_run_event_start_time: 1770128382796,
    min_run_duration_ms: 0,
    max_run_duration_ms: 1,
    avg_run_duration_ms: 0.063,
    avg_run_events_per_sec: 15873.015873015873,
}

```

### Run a scenario with Redis index (Using Cargo)
```bash
cargo run --release -p query-perf -- --scenario single_node_property_projection --element-index redis --result-index redis -i 1000
```
### Sample Output
```
    Finished `release` profile [optimized] target(s) in 0.90s
     Running `target/release/query-perf --scenario single_node_property_projection --element-index redis --result-index redis -i 1000`
--------------------------------
Drasi Query Component Perf Tests
--------------------------------
Test Run Config: 
TestRunConfig { scenario: "single_node_property_projection", element_index_type: Redis, result_index_type: Redis, iterations: Some(1000), seed: None }

--------------------------------
Scenario - single_node_property_projection
--------------------------------
 - Scenario Config: 
PerformanceTestScenarioConfig { name: "single_node_property_projection", query: "MATCH (r:Room) RETURN r.name as RoomName, r.temperature AS Temperature, r.humidity AS Humidity, r.co2 AS CO2", iterations: 1000, start_time_ms: 5000000, seed: 11558873445378713432 }

 - Initializing Scenario...
 - Bootstrapping Scenario...
 - Running Scenario... 
 - Result: TestRunResult {
    _scenario_name: "single_node_property_projection",
    _element_index_type: Redis,
    _result_index_type: Redis,
    bootstrap_start_time: 1770128465326,
    bootstrap_end_time: 1770128466424,
    bootstrap_duration_ms: 1098,
    bootstrap_events: 4210,
    last_bootstrap_event_start_time: 1770128466424,
    min_bootstrap_duration_ms: 0,
    max_bootstrap_duration_ms: 15,
    avg_bootstrap_duration_ms: 0.2608076009501188,
    avg_bootstrap_events_per_sec: 3834.244080145719,
    run_start_time: 1770128466424,
    run_end_time: 1770128466839,
    run_duration_ms: 415,
    run_events: 1000,
    last_run_event_start_time: 1770128466838,
    min_run_duration_ms: 0,
    max_run_duration_ms: 2,
    avg_run_duration_ms: 0.415,
    avg_run_events_per_sec: 2409.6385542168678,
}
```

### Run a scenario with RocksDB index (Using Cargo)
```bash
cargo run --release -p query-perf -- --scenario single_node_property_projection --element-index rocksdb --result-index rocksdb -i 1000
```
### Sample Output
```
   Finished `release` profile [optimized] target(s) in 0.99s
     Running `target/release/query-perf --scenario single_node_property_projection --element-index rocksdb --result-index rocksdb -i 1000`
--------------------------------
Drasi Query Component Perf Tests
--------------------------------
Test Run Config: 
TestRunConfig { scenario: "single_node_property_projection", element_index_type: RocksDB, result_index_type: RocksDB, iterations: Some(1000), seed: None }

--------------------------------
Scenario - single_node_property_projection
--------------------------------
 - Scenario Config: 
PerformanceTestScenarioConfig { name: "single_node_property_projection", query: "MATCH (r:Room) RETURN r.name as RoomName, r.temperature AS Temperature, r.humidity AS Humidity, r.co2 AS CO2", iterations: 1000, start_time_ms: 5000000, seed: 10234830241078594232 }

 - Initializing Scenario...
 - Bootstrapping Scenario...
 - Running Scenario... 
 - Result: TestRunResult {
    _scenario_name: "single_node_property_projection",
    _element_index_type: RocksDB,
    _result_index_type: RocksDB,
    bootstrap_start_time: 1770128551944,
    bootstrap_end_time: 1770128552017,
    bootstrap_duration_ms: 73,
    bootstrap_events: 4210,
    last_bootstrap_event_start_time: 1770128552017,
    min_bootstrap_duration_ms: 0,
    max_bootstrap_duration_ms: 2,
    avg_bootstrap_duration_ms: 0.017339667458432306,
    avg_bootstrap_events_per_sec: 57671.232876712325,
    run_start_time: 1770128552017,
    run_end_time: 1770128552082,
    run_duration_ms: 65,
    run_events: 1000,
    last_run_event_start_time: 1770128552082,
    min_run_duration_ms: 0,
    max_run_duration_ms: 1,
    avg_run_duration_ms: 0.065,
    avg_run_events_per_sec: 15384.615384615385,
}
```

*Note: Times associated with 'bootstrap' represent the time taken to load the initial dataset (2000 rooms + buildings). 'Run' metrics represent the performance of processing the dynamic updates.*

# Building Comfort Data Model

The performance test scenarios are based on a **synthetic building comfort data
model** designed to simulate high-volume, real-world sensor updates in a
structured graph.

---

### Hierarchical Structure

The model represents buildings as a hierarchy:

- **Building**
  - **Floor**
    - **Room**

By default, the scenarios generate:

- **10 buildings**
- **10 floors per building**
- **20 rooms per floor**
- **Total: 2,000 Room nodes**

Each level is connected using explicit graph relationships (for example,
`(:Room)-[:PART_OF]->(:Floor)`), enabling both **single-node** and
**path-based** query patterns.

---

### Room Properties

Each `Room` node contains time-varying environmental properties:

- `temperature`
- `humidity`
- `co2`

These properties simulate sensor readings and are **continuously updated** during
scenario execution using bounded random deltas.

---

### Source Change Simulation

The data model is driven by two types of source change streams:

1. **Bootstrap source changes**
   - Create the full building → floor → room hierarchy
   - Initialize all room properties
   - Establish a stable baseline before performance measurement

2. **Scenario source changes**
   - Randomly mutate room properties over time
   - Use controlled value ranges to simulate realistic sensor drift
   - Generate high-volume update events for performance testing

---

### Why This Model Is Used

This data model is intentionally designed to:

- Stress-test **continuous query evaluation**
- Measure **element index and result index performance**
- Compare backend implementations (Memory, Redis, RocksDB)

The same data model is reused across all scenarios to ensure **fair, consistent,
and repeatable performance comparisons**.


