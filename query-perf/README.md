# Introduction
The Component Performance Test Tool is designed to allow you to run large scale / high volume tests with minimal overhead in order to evaluate the raw performance of componets of the query processing subsystem (e.g. query solver, element index, etc). 

This allows us to establish a baseline understanding of raw component performance and provides a metric on which to compare different component implementations (i.e. rocks vs redis vs garnet indexes) and to evaluate the effect of code changes to the performance of these components.

The tool is structured around the concept of a Performance Test Scenario, which represents the combination of a specific Continuous Query against a data model. The same Performance Test Scenario with the same expected outcomes can then be run against different component configurations simply by specifying the desired component configuration using command line parameters.

## Prerequisites

Before running these scenarios, ensure the following requirements are met.

### Rust Toolchain

- Rust **stable** toolchain installed  
- Recommended version: latest stable  
- Verify with:
  ```bash
  rustc --version
  cargo --version
-   (Optional) Redis server running if using `--element-index redis` or `--result-index redis`.


No external system libraries are required for the `memory` or `rocksdb` backends.

## Execution

You can run the Component Performance Test Tool either from a compiled binary or using Cargo.

### Using Compiled Binary
If you prefer to run the binary directly (e.g., in a production environment):

1.  **Build the project:**
    ```bash
    cargo build --release -p query-perf
    ```
2.  **Copy the build to current directory:**
    ```bash
    cp ../target/release/query-perf .
    # Note: The 'space dot' at the end is critical! It means "copy to here".
    ```
3.  **Typical Command Structure:**
    ```bash
    ./query-perf --release -- --scenario <SCENARIO> --element-index <INDEX> --result-index <INDEX>
    ```

### Using Cargo
```bash
cargo run --release -- <ARGS>
```
*Note: We recommend using `--release` for accurate performance metrics.*

**Typical Command Structure:**
```bash
cargo run -p query-perf --release -- --scenario <SCENARIO> --element-index <INDEX> --result-index <INDEX>
```


### Command Line Arguments Description

| Argument | Description |
|----------|-------------|
| `-s <SCENARIO>`, `--scenario <SCENARIO>` | The Performance Test Scenario to run. To run all scenarios in sequence, use the value "all". |
| `-e <INDEX>`, `--element-index <INDEX>` | The element index implementation to use: `memory`, `redis`, or `rocksdb`. |
| `-r <INDEX>`, `--result-index <INDEX>` | The result index implementation to use: `memory`, `redis`, or `rocksdb`. |
| `-i <N>`, `--iterations <N>` | (Optional) The number of source change events to simulate. Defaults to scenario specific value (usually 10,000). |
| `--seed <SEED>` | (Optional) Random number seed. Using the same seed ensures deterministic source change sequences. |
| `-h`, `--help` | Show help message |

# Environment Variables
The tool uses the following environment variables for configuration if the corresponding backends are selected:

| Variable | Description | Default |
| :--- | :--- | :--- |
| `REDIS_URL` | Connection URL for Redis server | `redis://127.0.0.1:6379` |
| `ROCKS_PATH` | Directory path for RocksDB storage | `test-data` |


# Scenarios
The currently defined Performance Test Scenarios are:

## single_node_property_projection
**The Baseline.**
Monitors all `Room` nodes and projects their raw property values (`temperature`, `humidity`, `co2`).

**Query Explanation:** Matches every `Room` node and projects its identifier and environmental properties (`name`, `temperature`, `humidity`, `co2`).

**What it Tests:**
Establishes the baseline performance for direct node access and property retrieval. This scenario isolates the I/O and deserialization overhead of the element index without introducing complex filtering or transformation logic.

## single_node_calculation_projection
**The Calculator.**
Monitors `Room` nodes but computes a derived "Comfort Index" on the fly using arithmetic and conditional logic.

**Query Explanation:** Calculates a derived `ComfortLevel` for each `Room` using arithmetic operations on properties and classifies it using a conditional `CASE` expression.

**What it Tests:**
Evaluates the computational performance of the expression evaluation engine. This scenario measures the overhead of scalar arithmetic and conditional logic processing per event, independent of complex graph traversals.

## single_path_averaging_projection
**The Aggregator.**
Joins `Room` nodes to their parent `Floor` nodes and calculates the average comfort index per floor.

**Query Explanation:** Matches `Room` nodes connected to `Floor` nodes via the `PART_OF` relationship. Results are grouped by `Floor` to calculate the average comfort level of associated rooms.

**What it Tests:**
Assesses the performance of pattern matching (IO-bound) and aggregation (memory/CPU-bound). This scenario exercises the join algorithms for one-hop relationships and tests the efficiency of incremental state maintenance for aggregation functions.

## single_path_no_change_averaging_projection
**The Filtered Aggregator.**
Similar to the averaging projection, but consistently updates room data in a way that the *average* for the floor remains constant (e.g., oscillating values).

**Query Explanation:** Identical to the Aggregator query, but executed against a data stream designed to produce oscillating inputs that result in stable aggregate outputs.

**What it Tests:**
Validates the changelog suppression capabilities of the system. This scenario ensures that internal state updates which result in no net change to the projected output are correctly identified and do not trigger redundant downstream events.

# Examples & Sample Output

### Run a scenario with Memory indexes
```bash
cargo run --release -p query-perf -- --scenario single_node_property_projection --element-index memory --result-index memory -i 1000
```

### Sample Output
```text
 - Initializing Scenario...
 - Bootstrapping Scenario...
 - Running Scenario... 
 - Result: TestRunResult {
    _scenario_name: "single_node_property_projection",
    _element_index_type: Memory,
    _result_index_type: Memory,
    bootstrap_duration_ms: 107,
    bootstrap_events: 4210,
    avg_bootstrap_events_per_sec: 39345.79,
    run_duration_ms: 95,
    run_events: 1000,
    avg_run_events_per_sec: 10526.31,
}
```
***Note:** Times associated with 'bootstrap' represent the time taken to load the initial dataset (2000 rooms + buildings). 'Run' metrics represent the performance of processing the dynamic updates.*

# Data Model
All scenarios operate on a simulated **Building Comfort Model** with the following hierarchy:
-   **10 Buildings**
-   **10 Floors** per Building
-   **20 Rooms** per Floor
-   **Total:** 2,000 Rooms

Each `Room` node has dynamic properties: `temperature`, `humidity`, and `co2`.
