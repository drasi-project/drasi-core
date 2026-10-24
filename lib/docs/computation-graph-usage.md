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
cargo run --locked -p drasi-lib --example computation_runtime
```

The [example source](../examples/computation_runtime.rs) is a complete Rust
program. It adds a native application source, query and application reaction to
DrasiLib's existing graph, connects their declared ports, inserts one order and
shuts down. It uses no application Source/Reaction plugin adapters and requires
no external database.

The output identifies `ComputationGraph`, reports one result change, and prints
the current row with `name` equal to `First order`, followed by query metrics.

## Use the library in an application

ComputationGraph is the only runtime; no feature or builder selector chooses an
engine. It remains available with `--no-default-features`. These docs describe
this development branch, not necessarily a published crate with the same version.

For an application beside this checkout, the library dependency can be:

```toml
[dependencies]
drasi-lib = { path = "../drasi-core/lib" }
```

Add whichever source/reaction crates your application uses. Keep the Drasi
dependencies from the same checkout; the complete example above already does so.

In an existing application, keep constructing your source and reaction objects.
This excerpt assumes `source`,
`query_config` and `reaction` have already been created:

```rust,ignore
use drasi_lib::DrasiLib;

let drasi = DrasiLib::builder()
    .with_source(source)
    .with_query(query_config)
    .with_reaction(reaction)
    .build()
    .await?;

drasi.start().await?;
```

The add, update, remove, status and result APIs use the instance's ComputationGraph.

### Assemble queries from native inputs

For a native source already owned by the instance graph, bind its actual output
endpoint to the shared query builder:

```rust,ignore
use drasi_lib::{Query, computation::v1::*};

let source_output = Endpoint::new(
    ComponentId::try_new("orders")?,
    PortId::try_new("out")?,
);
let batch = drasi.computation_pipeline()?
    .native_source(source_output)?
    .query(Query::cypher("order-names")
        .query("MATCH (o:Order) RETURN o.name AS name")
        .from_source("orders")
        .build())
    .build()?;
let report = drasi.add_components(batch).await?;
anyhow::ensure!(report.committed, "{report:?}");
```

This reuses the existing query evaluator, ranked input, indexes, middleware,
scheduling and result outlet. It does not create a Source-plugin reader, and
query stop, replacement and removal do not own the native source. Multiple
queries can bind the same output. The source may be declared later: the required
connection remains visible and resolves through the existing graph controller.
Disabled auto-start and explicit stops remain effective, including when a waiting
query is updated before its source arrives.

Use `query_bootstrap(query_id, provider)` for an explicit application-owned
snapshot. This requires enabled bootstrap and native inputs; no snapshot is
invented from the live channel. Explicit Source-plugin `nodes`/`relations`
subscription selectors are rejected on native bindings rather than ignored;
normal labels in the query text still work. These inputs use bounded graph
pipes. A durable admitted source instead requires its actual QoS connections,
as shown in the [admission documentation](computation-graph-qos.md).

### Read live query results without a reaction

For an application/API reader, subscribe directly to a query's existing result
outlet instead of adding a temporary application reaction:

The example uses `futures = "0.3"` for stream iteration.

```rust,ignore
use futures::StreamExt;

let subscription = drasi.subscribe_query_results("order-names").await?;
println!("Initial rows: {:?}", subscription.snapshot.to_vec());
let mut results = subscription.stream;
while let Some(result) = results.next().await {
    let result = result?;
    println!("{}: {:?}", result.query_id, result.results);
}
```

Subscription waits for readiness and registers its reader before capturing an
initial snapshot. Apply that snapshot first; the stream contains only changes
after its `as_of_sequence`, with no snapshot/live gap or repeated earlier changes.
Channel delivery applies the configured bounded backpressure; Broadcast delivery
replays transport gaps from the existing query outbox and fails explicitly if
the required history has been evicted. It never silently skips missing results.

The stream adds no graph node, output edge, journal member or worker. Dropping it
unregisters the reader synchronously, even without a first poll. The reader is
bound to the query's component/output generation, so replacement, removal, reset
or instance shutdown invalidates it rather than leaking old results. It is not
a durable consumer checkpoint or confirmation of application-side effects.
Reconnecting creates a fresh snapshot/stream pair, not a resume token for effects.
Use a native application reaction when application handling belongs in the graph.

## Add a component and wait for readiness

Successful addition means the node was accepted.
Initialization or startup may still fail afterwards. The node and its error
remain available for inspection.

Missing dependencies are a waiting condition, not a terminal error. You can
declare a reaction before its query or a query before its source. Inspect
`component.observed()?.waiting_for` or
`component.observed()?.waiting_reason()` to see what is missing. When the
dependency becomes available, the graph constructs and starts eligible nodes
without re-adding them or requesting a retry. Auto-start settings and explicit
stops still apply. Unconnected ports need an explicit connection, not another
component declaration with a guessed connection.

`wait_created()` and `wait_started()` really wait for those states. Bound a
wait with a timeout if the dependency may never be supplied; timing out the
caller does not discard the node, its accepted configuration or its cleanup
owner. A real constructor/processing failure is still an error and is not
retried merely because an unrelated dependency changes.

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

Queries assembled with `computation_pipeline()` or the built-in continuous-query
factory are listed by `list_queries()` and expose configuration, status, results,
retained output, metrics, logs and events through the ordinary query APIs. These
readers use the graph-owned query's existing state; reading does not construct a
second evaluator. A stopped query remains discoverable, but result reads report
that it is not running.

Live subscriptions, ordinary reactions and Server's query attach stream require
a connected query-results outlet. A result catalogue by itself provides lookup,
not live delivery. Pipeline assembly supplies the outlet automatically.
`start_query` and `stop_query` include the query's dedicated subscription readers,
timer source and result outlet; they do not stop a shared application source or
another query's work.
Pipeline queries also support `update_query` and `remove_query`. Replacement
rewires subscription readers and retains transport ordering without stopping
shared sources or sibling queries. Replacing an ordinary source reconnects all
of its pipeline queries together. Reactions can be updated to consume these
queries through the normal reaction API.

Removal rejects dependent consumers, stops the query's dedicated helpers,
erases its owned processing state and removes unused resource declarations.
Ordinary stop and instance shutdown retain processing state. Custom native
components must implement `deprovision` to support permanent state removal;
arbitrary factory/input rewiring still uses explicit computation operations.
QueryConfig replacement applies to pipeline queries, not opaque external
query implementations or custom graph input layouts.

Replacement does not override consumer recovery policy. A strict reaction
with a checkpoint from the previous query generation rejects a reset rather
than silently abandoning its progress. Query configuration can be committed
even when restarting such a consumer fails; inspect the affected nodes.

Factory-created declarations retain their configuration when construction
fails. Native wrappers can forward the optional `QueryApi` read capability from
their query implementation; declaring a query role alone does not manufacture
result access.
Native Source/Reaction roles also participate in ordinary inventory, status,
diagnostics, start/stop and removal APIs. Internal subscription readers and
timer helpers are not extra public Sources. Available configuration is read
from the graph's captured values or factory recipe; an opaque external binding
remains visible and has diagnostics even when its full configuration is
unavailable. Descriptive snapshots include these components, while complete
reconstruction exports retain their native definitions exactly once.

A native graph role does not manufacture the legacy Source/Reaction plugin
interface. Operations requiring that interface, including replacement with
an ordinary plugin object, report the limitation explicitly; custom native
factory and connection changes use computation operations.

## Inspect a problem

Start with the component's status and error, then inspect its instance graph:

```rust,ignore
let status = drasi.get_query_status("order-names").await?;
let graph = drasi.inspect_query_computation("order-names").await?;
let inventory = drasi.inspect_computation_inventory().await?;
```

The inventory contains the instance's one ComputationGraph, including ordinary
queries and their helpers. `inspect_query_computation` validates the query and
returns that same graph's inspector; it does not create or expose a private query graph.
Logs and metrics use the DrasiLib APIs. Failure objects may
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
is subject to the component's restart contract. ApplicationSource retains its
input handles and buffered work across stop/start.

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
cargo run --locked -p drasi-lib --example computation_graph
cargo run --locked -p drasi-lib --example computation_instance
```

The first [example](../examples/computation_graph.rs) builds direct processing
chains. The second [example](../examples/computation_instance.rs) runs two native
queries from one native application source within one DrasiLib instance. Stopping
one query leaves the source and the other query running; restarting it consumes
the buffered change. It needs no borrowed Source-plugin host or cross-instance
transport.

A standalone `GraphRun` must be awaited by its caller. Within DrasiLib, submit
component batches to its owned ComputationGraph:

```rust,ignore
use drasi_lib::computation::v1::ComponentBatch;

let batch = ComponentBatch::builder()
    .source(source)
    .transformer(transformer)
    .sink(sink)
    .bind_stream(source_output, source_stream)
    .bind_stream(transformer_output, transformer_stream)
    .connect(source_to_transformer, input_pipe)
    .connect(transformer_to_sink, output_pipe)
    .build()?;
let report = drasi.add_components(batch).await?;
```

`with_components(batch)` supplies a batch during instance construction.
`computation_pipeline()` assembles existing source/query/reaction contracts into
a batch. Component IDs share the instance namespace; batches do not define
independent execution owners. Borrowing a source does not transfer ownership
of its start/stop lifecycle.

## Use Server or the test framework

These hosts use different configuration formats; do not interchange their keys:

- [Drasi Server runtime configuration](https://github.com/drasi-project/drasi-server/blob/agentofreality-parallel-computation-graph/README.md#execution-engine)
  uses ComputationGraph for all instances; remove old engine selectors.
- [Embedded test-framework configuration](https://github.com/drasi-project/test-infra/blob/agentofreality-parallel-computation-graph/e2e-test-framework/README.md)
  configures embedded instances without selecting an execution engine.

The [configuration reference](computation-graph-configuration.md) explains which
settings belong to which layer.
