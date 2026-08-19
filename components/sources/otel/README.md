# drasi-source-otel

Receives OpenTelemetry Protocol (OTLP) data and projects an allowlisted subset into Drasi's live graph. This is a correlation input, not a telemetry backend.

## Prerequisites

- Rust 1.95+
- An OTLP exporter (OpenTelemetry Collector, SDK, or the client harness in tests)
- For the getting-started example: Docker, so telemetry can go through a Collector container
- `protoc` is **not** required; the crate vendors proto files and `protoc-bin-vendored`

## Configuration

| Field | Default | Description |
| --- | --- | --- |
| `grpcBind` | `0.0.0.0:4317` | OTLP/gRPC listen address. Empty disables gRPC. |
| `httpBind` | unset | Optional OTLP/HTTP protobuf listen address (`/v1/traces`, `/v1/metrics`, `/v1/logs`) |
| `tlsCertPath` / `tlsKeyPath` | unset | TLS. Unset is the documented local-demo plaintext exception. |
| `authToken` | unset | Static bearer token. An identity provider Token/Basic credential wins if set. |
| `metricAllowlist` | `[]` | Accepted metric names. Empty rejects all. `*` allows all. Only `*` globs (`latency_*`, `*_p99`); `?` and `**` are not supported. |
| `destinationAttributes` | `["peer.service"]` | Client-span attributes used as the callee service |
| `heartbeatMetric` | unset | Metric name that refreshes `Heartbeat.lastSeen` |
| `dependencyTtlSecs` | `300` | `DEPENDS_ON` expiry unless refreshed. TTL is measured from **receipt time**, not OTLP event time. |
| `logEventTtlSecs` | `60` | `LogEvent` expiry from receipt time |
| `rejectDerived` | `true` | Drop `drasi.source.origin=derived` |
| `maxRequestBytes` | `4194304` (4 MiB) | Maximum decoded OTLP request size for gRPC and HTTP |
| `durability` | off | Optional WAL replay of **projected** changes |

OTLP timestamps are nanoseconds and are converted to millisecond `effective_from` values. TTL expiry uses the wall-clock time the export was received so late Collector batches are not deleted immediately.

## Data mapping

See [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md). Histogram/summary metrics are rejected; send a pre-aggregated gauge (for example p99) instead.

## Integration test

```bash
cargo test -p drasi-source-otel -- --ignored --nocapture
# or
make -C components/sources/otel integration-test
```

The test starts an OpenTelemetry Collector with testcontainers and sends metrics, traces, and logs through it. It asserts metric CREATE/UPDATE, DEPENDS_ON CREATE/DELETE via TTL, and LogEvent CREATE.

## Operations

This is an **ingress** source. There is no upstream poller, so there is no reconnect or backoff loop. The Collector or SDK retries failed OTLP exports.

| Status | When |
| --- | --- |
| `Starting` | `start()` begins (WAL register, lifecycle load) |
| `Running` | OTLP sockets bound successfully |
| `Error` | Bind failed, or the accept loop died |
| `Stopped` | `stop()` finished; listeners closed |

`stop()` signals the gRPC/HTTP servers, aborts WAL prune, persists lifecycle state, and clears channel dispatchers via `SourceBase::stop_common()`.

## Limitations

- Push-only. There is no bootstrap dump of current graph state.
- `supports_replay()` is false unless WAL durability is enabled.
- OTLP/JSON is not supported.
- Profiles are out of scope.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Connection refused | Source started? `grpcBind` port free? For the example, is the Collector up on 4317? |
| Collector drops with gzip unsupported | Source gRPC accepts gzip; restart after upgrading this crate |
| Bootstrap works but no metric updates | Metric name on `metricAllowlist`? Resource has `service.name`? |
| No DEPENDS_ON | Span kind CLIENT? `peer.service` (or configured attribute) present? |
| Edges never disappear | `dependencyTtlSecs` and sweeper; use a short TTL in tests |
| `effective_from` rejected | Source must convert nanos to millis (already done in mapping) |
