# Native profiler sink

`drasi-computation-sink-profiler` provides `drasi.standard/profiler` as a
native ComputationGraph sink.

It reads profiling metadata from native query envelopes, keeps a bounded sample
window, and periodically reports latency statistics. Inputs complete with
`Handled`; graph pipes own retry and persistence, and the sink has no legacy
reaction checkpoint or migration behavior.

Configuration fields are `windowSize` (default `1000`) and
`reportIntervalSecs` (default `60`).

```sh
cargo build -p drasi-computation-sink-profiler --features dynamic-plugin
```
