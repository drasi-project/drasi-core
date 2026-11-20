# Native gRPC source

`drasi-computation-source-grpc` provides `drasi.network/grpc-source`
(implementation `1`, configuration version `1`, native ABI 0.1). It serves the
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
