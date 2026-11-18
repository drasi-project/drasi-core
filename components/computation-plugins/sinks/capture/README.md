# Native file capture sink

`drasi-computation-sink-capture` provides only `drasi.standard/capture`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-sink-capture --features dynamic-plugin
```

Its `in` port consumes `GraphChangeCodec` envelopes. Configuration requires
`path`; `append` defaults to `false`; `onPermanentFailure` (`stall` or `skip`,
default `stall`) is the shared sink setting of the
[delivery-failure model](../NATIVE-SINK-GUIDE.md#delivery-failures).
Construction performs no file I/O. Start opens the file, truncating it unless
append was selected; with append, a trailing partial line left by an earlier
process is removed first.

One complete `EnvelopeCodec` JSON frame is written per line. `Handled` means the
write and flush completed, **not** fsync durability or exactly-once effects.
The sink tracks the length of its complete frames: a write that fails (for
example on a full disk) or is cancelled is cut back to the last complete frame
before anything else is written, so the graph's in-place retry of the same
input never follows a partial frame. Write failures are transient. Replay can
append duplicates.
See the [capture contract](../../STANDARD.md#volatile-example-pipeline).
No source or transformer component is loaded.
