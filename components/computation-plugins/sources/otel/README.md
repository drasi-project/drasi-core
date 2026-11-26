# Native OpenTelemetry source

`drasi-computation-source-otel` provides `drasi.network/otel-source`
(implementation `1`, configuration version `1`, native ABI 0.2). It receives
OpenTelemetry Protocol (OTLP) exports over gRPC and/or HTTP (binary protobuf)
and projects an allowlisted subset into a live service graph on its `out` port:

```text
(:Service)-[:REPORTS]->(:Metric)        current value of an allowlisted gauge or sum
(:Service)-[:DEPENDS_ON]->(:Service)    from client spans; expires after dependencyTtlSecs
(:Service)-[:EMITS]->(:LogEvent)        from error logs; expires after logEventTtlSecs
(:Service)-[:HEARTBEAT]->(:Heartbeat)   from a heartbeat metric or log event
```

It is a correlation input, not a telemetry backend: it keeps only the current
state of a bounded set of elements, and its ingress is volatile.

```sh
cargo build -p drasi-computation-source-otel --features dynamic-plugin
```

The OTLP protocol definitions (opentelemetry-proto v1.5.0) and `protoc` are
vendored; building needs no system protobuf compiler.

- **[DESIGN.md](DESIGN.md)**: architecture, projection rules and graph schema,
  state and expiry, ordering, lifecycle, management, security model, failure
  model and differences from the legacy `drasi-source-otel`.
- **[CONFIGURATION.md](CONFIGURATION.md)**: every field with defaults, ranges
  and update boundaries, the OTLP wire behaviour and status codes, examples and
  an operations runbook.

Minimal configuration:

```json
{"stream": "otel", "metricAllowlist": ["latency_p99_ms"]}
```

This serves OTLP/gRPC on `127.0.0.1:4317`. Add `"protocols": ["grpc", "http"]`
for OTLP/HTTP on `4318`. Listening on another address requires `authToken` or
an explicit `allowUnauthenticated: true`.
