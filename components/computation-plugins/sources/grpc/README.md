# Native gRPC source

`drasi-computation-source-grpc` provides `drasi.network/grpc-source`
(implementation `1`, configuration version `1`, native ABI 4.0). It serves the
published `drasi.v1.SourceService` and the durable
`drasi.admission.v1.AdmissionService` ([`proto/admission.proto`](proto/admission.proto)),
and emits each change on its `out` port as a `GraphChangeCodec` envelope.

```sh
cargo build -p drasi-computation-source-grpc --features dynamic-plugin   # requires protoc
```

The build script compiles `admission.proto` against the shared source messages
in `drasi-grpc-common`; there is no local copy of `common.proto`.

- **[DESIGN.md](DESIGN.md)**: services, admission modes, streaming semantics,
  lifecycle, management, security model and differences from the legacy source.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges
  and update boundaries, services and status codes, durable admission, exposure
  and operations.

Minimal configuration: `{"stream": "telemetry"}` listens on `127.0.0.1:50051`.
Listening on another address requires `authToken` or an explicit
`allowUnauthenticated: true`.

## Incoming trace context

Unary event calls, streaming events and durable admission extract W3C
`traceparent` and optional `tracestate` from request metadata through the
shared SDK. Request metadata applies to every event in that stream. The
incoming trace ID, sender parent span and sampling decision are retained;
framework spans receive new local span IDs. Missing metadata clears prior
context; malformed metadata logs a warning and is ignored without rejecting
business data. Trace context is transient and does not change protobuf event
messages, persisted data or receipt identities.
