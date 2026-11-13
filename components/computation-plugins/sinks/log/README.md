# Native log sink

`drasi-computation-sink-log` provides `drasi.standard/log` as a native
ComputationGraph sink.

The sink consumes standard graph-change envelopes and completes each input only
after formatting and emitting every change. Graph pipes therefore own retry and
durability; the sink has no legacy reaction checkpoint or migration behavior.

Configuration supports optional `outputs` aliases, per-output `routes`, and a
`defaultTemplate`. Templates use the standard native change context, including
`output`, `sequence`, `operation`, `identity`, `before`, `after`, and
`removedProperties`. Without a template, the complete change notification is
logged as JSON.

```sh
cargo build -p drasi-computation-sink-log --features dynamic-plugin
```
