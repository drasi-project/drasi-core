# Introduction
The Component Performance Test Tool is designed to allow you to run large scale / high volume tests with minimal overhead in order to evaluate the raw performance of componets of the query processing subsystem (e.g. query solver, element index, etc). 

This allows us to establish a baseline understanding of raw component performance and provides a metric on which to compare different component implementations (i.e. rocks vs redis vs garnet indexes) and to evaluate the effect of code changes to the performance of these components.

The tool is structured around the concept of a Performance Test Scenario, which represents the combination of a specific Continuous Query against a data model. The same Performance Test Scenario with the same expected outcomes can then be run against different component configurations simply by specifying the desired component configuration using command line parameters.

# Execution
You can run the Component Performance Test Tool from a compiled binary:

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
TODO

## single_node_calculation_projection
TODO

## single_path_averaging_projection
TODO

## single_path_no_change_averaging_projection
TODO

# Element Indexes
The currently supported Element Indexes options are:

- memory
- redis
- rocksdb

# Result Indexes
The currently supported Result Indexes options are:

- memory
- redis
- rocksdb
