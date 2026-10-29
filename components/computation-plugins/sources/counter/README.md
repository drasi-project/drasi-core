# Native volatile counter source

`drasi-computation-source-counter` provides only `drasi.standard/volatile-counter`
(implementation `"1"`, configuration `1`). Build its standalone plugin from the
workspace:

```sh
cargo build -p drasi-computation-source-counter --features dynamic-plugin
```

The `out` port emits `GraphChangeCodec` envelopes. Configuration requires `stream`;
defaults are `count=10`, `start=0`, `step=1`, `interval_ms=0`, `paused=false`.
Counts must be nonnegative and the configured integer range must not overflow.

This is a finite **volatile example**, not a persistent or replayable source.
Reconstruction resets its position. Dedicated `drasi.counter.pause` and
`drasi.counter.resume` custom controls take null payloads and remain serviceable
while data is pending. See the [counter contract](../../STANDARD.md#volatile-example-pipeline)
for sequencing and control boundaries.
