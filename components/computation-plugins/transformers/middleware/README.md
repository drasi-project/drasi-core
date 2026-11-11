# Native middleware transformer

Map and relabel use the same processing implementations as query middleware,
without the `drasi-middleware/engine-adapter` feature. The plugin owns only
pipeline ordering, envelope identity and lifecycle; it needs no query evaluator
or query index.

`drasi-computation-transformer-middleware` provides only
`drasi.standard/middleware` (implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-transformer-middleware --features dynamic-plugin
```

Required configuration is `stream`, `middleware` and `pipeline`. Both `in` and
`out` use `GraphChangeCodec`. The factory directly reuses the existing `map` and
`relabel` middleware, not a legacy Source/Reaction adapter. Construction is pure;
the surrounding transformer's sequencing/element bookkeeping is volatile.

Optional `bootstrap: true` binds an existing quiescent `bootstrap` provider.
Initial records pass through the same map/relabel pipeline as live records.
The transformer keeps a canonical transformed output snapshot, hidden until
provider completion and awaited cleanup, at logical sequence zero. Later
inserts, replacements, patches and deletes update that same snapshot. A
downstream query can initialize from `out` without bypassing the middleware.
Restart rebuilds this opted-in volatile state under a fresh producer epoch.

Writers must remain quiescent during initialization. This mode does not accept
durable handover state or source watermarks. Without `bootstrap: true`, the
existing live-only processing path remains unchanged and snapshot requests fail
explicitly; it does not materialize input elements.

See the [pipeline example and contract](../../STANDARD.md#volatile-example-pipeline).
No counter, capture or arithmetic component is loaded.
