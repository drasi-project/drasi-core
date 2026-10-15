# Native gRPC sink

`drasi-computation-sink-grpc` provides only `drasi.network/grpc-sink`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-sink-grpc --features dynamic-plugin
```

Its `in` port consumes `QueryChangeCodec` envelopes. Configuration requires
`queryId`. Defaults include `stream="{queryId}/out"`,
`endpoint="grpc://localhost:50052"`, `timeoutMs=60000`, `maxRetries=5`,
`connectionRetryAttempts=10`, `initialConnectionTimeoutMs=15000`, `batchSize=1`,
`batchFlushTimeoutMs=100`, `outputFormat="canonicalJson"`,
`failurePolicy="strict"`. Default metadata includes `x-query-sequence: {queryId}`.

Only immediate single-item delivery is supported. Start establishes the actual
connection; exhausted failure is not lazy startup success. `Handled` means the
receiver reported success, or explicit `"skip"` logged and skipped failure,
not a transactional remote effect. See the
[configuration and completion contract](../../NETWORK.md#grpc-sink-configuration).
No gRPC source or HTTP/SSE sink component is loaded.
