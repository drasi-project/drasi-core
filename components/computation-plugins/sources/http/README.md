# Native HTTP source

`drasi-computation-source-http` provides only `drasi.network/http-source`
(implementation `"1"`, configuration `1`).

```sh
cargo build -p drasi-computation-source-http --features dynamic-plugin
```

Configuration requires `stream`. Defaults are `sourceId="facilities-db"`,
`host="0.0.0.0"`, `port=9000`, `timeoutMs=60000`, `ingressCapacity=1024`,
`maxMessageBytes=2097152`, `maxBatchEvents=1024`, `adaptiveEnabled=false`.
Its `out` port emits `GraphChangeCodec` envelopes; construction binds no socket.

The crate includes ordinary and durable HTTP routes. Ordinary event acceptance
is volatile; durable acceptance requires an actual host admission channel and
the separate session/sequence protocol. See
[source configuration](../../NETWORK.md#source-configuration) and
[durable admission](../../NETWORK.md#optional-durable-admission) for wire shapes,
limits and acknowledgement semantics. No gRPC component is loaded.
