# Native middleware transformer

The stateless middleware kinds (`decoder`, `map`, `parse_json`, `promote`,
`relabel`) run through the pipeline in
[`drasi-computation-middleware-common`](../../common/middleware), the same
implementation the [view transformer](../view) uses, built on the
`drasi-middleware` `StatelessMiddleware` implementations the query middleware
also uses. The plugin owns only envelope identity, the optional bootstrap
snapshot and lifecycle; it needs no query evaluator or query index.

`drasi-computation-transformer-middleware` provides only
`drasi.standard/middleware` (implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-transformer-middleware --features dynamic-plugin
```

Required configuration is `stream`, `middleware` and `pipeline`. Both `in` and
`out` use `GraphChangeCodec`. Each `middleware` entry is `{name, kind, config}`.

The supported kinds are declared: the factory's configuration schema
constrains `kind` to `decoder`, `map`, `parse_json`, `promote` and `relabel`,
so the graph rejects a definition naming any other kind when it binds the
component, and the list does not depend on how the library was built. `unwind`
reads stored element images and is available only in the
[query transformer](../query); `jq` needs the system libjq and is available
only in query transformer builds with `middleware-jq`. Middleware names must be
unique and every pipeline step must name a declaration. Construction is pure;
the surrounding transformer's sequencing/element bookkeeping is volatile. A
step that fails latches the component until it is reconstructed.

Optional `bootstrap: true` binds an existing quiescent `bootstrap` provider.
Initial records pass through the same pipeline as live records.
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
