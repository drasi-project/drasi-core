# Native HTTP source

`drasi-computation-source-http` provides `drasi.network/http-source`
(implementation `1`, configuration version `1`, native ABI 5.0). It accepts
graph changes over HTTP, through standard event routes or configured webhook
routes, and emits them on its `out` port as `GraphChangeCodec` envelopes.

```sh
cargo build -p drasi-computation-source-http --features dynamic-plugin
```

- **[DESIGN.md](DESIGN.md)**: architecture, admission modes (volatile,
  server-sequenced durable, session protocol), lifecycle, management, security
  model, failure model and differences from the legacy source.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges
  and update boundaries, request formats, webhooks, durable admission,
  exposure rules and operations.

Minimal configuration: `{"stream": "orders"}` listens on `127.0.0.1:9000`.
Listening on another address requires `authToken`, webhook route
authentication, or an explicit `allowUnauthenticated: true`.
