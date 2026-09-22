# Middleware transformer

[Usage](computation-graph-usage.md) |
[Configuration](computation-graph-configuration.md) |
[Implementation reference](computation-graph-reference.md)

`MiddlewareTransformer` runs the same middleware used by Continuous Queries,
without requiring a query. Its input and output are graph-node and relationship
changes using `GraphChangeCodec`.

```text
Producer A --\
             >-- MiddlewareTransformer --> query, transformer or sink
Producer B --/
```

Each producer must expose a graph-change output port. It can be a source or
another transformer. Query-result rows and custom record formats are rejected
as incompatible; they are not silently converted into graph nodes.

## Configure a sequence

Enable `computation` and the middleware features you use. For example:

```toml
drasi-lib = { path = "../drasi-core/lib", features = [
  "computation",
  "middleware-decoder",
  "middleware-parse-json",
  "middleware-promote"
] }
```

Configuration follows the query middleware format:

- `middleware` declares named middleware instances with `kind`, `name` and `config`.
- `pipeline` lists those names in execution order.
- Every connected producer uses this sequence. Use separate transformers if
  different producers need different sequences.

With an existing `drasi` instance:

```rust,ignore
use drasi_lib::computation::v1::{
    ComponentId, MiddlewareTransformer, MiddlewareTransformerDefinition, StreamId,
};

let definition = MiddlewareTransformerDefinition {
    id: ComponentId::try_new("normalize")?,
    output_stream: StreamId::try_new("normalize/out")?,
    middleware: serde_json::from_value(serde_json::json!([
        {
            "kind": "decoder",
            "name": "decode",
            "config": {
                "encoding_type": "base64",
                "target_property": "encoded",
                "output_property": "decoded",
                "on_error": "fail"
            }
        },
        {
            "kind": "parse_json",
            "name": "parse",
            "config": {
                "target_property": "decoded",
                "output_property": "parsed",
                "on_error": "fail"
            }
        },
        {
            "kind": "promote",
            "name": "extract",
            "config": {
                "mappings": [{"path": "$.parsed.value", "target_name": "value"}],
                "on_conflict": "overwrite",
                "on_error": "fail"
            }
        }
    ]))?,
    pipeline: vec!["decode".into(), "parse".into(), "extract".into()],
};

let transformer =
    MiddlewareTransformer::new(definition, drasi.middleware_registry())?;
```

This constructor keeps state in memory. Use the durable constructor below when
the pipeline must recover after a process restart. Persistent queries reject
known volatile middleware output rather than treating that pipeline as recoverable.

For a node with `encoded: "eyJ2YWx1ZSI6NDJ9"`, this adds decoded JSON text,
a parsed object and a top-level `value: 42`. Reversing the sequence does not
produce the same result: parsing needs the decoder's output first.

The registry contains the middleware enabled in the build. Applications can also
supply a `MiddlewareTypeRegistry` with custom `SourceMiddlewareFactory`
implementations. Unknown kinds, duplicate definitions, missing pipeline names
and invalid middleware configurations return errors.

## Connect components

The transformer has an input named `in` and an output named `out`.
Multiple producers can connect to `in`. Bind `out` to the same stream ID used
in its definition.

The following example uses memory-only middleware and connections. Durable
middleware has stronger connection requirements, described below.

This graph fragment assumes `producer_a`, `producer_b` and `consumer` are already
constructed boxed graph components with matching ports and graph-change formats:

```rust,ignore
use drasi_lib::computation::v1::*;

let endpoint = |component: &str, port: &str| -> anyhow::Result<Endpoint> {
    Ok(Endpoint::new(
        ComponentId::try_new(component)?,
        PortId::try_new(port)?,
    ))
};

let mut graph = ComputationGraph::builder("normalization")
    .source(producer_a)
    .source(producer_b)
    .transformer(Box::new(transformer))
    .sink(consumer)
    .bind_stream(endpoint("a", "out")?, StreamId::try_new("a/out")?)
    .bind_stream(endpoint("b", "out")?, StreamId::try_new("b/out")?)
    .bind_stream(endpoint("normalize", "out")?, StreamId::try_new("normalize/out")?)
    .connect(
        EdgeDefinition::new(endpoint("a", "out")?, endpoint("normalize", "in")?),
        Box::new(BoundedPipeConfig { capacity: 128 }),
    )
    .connect(
        EdgeDefinition::new(endpoint("b", "out")?, endpoint("normalize", "in")?),
        Box::new(BoundedPipeConfig { capacity: 128 }),
    )
    .connect(
        EdgeDefinition::new(endpoint("normalize", "out")?, endpoint("consumer", "in")?),
        Box::new(BoundedPipeConfig { capacity: 128 }),
    )
    .build()?;

graph.start()?.await?;
graph.dispose().await?;
```

The finite-source version of this arrangement is covered by the
[transformer tests](../tests/computation_middleware.rs).
For a running instance, the component can also be supplied to
`add_transformer_with_handle`, with its ports connected through graph control.

The standard DrasiLib source/query/reaction wrapper nodes are services without
data ports. Connect the appropriate graph source adapter or a component exposing
real data ports, not an invented port on one of those wrappers.
This is not a new Server YAML source/reaction kind.

## Which middleware fits

All seven existing kinds operate on this graph-change format:

| Kind | Cargo feature | Behaviour |
|---|---|---|
| `decoder` | `middleware-decoder` | Decode an encoded property value |
| `parse_json` | `middleware-parse-json` | Parse a string property into a structured value |
| `promote` | `middleware-promote` | Copy selected nested properties to named top-level properties |
| `relabel` | `middleware-relabel` | Change node/relationship labels, including delete metadata |
| `map` | `middleware-map` | Filter or reshape changes; can produce several nodes/relationships and rewrite identities |
| `jq` | `middleware-jq` | Filter or reshape changes using jq; can produce several graph changes |
| `unwind` | `middleware-unwind` | Expand arrays into child nodes and optional relationships; remove old children on updates/deletes |

`map` and `jq` are not limited to editing one property. Their output remains
ordered graph changes. `unwind` is not stateless: it reads previous parent
elements to determine which child records must be deleted.

The transformer therefore keeps previous emitted elements in an owned index:
in memory with `new`, or in persistent storage with `new_durable`. Patch updates
merge missing properties when updating this saved view, as the query does after
middleware processing. Middleware-specific skip/fail settings and patch
semantics remain those of the existing implementations.
See the [middleware reference](../../middleware/README.md) for their configurations.

For Unwind, include the current selected array in parent updates. The existing
middleware treats an omitted array as an empty selection and deletes the old
children; omission does not mean "leave the children unchanged". A later pipeline
step must also retain the parent/child records that Unwind will need to look up
on the next update or deletion, including their identities and selected array
properties.

`relabel` followed by `unwind` works when the renamed label matches Unwind's
configuration. Two Unwind steps can expand nested arrays when the second step
matches the child labels produced by the first. Their derived changes retain
the middleware's order, including child changes before their parent.

## Durable state and delivery

Use `new_durable` with a persistent `ComputationIndexProvider`. The provider must
commit element changes, input progress and saved outputs together. An in-memory,
incomplete or non-atomic provider is rejected; there is no fallback to memory.

Replace the earlier constructor with:

```rust,ignore
use std::num::NonZeroUsize;
use drasi_lib::computation::v1::DurableMiddlewareOptions;

let transformer = MiddlewareTransformer::new_durable(
    definition,
    drasi.middleware_registry(),
    provider, // Arc<dyn ComputationIndexProvider> backed by persistent storage
    DurableMiddlewareOptions {
        graph_id: "normalization".into(),
        outbox_capacity: NonZeroUsize::new(1024).expect("positive capacity"),
    },
).await?;
```

An existing persistent index plugin can be supplied through
`LegacyIndexProviderAdapter`. Use an isolated provider scope for each logical
graph; the middleware exclusively owns its returned index bundle.

Durable mode requires:

- Input connections that preserve each stream's order and apply backpressure.
- **Every output connection** to provide durable acceptance, replay, explicit
  acknowledgement, backpressure and per-stream ordering. A `BoundedPipeConfig`
  alone is not enough. Use a `RetainedPipeConfig` with an `IndexedEnvelopeStore`
  and a separate persistent index bundle for each connection.
- The same graph/component identity, output stream and configuration when
  reopening existing state, including retention capacity. Changed ownership or
  configuration is rejected.
- Restart-stable input stream IDs and sequence numbers when the producer sends
  generic graph events without raw source metadata. Middleware cannot recover a
  producer that silently resets those identities.

The graph enforces these connection requirements. It confirms a middleware
output only after every outgoing branch has accepted it durably. This does not
mean that every reaction has finished its external action.

On restart, middleware restores its previous elements and replays saved output
whose delivery was not confirmed. It does **not** run the middleware again on
that already-committed input. This covers both lost delivery and lost delivery
acknowledgements. Replay precedes new input, and preserves the saved batch's
identity while assigning a new delivery number. Downstream queries use the saved
middleware output position to avoid applying that batch twice.
If a repeated input's confirmed output has already left the middleware's retained
history, it can produce no new output: the durable outgoing connections already
own any remaining delivery. This is not permission to discard unconfirmed output.

`outbox_capacity` counts input batches, including empty filtered batches. Pending
output is never evicted just to make room. Direct calls return
`MiddlewareRecoveryError::RetentionExhausted` when unconfirmed output fills the
limit; deliver and confirm it before retrying the input.

For replay-capable source adapters, bind a `QuerySourceProgress` resource owned
by the middleware through `with_source_progress`. Those sources must resume from
the middleware's committed input, not from a downstream query's different
positions. Configure these adapters with `enable_bootstrap=false`: this is a
replay-only streaming path, not a middleware bootstrap/reset API. The graph
rejects a source checkpoint binding that bypasses its immediate consumer.

When calling the transformer directly instead of through the graph, drain
`on_wakeup`/`continue_transform` output before new input and call
`delivery_completed(&outputs)` only after **all** branches accept durably.
The graph driver performs these steps automatically.

The [durability tests](../../lib-integration-tests/tests/computation_middleware_durability.rs)
show real storage reopening and Unwind cleanup. The
[recovery tests](../tests/computation_middleware_recovery.rs) include complete
durable graph construction, fanout, source replay and injected storage failures.

## Event behaviour and limits

- Each new input produces one saved batch containing all expanded changes.
  Expansion does not split one source sequence across several output events;
  recovery can deliver that same batch again.
- If all changes are filtered out, the output is an empty change batch. This lets
  downstream queries record input progress without inventing a data change.
- The input event is not modified. Its annotations, source metadata, profiling,
  event time and source position are preserved, along with the reference to
  the input event.
- The output uses the transformer's own stream and increasing sequence number.
  Multiple input streams do not imply a new global event-time order.
- Graph full replacements become delete-then-insert changes at the existing
  SourceChange conversion boundary. When the old record is supplied, its labels
  are used for the deletion. Updates do not retain an optional copy of the old
  record in the output.
- Identity-only deletions carry no labels. For middleware that selects by label,
  supply delete metadata or the old record, not just its identity.
- Built-in middleware passes future notifications through rather than rewriting
  them as ordinary records.
- Do not leave the same middleware configured on a downstream query unless you
  intend it to run a second time.

Successful stop/start retains the index and output counter. Failed or cancelled
processing can leave incomplete internal work, so that transformer instance
requires reconstruction rather than pretending it can safely continue.
Use a middleware's supported skip policy when bad records should not stop processing.

With `new`, the saved index is **not durable**. Rebuilding that volatile component
loses its state and counter; a persistent downstream query does not repair that.
With `new_durable`, the index, progress and output are restored from storage.
Durability covers the supplied index and emitted changes, not private mutable
state or external side effects inside custom middleware.

Queue limits count events, not expanded records or bytes. A middleware that
expands a very large array can still allocate a large result batch.

## Declarative construction and checks

`MiddlewareTransformerFactory` is in `FactoryRegistry::standard()`.
`definition.specification(registry_resource_id)` creates its specification.
Supply a `ResourceRole::Middleware` resource containing
`MiddlewareRegistryResource(Arc<MiddlewareTypeRegistry>)`. This is also compatible
with the existing `QueryMiddlewareResource` name.

Its configuration fields are `stream` (string), `middleware` (array) and
`pipeline` (array). The [factory test](../tests/computation_middleware_factory.rs)
shows the complete resource declaration and graph.

For durable mode, use
`definition.durable_specification(registry_id, indexes_id, options)`.
It adds `durability` configuration and an `indexes` dependency containing a
`QueryIndexProviderResource`. An optional `source_progress` dependency can contain
the middleware's `QuerySourceProgressResource`.

From the drasi-core repository root:

```bash
cargo test --locked -p drasi-lib --features computation,middleware-all \
  --test computation_middleware --test computation_middleware_factory

cargo test --locked -p drasi-lib --features computation-rocksdb-tests,middleware-all \
  --test computation_middleware_recovery

cargo test --locked -p lib-integration-tests --features computation-middleware-tests \
  --test computation_middleware_durability
```

`middleware-all` includes jq and uses the system jq library. It does not require
enabling `middleware-bundled-jq`; follow the middleware crate's prerequisite
instructions if jq is unavailable.
