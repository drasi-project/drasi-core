# Native profiler sink

`drasi-computation-sink-profiler` provides `drasi.standard/profiler` as a
native ComputationGraph sink.

It reads profiling metadata from native query envelopes, keeps a bounded sample
window, and periodically reports latency statistics. Inputs complete with
`Handled`; the sink has no legacy reaction checkpoint or migration behavior.

Reports are `info` records with the `log` target `drasi.profiler`. Loaded as a
plugin library, the SDK's logging bridge forwards them to the host's logger, so
they appear wherever the host's other logs do (at the level the host enabled
when it loaded the library).

Configuration fields are `windowSize` (default `1000`), `reportIntervalSecs`
(default `60`) and `onPermanentFailure` (default `stall`). An input without
query output metadata can never be profiled and fails permanently;
`onPermanentFailure: skip` counts and skips it instead of stalling the sink.
See `components/computation-plugins/sinks/NATIVE-SINK-GUIDE.md` for the shared
delivery-failure model.

```sh
cargo build -p drasi-computation-sink-profiler --features dynamic-plugin
```
