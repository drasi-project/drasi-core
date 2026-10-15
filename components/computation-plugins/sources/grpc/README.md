# Native gRPC source

`drasi-computation-source-grpc` provides only `drasi.network/grpc-source`
(implementation `"1"`, configuration `1`).

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
