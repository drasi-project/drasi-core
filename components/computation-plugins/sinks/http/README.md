# Native HTTP sink

`HttpSinkConfig` and its defaults, URL/header checks and parsing live in this
crate's `config` module. Shared network code supplies only common validation
and delivery-policy helpers.

`drasi-computation-sink-http` provides only `drasi.network/http-sink`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-sink-http --features dynamic-plugin
```

Its `in` port consumes `QueryChangeCodec` envelopes. Configuration requires
`queryId` and a complete `url`. `stream` defaults to `{queryId}/out`,
`timeoutMs=60000`, `maxRetries=3`, `failurePolicy="strict"`. Default headers are
`content-type: application/json` and `x-query-sequence: {queryId}`; conflicting
values reject. Redirects are disabled.

`Handled` means the notification attempts completed successfully, or an explicit
`"skip"` policy logged and skipped exhausted failures. An ordinary HTTP success,
including 202, does not establish remote business completion.
See [configuration and delivery](../../NETWORK.md#http-sink-configuration).

The opt-in Rust `delivery` module is also packaged here; it supplies
`HttpDeliveryHandler` and `HttpDeliveryEndpoint` for the existing `DeliveryRunner`.
It is never enabled implicitly by the ordinary sink factory. See its
[completion contract](../../NETWORK.md#opt-in-http-completion-service).
No HTTP source or gRPC/SSE sink component is loaded.
