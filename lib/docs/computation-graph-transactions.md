# Transaction transformer

[Design](computation-graph-design.md) |
[Usage](computation-graph-usage.md) |
[Middleware](computation-graph-middleware.md)

`TransactionTransformer` runs an ordered sequence of transformers inside one
storage transaction. It is one component in the graph, not a nested graph:

```text
upstream --> TransactionTransformer [first --> second --> third] --> downstream
```

There are **no pipes, queues or independently scheduled tasks between the steps**.
The output event from one step is passed directly to the next.

## Transaction behaviour

For each new input event, the container:

1. Begins one transaction in the supplied shared storage.
2. Runs every step in order, giving each its own storage area.
3. Saves the input position and final output in that same transaction.
4. Commits, then returns the output for delivery.

If any step fails, or processing is cancelled before commit, participating writes
roll back together. The failed instance must be reconstructed before processing
continues: rollback cannot undo a badly behaved implementation's private mutable
fields or external effects.

If the process stops after commit but before delivery is confirmed, the container
replays the saved output without running the steps again. Replay uses a new
delivery number while retaining the original batch identity and logical position.

This is **not** a transaction around the upstream source or downstream reaction.
An HTTP call or another external effect cannot be rolled back by this storage
transaction.

## Which transformers can participate

Ordinary graph transformers still implement `Transformer` and run independently.
An implementor opts into participation by additionally implementing
`TransactionalTransformer`.

The additional trait declares executable input/output schemas and provides:

```rust,ignore
async fn transform_in_transaction(
    &self,
    input: ChangeEnvelope,
    context: &TransactionContext<'_>,
) -> anyhow::Result<ChangeEnvelope>;
```

The container calls this method, **not** the ordinary `start`, `transform`,
`stop`, wakeup or delivery hooks. A transactional factory must therefore create
a ready-to-use, configuration-only step, without starting workers or opening a
separate transaction.

Each step accepts one batch and returns one batch. Filtering returns an empty
change set; expansion puts multiple ordered operations in the same event. Empty
batches still pass through the remaining steps and preserve input progress.
Adjacent schemas must match exactly. Custom schemas are supported when their
implementations supply the corresponding validators.

The container rejects empty sequences, duplicate step IDs, unknown or
non-participating implementations, incompatible schemas, multiple input/output
ports, and independently scheduled work. Sources and reactions are not members.
The container itself does not implement the participation trait, so transaction
containers cannot be nested.

`MiddlewareTransformer` implements the new trait and is in the standard
transactional registry. Its ordinary and independently durable modes remain
available. Other transformers, including the existing Continuous Query
transformer, are **not automatically transactional participants**.

## Separate state for each step

`TransactionContext` provides:

- `get`, `put` and `remove` for named `ElementValue` state.
- `get_element`, `put_element` and `remove_element` for graph-element state.
- `derive` to give an output batch an intermediate identity scoped to its step,
  container and transaction, while preserving input context and lineage.

Two steps can use the same key or graph-element identity without sharing or
overwriting each other's values. Reads see that step's earlier writes in the
current transaction. Keys and stored node/relationship references are scoped
internally; emitted data keeps its original identities.

The context is borrowed only for the current call. It does not expose the
underlying provider, another step's storage, or begin/commit/rollback operations.
There is no cross-step state access: steps communicate through their output
events.

Implementors must keep all commit-sensitive state in this context. They must
not perform external side effects or background writes. The trait is an explicit
implementation contract, not a way to make arbitrary existing code atomic.
For middleware, this also applies to any custom middleware supplied by the
application.

## Configure a sequence

The definition contains the sequence and output settings; the storage provider
is supplied separately. Enable the middleware features used by the steps;
ComputationGraph itself is always available. This example uses
`middleware-decoder` and `middleware-parse-json`.

```rust,ignore
use std::{num::NonZeroUsize, sync::Arc};
use drasi_lib::computation::v1::*;

let registry = Arc::new(TransactionalTransformerRegistry::standard(
    drasi.middleware_registry(),
));

let definition = TransactionTransformerDefinition {
    graph_id: "processing".into(),
    id: ComponentId::try_new("decode-and-parse")?,
    output_stream: StreamId::try_new("decode-and-parse/out")?,
    outbox_capacity: NonZeroUsize::new(128).expect("positive capacity"),
    steps: vec![
        TransactionStepDefinition {
            id: ComponentId::try_new("decode")?,
            implementation: ImplementationIdentity::try_new("drasi/middleware-transformer", "1")?,
            configuration_version: 1,
            configuration: serde_json::json!({
                "middleware": [{
                    "name": "decode-value",
                    "kind": "decoder",
                    "config": {
                        "encoding_type": "base64",
                        "target_property": "encoded",
                        "output_property": "decoded",
                        "on_error": "fail"
                    }
                }],
                "pipeline": ["decode-value"]
            }),
        },
        TransactionStepDefinition {
            id: ComponentId::try_new("parse")?,
            implementation: ImplementationIdentity::try_new("drasi/middleware-transformer", "1")?,
            configuration_version: 1,
            configuration: serde_json::json!({
                "middleware": [{
                    "name": "parse-value",
                    "kind": "parse_json",
                    "config": {
                        "target_property": "decoded",
                        "output_property": "parsed",
                        "on_error": "fail"
                    }
                }],
                "pipeline": ["parse-value"]
            }),
        },
    ],
};

let transformer = TransactionTransformer::new(
    definition,
    registry,
    provider, // Arc<dyn ComputationIndexProvider>
).await?;
```

Applications can register additional `TransactionalTransformerFactory`
implementations. Such factories return `Box<dyn TransactionalTransformer>`;
a boolean configuration flag cannot turn an ordinary transformer into one.

## Storage, connections and recovery

The initial implementation requires **persistent storage with a complete atomic
transaction**, such as the existing RocksDB computation provider or an atomic
index plugin through `LegacyIndexProviderAdapter`. All steps use one index bundle
and session. Separate providers or sessions are not combined into a distributed
transaction.

Input connections must preserve per-stream order and apply backpressure. Every
output connection must provide durable acceptance, replay, explicit
acknowledgement and backpressure. A memory-only bounded pipe is not sufficient;
the [durable middleware guide](computation-graph-middleware.md#durable-state-and-delivery)
describes the retained-pipe setup shared by both components.

The graph calls `delivery_completed` only after every outgoing branch accepts
the batch. Unconfirmed output cannot be evicted when retention is full. Direct
callers must drain recovered output and continuations, then confirm only actual
durable acceptance; this is not acknowledgement of a reaction's external effects.

`with_source_progress` binds replay-capable streaming sources to the container's
committed input. As with durable middleware, this is replay-only source wiring,
not a new bootstrap API.

For query-result inputs, the container checks the persistent query's identity,
reset generation and result sequence rather than an inherited raw-source cursor
or a replay delivery number. Volatile query results and snapshot/skip control
batches are rejected. Generic producers must supply stable stream identities and
sequences across restart. A preceding durable transformer instead supplies its
verified logical output position.

Reopen with the same graph/component identity, ordered step IDs, implementations,
versions, configuration, schemas and capacity. A changed definition is rejected,
not silently applied to saved state. Use a separate storage scope for an unrelated
pipeline. Each input, intermediate batch and final output is limited to the
existing 64 MiB envelope encoding limit.

## Declarative graph construction

`TransactionTransformerFactory` is registered in `FactoryRegistry::standard()`.
Use:

```rust,ignore
let specification = definition.specification(
    &registry,
    ResourceId::try_new("transformer-registry")?,
    ResourceId::try_new("shared-storage")?,
)?;
```

Provide `TransactionalTransformerRegistryResource` with role `Component` and
`QueryIndexProviderResource` with role `IndexBackend`. The latter is the existing
provider wrapper; no hidden query is created. An optional `source_progress`
dependency can supply the container's `QuerySourceProgressResource`.

The registry constructs configuration-only steps to validate the declared
ports. Factory construction must not perform I/O or side effects.
`step_descriptors()` exposes the step names and interfaces for inspection without
pretending they are independently running graph nodes.

## Checks

From the drasi-core repository root:

```bash
cargo test --locked -p drasi-lib \
  --features computation-rocksdb-tests,middleware-all \
  --test computation_transaction_transformer
```

The tests cover one shared commit, isolated state, failure and cancellation
rollback, uncertain commit recovery, retained output replay, filtering/expansion,
custom schemas, durable graph construction and Unwind state after reconstruction.
