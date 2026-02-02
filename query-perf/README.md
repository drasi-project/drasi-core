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
- (Optional) Redis server running if using `--element-index redis` or `--result-index redis`.


No external system libraries are required for the `memory` or `rocksdb` backends.

## Redis server setup (if using redis backend)
If you plan to use Redis backend, you need a running Redis instance.

#### 1. Installation
*   **macOS:**
    ```bash
    brew install redis
    brew services start redis
    ```
*   **Ubuntu/Debian:**
    ```bash
    sudo apt-get install redis-server
    sudo systemctl start redis-server
    ```
*   **Docker:**
    ```bash
    docker run -d -p 6379:6379 --name drasi-redis redis:latest
    ```

#### 2. Verification (Ping-Pong)
Ensure Redis is reachable before running tests:
```bash
redis-cli ping
# Expected Output: PONG
```

## RocksDB path configuration (if using RocksDB backend)
If you use the RocksDB backend (`--element-index rocksdb` or `--result-index rocksdb`), the tool creates a persistent database on disk.

#### Default Location
By default, the tool creates a directory named `test-data` in your current working directory.

#### Custom Location
You can change the storage location by setting the `ROCKS_PATH` environment variable:
```bash
ROCKS_PATH=/tmp/my-custom-rocksdb cargo run --release -- --scenario <SCENARIO> --element-index <INDEX> --result-index <INDEX>
# Note: (/tmp/my-custom-rocksdb) is the custom path
```

#### Cleanup
The database persists between runs. If you want to start fresh (e.g., to clear indices), simply delete the directory:
```bash
rm -rf test-data
# or your custom path
rm -rf /tmp/my-custom-rocksdb
```

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
    # Note: The 'space dot' at the end is critical! It means "copy to here".
    ```
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
| `-i <N>`, `--iterations <N>` | (Optional) The number of source change events to simulate. Defaults to scenario-specific value (usually 10,000). |
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

### `single_node_property_projection`
**The Baseline.**
Monitors all `Room` nodes and projects their raw property values (`temperature`, `humidity`, `co2`).

**Query Explanation:** Matches every `Room` node and projects its identifier and environmental properties (`name`, `temperature`, `humidity`, `co2`).

**What it Tests:**
Establishes the baseline performance for direct node access and property retrieval. This scenario isolates the I/O and deserialization overhead of the element index without introducing complex filtering or transformation logic.

### `single_node_calculation_projection`
**The Calculator.**
Monitors `Room` nodes but computes a derived comfort level on the fly using arithmetic and conditional logic.

**Query Explanation:** Calculates a derived `ComfortLevel` for each `Room` using arithmetic operations on properties and classifies it using a conditional `CASE` expression.

**What it Tests:**
Evaluates the computational performance of the expression evaluation engine. This scenario measures the overhead of scalar arithmetic and conditional logic processing per event, independent of complex graph traversals.

### `single_path_averaging_projection`
**The Aggregator.**
Joins `Room` nodes to their parent `Floor` nodes and calculates the average comfort level per floor.

**Query Explanation:** Matches `Room` nodes connected to `Floor` nodes via the `PART_OF` relationship. Results are grouped by `Floor` to calculate the average comfort level of associated rooms.

**What it Tests:**
Assesses the performance of pattern matching (IO-bound) and aggregation (memory/CPU-bound). This scenario exercises the join algorithms for one-hop relationships and tests the efficiency of incremental state maintenance for aggregation functions.

### `single_path_no_change_averaging_projection`
**The Filtered Aggregator.**
Similar to the averaging projection, but consistently updates room data in a way that the *average* for the floor remains constant (e.g., oscillating values).

**Query Explanation:** Identical to the Aggregator query, but executed against a data stream designed to produce oscillating inputs that result in stable aggregate outputs.

**What it Tests:**
Validates the changelog suppression capabilities of the system. This scenario ensures that internal state updates which result in no net change to the projected output are correctly identified and do not trigger redundant downstream events.

# Examples & Sample Output

### Run a scenario with Memory index (Using Compiled Binary)
```bash
./query-perf --scenario single_node_property_projection --element-index memory --result-index memory -i 1000
```

### Sample Output

<img width="1101" height="547" alt="Screenshot 2026-02-02 at 12 27 49 AM" src="https://github.com/user-attachments/assets/04553c44-b852-436d-92d8-92b4af2b18f3" />


### Run a scenario with Redis index (Using Cargo)
```bash
cargo run --release -p query-perf -- --scenario single_node_property_projection --element-index redis --result-index redis -i 1000
```
### Sample Output

<img width="1104" height="588" alt="Screenshot 2026-02-01 at 11 22 53 PM" src="https://github.com/user-attachments/assets/23d06d7a-5836-4ae9-a6f1-248ed66e37fb" />

### Run a scenario with RocksDB index (Using Cargo)
```bash
cargo run --release -p query-perf -- --scenario single_node_property_projection --element-index rocksdb --result-index rocksdb -i 1000
```
### Sample Output

<img width="1105" height="589" alt="Screenshot 2026-02-01 at 11 46 18 PM" src="https://github.com/user-attachments/assets/7b33dd12-b6fc-466c-a200-921202fb9468" />

**Note:** Times associated with 'bootstrap' represent the time taken to load the initial dataset (2000 rooms + buildings). 'Run' metrics represent the performance of processing the dynamic updates.

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

