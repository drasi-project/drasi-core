# Native file capture sink

`drasi-computation-sink-capture` provides only `drasi.standard/capture`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-sink-capture --features dynamic-plugin
```

Its `in` port consumes `GraphChangeCodec` envelopes. Configuration requires
`path`; `append` defaults to `false`. Construction performs no file I/O.
Start opens the file, truncating it unless append was selected.

One complete `EnvelopeCodec` JSON frame is written per line. `Handled` means the
write and flush completed, **not** fsync durability or exactly-once effects.
Failure/cancellation can leave a partial line, and replay can append duplicates.
See the [capture contract](../../STANDARD.md#volatile-example-pipeline).
No source or transformer component is loaded.
