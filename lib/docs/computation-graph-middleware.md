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

The transformer therefore keeps previous emitted elements in an owned in-memory
index. Patch updates merge missing properties when updating this saved view, as
the query does after middleware processing. Middleware-specific skip/fail
settings and patch semantics remain those of the existing implementations.
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

## Event behaviour and limits

- One input event produces one output event containing all expanded changes.
  Expansion does not split one source sequence across several output events.
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

The saved index is **not durable**. Reconstructing the component requires rebuilding
the state needed by middleware such as Unwind. Use a fresh output stream when
reconstruction starts its sequence from zero. A persistent downstream query
does not make this transformer's private state persistent.

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

From the drasi-core repository root:

```bash
cargo test --locked -p drasi-lib --features computation,middleware-all \
  --test computation_middleware --test computation_middleware_factory
```

`middleware-all` includes jq and uses the system jq library. It does not require
enabling `middleware-bundled-jq`; follow the middleware crate's prerequisite
instructions if jq is unavailable.
