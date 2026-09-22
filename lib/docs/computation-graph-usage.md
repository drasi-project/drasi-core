# Using ComputationGraph

[Design](computation-graph-design.md) |
[Configuration](computation-graph-configuration.md) |
[Implementation reference](computation-graph-reference.md)

Use this guide to run existing Drasi sources, queries and reactions with
ComputationGraph. For a custom processing graph without a query, see
[custom graphs](#build-a-custom-graph).

## Run a complete example

From the **drasi-core repository root**:

```bash
cargo run --locked -p drasi-lib --features computation --example computation_runtime
```

The [example source](../examples/computation_runtime.rs) is a complete Rust
program. It creates an application source, runs a query over `Order` nodes,
connects an application reaction, inserts one order and shuts down. It requires
no external database.

The output identifies `ComputationGraph` and includes an added result whose
`name` is `First order`. It also prints the current query rows and query metrics.

## Select the engine in an application

There are two separate choices:

1. Compile `drasi-lib` with its `computation` Cargo feature.
2. Select `ExecutionMode::ComputationGraph` when building the instance.

Enabling the Cargo feature alone does **not** change the default engine.
These docs describe this development branch; do not assume a published crate
with the same version number contains the branch's implementation.

For an application beside this checkout, the library dependency can be:

```toml
[dependencies]
drasi-lib = { path = "../drasi-core/lib", features = ["computation"] }
```

Add whichever source/reaction crates your application uses. Keep the Drasi
dependencies from the same checkout; the complete example above already does so.

In an existing application, keep constructing your source and reaction objects
and add the engine selection to its builder. This excerpt assumes `source`,
`query_config` and `reaction` have already been created:

```rust,ignore
use drasi_lib::{DrasiLib, ExecutionMode};

let drasi = DrasiLib::builder()
    .with_execution_mode(ExecutionMode::ComputationGraph)
    .with_source(source)
    .with_query(query_config)
    .with_reaction(reaction)
    .build()
    .await?;

drasi.start().await?;
```

The normal add, update, remove, status and result APIs now use ComputationGraph.
`drasi.execution_mode()` returns the selected engine. This is a choice made at
construction, not a way to switch an existing running instance or migrate its
stored state.

## Add a component and wait for readiness

In ComputationGraph mode, successful addition means the node was accepted.
Initialization or startup may still fail afterwards. The node and its error
remain available for inspection.

Use a handle when subsequent work requires successful creation or startup.
In this example, `drasi` is running and a source named `orders` already exists:

```rust,ignore
use drasi_lib::Query;
use std::time::Duration;

let component = drasi
    .add_query_with_handle(
        Query::cypher("order-names")
            .query("MATCH (o:Order) RETURN o.name AS name")
            .from_source("orders")
            .enable_bootstrap(false)
            .auto_start(false)
            .build(),
    )
    .await?;

tokio::time::timeout(Duration::from_secs(30), component.wait_created()).await??;
tokio::time::timeout(Duration::from_secs(30), component.start()).await??;
```

`start()` on the handle requests startup and waits for readiness.
`wait_started()` only waits: it does not start an `auto_start = false` component.
Use a caller timeout because a component can remain blocked waiting for a
dependency.

The standard `start_source`, `start_query` and `start_reaction` calls can return
after the start request or hook, before readiness is confirmed. A partially
failed instance start can leave independent components running.

The same readiness rule applies to a newly added reaction. Wait for its component
handle before producing events that it must receive; a fresh trigger reaction
does not replay changes from before its subscription was attached.

You can inspect the full component observation with `component.observed()`.
A handle belongs to one particular instance of the component; replacing the
component invalidates old handles, even if its name stays the same.

Source/reaction constructors run before their objects are added. An error from
such a constructor is different from a recorded initialization error.

## Choose source order deliberately

The order of `from_source` calls is the tie-breaker when queued events have the
same source-reported timestamp:

```rust,ignore
let query = Query::cypher("combined")
    .query("MATCH (n:Item) RETURN n.name AS name")
    .from_source("primary")
    .from_source("secondary")
    .build();
```

Here `primary` ranks before `secondary`. Within a source, its sequence number
breaks remaining ties. This orders queued events, not events that have not
arrived. See [ordering and buffers](computation-graph-configuration.md#ordering-and-buffers)
before changing source order or queue sizes.

## Read changes and current results

An application reaction receives `QueryResult` messages. Each has a query ID,
a per-query sequence and a list of `ResultDiff` changes. A change is an addition,
update, deletion, aggregation update or no-op; it is not just a JSON row.

For a current view rather than the change stream:

```rust,ignore
let rows = drasi.get_query_results("order-names").await?;
let metrics = drasi.get_query_output_metrics("order-names").await?;
```

The [application reaction guide](../../components/reactions/application/README.md)
shows the exact result shape and the receive APIs. Take its receiver only once;
create separate reactions if you need separate consumers.

## Inspect a problem

Start with the component's status and error, then look at the query's own graph:

```rust,ignore
let status = drasi.get_query_status("order-names").await?;
let query_graph = drasi.inspect_query_computation("order-names").await?;
let inventory = drasi.inspect_computation_inventory().await?;
```

The inventory includes the instance, query graphs and separately registered
graphs. Logs and metrics use the existing DrasiLib APIs. Failure objects may
contain configuration or provider details; do not publish them as sanitized
error messages without checking them.

| Symptom | First thing to check |
|---|---|
| Addition succeeded but nothing runs | Creation/start error, missing source or connection, `auto_start`, then readiness |
| `wait_started()` never completes | Whether startup was requested and whether a required dependency can become ready |
| Persistent query refuses to start | Source replay support, storage capabilities and recovery policy |
| A reaction cannot resume | Available query output history, saved progress and whether the query was reset/recreated |
| Output is slow | Queue waits versus query work versus reaction/application handling; collect profiling separately from throughput measurements |

## Stop and clean up

`stop()` stops processing without deleting the component declarations. Restart
is subject to plugin support. For example, a consumed ApplicationSource receiver
requires reconstruction rather than assuming the same object can start again.

Always finish with:

```rust,ignore
drasi.shutdown().await?;
```

Shutdown waits for graph-owned tasks and cleanup. If a shutdown wait is cancelled,
await shutdown again. Do not use dropping the instance as a substitute, including
after a failed startup.

Pausing/resuming selected graph components and applying a previewed change are
advanced operations described in the
[implementation reference](computation-graph-reference.md#changing-a-running-graph).

## Build a custom graph

You can connect sources, transformers and sinks directly; a continuous query is
not mandatory. Components declare input/output ports and the data they accept.
The graph validates the connections before starting them.

Use the [middleware transformer](computation-graph-middleware.md) to run an ordered
sequence of existing middleware on graph changes from one or more components.
It does not require a Continuous Query.

Use a [transaction transformer](computation-graph-transactions.md) when a linear
sequence of participating transformers must commit together. The sequence is
configured inside one component; there are no internal pipes. Ordinary
transformers remain independently usable and are not automatically participants.

From the drasi-core repository root:

```bash
cargo run --locked -p drasi-lib --features computation --example computation_graph
cargo run --locked -p drasi-lib --features computation --example computation_instance
```

The first [example](../examples/computation_graph.rs) builds direct processing
chains. The second [example](../examples/computation_instance.rs) hosts both
engines in one DrasiLib instance over a shared application source.

A standalone `GraphRun` must be awaited by its caller. A graph registered through
`add_computation_graph` or `with_computation_graph` has its task owned by DrasiLib.
Borrowing a source does not transfer ownership of its start/stop lifecycle.

## Use Server or the test framework

These hosts use different configuration formats; do not interchange their keys:

- [Drasi Server engine configuration](https://github.com/drasi-project/drasi-server/blob/agentofreality-parallel-computation-graph/README.md#execution-engine)
  selects the engine behind Server's existing component APIs.
- [Embedded test-framework configuration](https://github.com/drasi-project/test-infra/blob/agentofreality-parallel-computation-graph/e2e-test-framework/README.md)
  selects the engine for a particular embedded instance in a test run.

The [configuration reference](computation-graph-configuration.md) explains which
settings belong to which layer.
