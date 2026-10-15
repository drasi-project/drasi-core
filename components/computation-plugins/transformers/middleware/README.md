# Native middleware transformer

`drasi-computation-transformer-middleware` provides only
`drasi.standard/middleware` (implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-transformer-middleware --features dynamic-plugin
```

Required configuration is `stream`, `middleware` and `pipeline`. Both `in` and
`out` use `GraphChangeCodec`. The factory directly reuses the existing `map` and
`relabel` middleware, not a legacy Source/Reaction adapter. Construction is pure;
the surrounding transformer's sequencing/element bookkeeping is volatile.

See the [pipeline example and contract](../../STANDARD.md#volatile-example-pipeline).
No counter, capture or arithmetic component is loaded.
