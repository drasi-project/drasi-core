# Native middleware transformers

The middleware kinds (`decoder`, `map`, `parse_json`, `promote`, `relabel`)
run through the pipeline in
[`drasi-computation-middleware-common`](../../common/middleware), the same
implementation the [view transformer](../view) uses, built on the
`drasi-middleware` `StatelessMiddleware` implementations the query middleware
also uses. Neither factory needs a query evaluator or query index.

`drasi-computation-transformer-middleware` provides two factories
(implementation `"1"`, configuration `1`):

| Factory | State | Current state of `out` |
| --- | --- | --- |
| `drasi.standard/middleware` | None | Its input's current state, transformed on read |
| `drasi.standard/stateful-middleware` | The transformed current state | Its own materialized output |

```sh
cargo build -p drasi-computation-transformer-middleware --features dynamic-plugin
```

Both require `stream`, `middleware` and `pipeline`. Both `in` and `out` use
`GraphChangeCodec`. Each `middleware` entry is `{name, kind, config}`.

The supported kinds are declared: the factories' configuration schemas
constrain `kind` to `decoder`, `map`, `parse_json`, `promote` and `relabel`,
so the graph rejects a definition naming any other kind when it binds the
component, and the list does not depend on how the library was built. `unwind`
reads stored element images and is available only in the
[query transformer](../query); `jq` needs the system libjq and is available
only in query transformer builds with `middleware-jq`. Middleware names must be
unique and every pipeline step must name a declaration. Construction is pure.
A step that fails latches the component until it is reconstructed.

## Stateless middleware

`drasi.standard/middleware` stores no rows. A `current_state` read of `out`
applies its read-only pipeline to the current state of the single producer on
`in`; it does not call `transform` or publish to live subscribers. A
pass-through or `relabel` pipeline also supports a connected consumer's
bootstrap when the upstream provides a logical cut: the mapper emits one
envelope per input and stamps both the cut and live output in an epoch
specific to that mapper and upstream producer. An upstream datastore read
without a position remains one-shot. Event-only `map` rules cannot in general
reconstruct an output view from today's input, so this factory offers no
current state for a pipeline with a `map` step; use the stateful middleware.

## Stateful middleware

`drasi.standard/stateful-middleware` is a stateful transformer: it runs on the
same runtime as the [query transformer](../query), described in the
[native transformer guide](../NATIVE-TRANSFORMER-GUIDE.md#stateful-transformers).
Its output is the transformed current state of its input, maintained from the
pipeline's output changes: inserts and replacements set a row, patches merge
into it, and deletes remove it. Because the state is its own, every pipeline
works, including `map`.

It accepts the runtime's shared options in addition to the pipeline:

| Option | Effect |
| --- | --- |
| `bootstrap` | Initialize from the bound bootstrap provider or the connected input's current state |
| `volatile` | Keep state in memory; every initialization publishes a new output epoch |
| `publication` | `atomic` (default) or `non_atomic` |
| `recovery` | `strict` (default) or `auto_reset` |
| `resetConfiguration` | Reset retained state when the configuration changed |
| `outboxCapacity` | Committed outputs retained for replay and restart delivery (default 1) |

With a persistent processing provider the middleware keeps its rows, output
history and input progress across restarts. On restart it replays unconfirmed
output and continues from its committed input position. After a gap in its
input's retained history, `auto_reset` with a connected input rebuilds the
state from that input's current state and publishes only the keyed difference,
so a running dependent follows the correction without a reset. A dependent
that installed the empty view before the first input connected receives the
first input's rows the same way. A rebuild that would strand a running
dependent is refused.

See the [pipeline example and contract](../../STANDARD.md#volatile-example-pipeline).
No counter, capture or arithmetic component is loaded.
