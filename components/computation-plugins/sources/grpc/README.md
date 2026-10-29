# Native gRPC source

`drasi-computation-source-grpc` provides only `drasi.network/grpc-source`
(implementation `"1"`, configuration `1`).

`GrpcSourceConfig` and its validation live in this crate's `config` module.
Use `GrpcSourceConfig::from_value` for JSON settings; `SourceConfig` remains an
import-compatible name.

```sh
cargo build -p drasi-computation-source-grpc --features dynamic-plugin
```

Configuration requires `stream`. Defaults are `sourceId="facilities-db"`,
`host="0.0.0.0"`, `port=50051`, `timeoutMs=60000`, `ingressCapacity=1024`,
`maxMessageBytes=4194304`, `adaptiveEnabled=false`. `maxBatchEvents` is not accepted.
Its `out` port emits `GraphChangeCodec` envelopes; construction binds no socket.

This crate owns the durable [admission protocol](proto/admission.proto).
Its build script imports source messages from `drasi-grpc-common`, without a
duplicate `common.proto` or dependency on an ordinary source implementation.
Building this component requires `protoc`. Ordinary acceptance is
volatile; durable acceptance requires an actual host admission channel and the
separate session/sequence protocol. See
[source configuration](../../NETWORK.md#source-configuration) and
[durable admission](../../NETWORK.md#optional-durable-admission).
No HTTP source or gRPC sink component is loaded.

`SourceService.StreamEvents` reports **cumulative accepted counts**, matching
the published source protocol and both test-run-host dispatchers. The native
listener emits a progress response for every accepted input; clients retain
the latest total rather than summing responses. Invalid input returns a failed
response with the accepted prefix count and closes that stream. Transport,
timeout and admission errors remain errors even if earlier responses succeeded.
The separate durable admission service returns receipts, not these counters.
