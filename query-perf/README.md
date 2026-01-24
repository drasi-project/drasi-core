# Introduction
The Component Performance Test Tool is designed to allow you to run large scale / high volume tests with minimal overhead in order to evaluate the raw performance of componets of the query processing subsystem (e.g. query solver, element index, etc). 

This allows us to establish a baseline understanding of raw component performance and provides a metric on which to compare different component implementations (i.e. rocks vs redis vs garnet indexes) and to evaluate the effect of code changes to the performance of these components.

The tool is structured around the concept of a Performance Test Scenario, which represents the combination of a specific Continuous Query against a data model. The same Performance Test Scenario with the same expected outcomes can then be run against different component configurations simply by specifying the desired component configuration using command line parameters.

# Execution
You can run the The Component Performance Test Tool from a compiled binary:

```
./query-perf <ARGS>
```

... or using the Cargo tool from the project directory:

```
 cargo run -- <ARGS>
```

The Component Performance Test Tool takes the following command line arguments:

| Argument | Description |
|----------|-------------|
| -s <SCENARIO>, --scenario <SCENARIO> | The Performance Test Scenario to run. To run all scenarios in sequence, use the value "all". The available Scenarios are described below. |
| -e <ELEMENT_INDEX>, --element-index <ELEMENT_INDEX> | The element index implementation to use. The available index implementations are described below. |
| -r <RESULT_INDEX>, --result-index <RESULT_INDEX> | The result index implementation to use. The available index implementations are described below. |
| -i, --iterations <ITERATIONS> | OPTIONAL - The number of iterations to run i.e. the number of source change events to simulate. If not provided the default value defined in the Performance Test Scenario is used. |
| --seed <SEED> | OPTIONAL - The random number seed to use. Using the same seed ensures that the exact same sequence of source change events will be generated in the Scenario. If not provided, a random seed is used and results will differ across test runs. |
| -h, --help | Show help message |

# Scenarios
The currently defined Performance Test Scenarios are:

- single_node_property_projection
- single_node_calculation_projection
- single_path_averaging_projection
- single_path_no_change_averaging_projection

## single_node_property_projection

**What the query does:**  
Matches each `Room` node and directly returns its stored properties
(`name`, `temperature`, `humidity`, and `CO2`) without performing any
calculations or transformations.

**What it tests:**  
This scenario tests baseline read performance, including node lookup
speed, property access cost, and result projection overhead.

---

## single_node_calculation_projection

**What the query does:**  
Matches each `Room` node and computes a derived `ComfortLevel` value
using arithmetic operations and conditional logic based on temperature,
humidity, and CO2 levels.

**What it tests:**  
This scenario tests per-node computation performance, including
expression evaluation, arithmetic processing, and conditional
execution during result projection.

---

## single_path_averaging_projection

**What the query does:**  
Traverses from each `Room` node to its associated `Floor`, computes a
comfort score per room, groups rooms by floor, and returns the average
comfort level for each floor.

**What it tests:**  
This scenario tests graph traversal and aggregation performance,
including path traversal efficiency, grouping behavior, and average
calculation across related nodes.

---

## single_path_no_change_averaging_projection

**What the query does:**  
Executes the same traversal and averaging logic as the path averaging
scenario but simulates updates that do not change the final aggregated
results.

**What it tests:**  
This scenario tests incremental query processing behavior, specifically
change detection efficiency and the system’s ability to avoid
unnecessary recomputation when results remain unchanged.

# Element Indexes
The currently supported Element Indexes options are:

- memory
- redis
- rocksdb

# Prerequisites

Before running the performance tests, ensure the following requirements
are met:

- **Rust toolchain**  
  A recent stable Rust toolchain must be installed, including `rustc`
  and `cargo`, to build and run the `query-perf` tool.

- **Redis server (optional)**  
  Required only when using the `redis` element or result index backend.
  A Redis (or Garnet) server must be running and accessible via the
  configured `REDIS_URL`.

- **RocksDB path configuration (optional)**  
  Required only when using the `rocksdb` element or result index backend.
  A writable filesystem path must be available and configured via the
  `ROCKS_PATH` environment variable for RocksDB data storage.

 # Environment Variables

The following environment variables are used to configure external
storage backends for the performance test tool:

| Variable | Description | Default |
|----------|-------------|---------|
| `REDIS_URL` | Connection string for the Redis server, used when the `redis` element or result index backend is selected. | `redis://127.0.0.1:6379` |
| `ROCKS_PATH` | Filesystem path where RocksDB will store its data, used when the `rocksdb` backend is selected. | `test-data` |

# Examples

Run a single scenario using in-memory indexes:

```bash
cargo run -- \
  --scenario single_node_property_projection \
  --element-index memory \
  --result-index memory
```

# Sample Output

Below is an example output from running the
`single_path_averaging_projection` scenario using RocksDB for both
element and result indexes.

```text
Compiling rocksdb v0.21.0
Compiling drasi-index-rocksdb v0.2.1
Compiling query-perf v0.2.3
Finished `dev` profile [unoptimized + debuginfo] target(s) in 19m 54s

Running `target/debug/query-perf --scenario single_path_averaging_projection --element-index rocksdb --result-index rocksdb --seed 42`

--------------------------------
Drasi Query Component Perf Tests
--------------------------------

Test Run Config:
TestRunConfig {
  scenario: "single_path_averaging_projection",
  element_index_type: RocksDB,
  result_index_type: RocksDB,
  seed: 42
}

--------------------------------
Scenario - single_path_averaging_projection
--------------------------------

Bootstrap Phase:
- Duration: 824 ms
- Events: 4210
- Avg duration per event: 0.19 ms
- Throughput: ~5109 events/sec

Run Phase:
- Duration: 5912 ms
- Events: 10000
- Avg duration per event: 0.59 ms
- Throughput: ~1691 events/sec

```

# Data Model

The performance test scenarios use a synthetic hierarchical
building comfort data model.

Structure:
- 10 buildings
- 10 floors per building
- 20 rooms per floor

This results in a total of 2,000 `Room` nodes.

Each `Room` node contains the following properties:
- `temperature`
- `humidity`
- `co2`

Rooms are connected to floors using the `PART_OF` relationship.
The scenarios simulate sensor-driven updates to room properties
to evaluate query execution, traversal, aggregation, and
incremental processing performance.


# Result Indexes
The currently supported Result Indexes options are:

- memory
- redis
- rocksdb
