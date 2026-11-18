# Native OpenTelemetry source: configuration

Configuration of `drasi.network/otel-source`, configuration version `1`. Keys
are camelCase; unknown keys are refused. [DESIGN.md](DESIGN.md) explains the
projection and lifecycle these settings control.

## Listener fields

| Field | Type | Default | Range | Secret | Update boundary | Description |
|---|---|---|---|---|---|---|
| `stream` | string | **required** | nonempty stream id | | Replace | Output stream identifier; must match the graph's binding for the `out` port. |
| `host` | string | `127.0.0.1` | literal IP | | Stop | Address both listeners bind. A non-loopback address requires `authToken` or `allowUnauthenticated`. |
| `protocols` | array | `["grpc"]` | one or both of `grpc`, `http`, no duplicates | | Stop | OTLP transports to serve. |
| `grpcPort` | integer | `4317` | 0–65535 | | Stop | OTLP/gRPC port, when `protocols` contains `grpc`. Zero picks an available port. |
| `httpPort` | integer | `4318` | 0–65535 | | Stop | OTLP/HTTP port, when `protocols` contains `http`. Must differ from `grpcPort` when both are served (unless zero). |
| `timeoutMs` | integer | `60000` | ≥ 1 | | Stop | Deadline for reading an HTTP body and for admitting an export into the queue. |
| `ingressCapacity` | integer | `1024` | 8–1048576 | | Stop | Concurrent exports being admitted, and the volatile queue bound. At least 8 so one record's group always fits. |
| `maxMessageBytes` | integer | `4194304` | 1–33554432 | | Stop | Largest export request, before and after gzip decompression. |
| `maxConnections` | integer | `1024` | 1–65536 | | Stop | Open connections per listener; further connections are closed. |
| `headerReadTimeoutMs` | integer | `10000` | 1–600000 | | Stop | Deadline to finish TLS and send request headers (HTTP) or the HTTP/2 preface (gRPC). |
| `authToken` | string | unset | 16–4096 visible ASCII | yes | Pause | Required bearer token (`Authorization: Bearer <token>`) on every export. |
| `authTokenSecondary` | string | unset | as above; differs from `authToken` | yes | Pause | Second accepted token, for rotation. Requires `authToken`. |
| `allowUnauthenticated` | boolean | `false` | | | Pause | Permit a non-loopback listener without a token, behind an authenticating proxy. |
| `rateLimitPerSecond` | integer | unset | 1–1000000 | | Pause | Per-client-address request rate. Unset disables rate limiting. |
| `rateLimitBurst` | integer | `rateLimitPerSecond` | 1–1000000 | | Pause | Per-client burst. Requires `rateLimitPerSecond`. |
| `tlsCertificatePem` | string | unset | PEM, ≤ 256 KiB | | Stop | Certificate chain for both listeners. Requires `tlsPrivateKeyPem`. |
| `tlsPrivateKeyPem` | string | unset | PEM, ≤ 256 KiB | yes | Stop | Private key for `tlsCertificatePem`. |
| `tlsClientCaPem` | string | unset | PEM, ≤ 256 KiB | | Stop | CA bundle; clients must present a certificate it issued (mutual TLS). Requires the certificate and key. |

## Projection fields

Lists hold at most 1024 entries; each entry, and each name, is 1–256 bytes.

| Field | Type | Default | Range | Update boundary | Description |
|---|---|---|---|---|---|
| `metricAllowlist` | array of strings | `[]` | `*` is the only wildcard | Pause | Metric names that become `Metric` nodes. Empty admits no metric; `["*"]` admits all. |
| `metricIdentityAttributes` | array of strings | `[]` | | Replace | Data-point attribute keys appended (sorted) to a Metric's identity, so each value combination is its own node. |
| `destinationAttributes` | array of strings | `["peer.service"]` | | Pause | Span attribute keys tried in order to name the callee; the first nonempty value wins. Empty disables `DEPENDS_ON`. |
| `spanKinds` | array | `["CLIENT"]` | `UNSPECIFIED`, `INTERNAL`, `SERVER`, `CLIENT`, `PRODUCER`, `CONSUMER`; no duplicates | Pause | Span kinds that may produce `DEPENDS_ON`. |
| `heartbeatMetric` | string | unset | | Pause | Metric name that refreshes the service's `Heartbeat`; need not be allowlisted. |
| `heartbeatEventName` | string | unset | | Pause | Log `event_name` that refreshes the `Heartbeat`, regardless of severity or log allowlist. |
| `logMinSeverity` | string | `ERROR` | `TRACE`, `DEBUG`, `INFO`, `WARN`, `ERROR`, `FATAL` | Pause | Minimum severity of a `LogEvent` (compared with the OTLP `severity_number`). |
| `logEventNameAllowlist` | array of strings | `[]` | `*` wildcard | Pause | When nonempty, only matching `event_name`s become `LogEvent`s. |
| `logEventTtlSecs` | integer | `60` | 1–31536000 | Pause | `LogEvent` and `EMITS` lifetime from receipt. |
| `dependencyTtlSecs` | integer | `300` | 1–31536000 | Pause | `DEPENDS_ON` lifetime from the last refreshing span's receipt. |
| `maxServices` | integer | `1000` | 1–10000000 | Pause | Live `Service` nodes. |
| `maxMetrics` | integer | `2000` | 1–10000000 | Pause | Live `Metric` nodes. |
| `maxDependencies` | integer | `5000` | 1–10000000 | Pause | Live `DEPENDS_ON` relations. |
| `maxLogEvents` | integer | `5000` | 1–10000000 | Pause | Live `LogEvent` nodes. |
| `rejectDerived` | boolean | `true` | | Pause | Reject records carrying `drasi.source.origin=derived` on the resource or the record. |

## Examples

Local collector over gRPC:

```json
{
  "stream": "otel",
  "metricAllowlist": ["latency_p99_ms", "*_error_rate"],
  "heartbeatMetric": "health.heartbeat"
}
```

Both transports on all interfaces, with TLS and token rotation (secret values
are host secret references):

```json
{
  "stream": "otel",
  "host": "0.0.0.0",
  "protocols": ["grpc", "http"],
  "authToken": {"kind": "secret", "name": "otel-ingest-token"},
  "authTokenSecondary": {"kind": "secret", "name": "otel-ingest-token-next"},
  "rateLimitPerSecond": 200,
  "tlsCertificatePem": "-----BEGIN CERTIFICATE-----\n...",
  "tlsPrivateKeyPem": {"kind": "secret", "name": "otel-tls-key"},
  "metricAllowlist": ["latency_p99_ms"],
  "metricIdentityAttributes": ["region"],
  "destinationAttributes": ["peer.service", "server.address"],
  "logMinSeverity": "WARN",
  "logEventNameAllowlist": ["payment_*"],
  "dependencyTtlSecs": 300,
  "logEventTtlSecs": 60
}
```

OpenTelemetry Collector exporter for the first example:

```yaml
exporters:
  otlp/drasi:
    endpoint: 127.0.0.1:4317
    tls:
      insecure: true
  otlphttp/drasi:            # with "protocols": ["http"]
    endpoint: http://127.0.0.1:4318
    encoding: proto
```

With `authToken`, add `headers: {authorization: "Bearer <token>"}` to the
exporter.

## Wire behaviour

**OTLP/gRPC.** `Export` on `opentelemetry.proto.collector.metrics.v1.MetricsService`,
`...trace.v1.TraceService` and `...logs.v1.LogsService`. gzip request
compression is accepted. Every RPC passes the rate limit and token check.

**OTLP/HTTP.** `POST /v1/metrics`, `/v1/traces`, `/v1/logs` with
`Content-Type: application/x-protobuf` and optional `Content-Encoding: gzip`.
Success is 200 with the protobuf `Export*ServiceResponse`
(`Content-Type: application/x-protobuf`). Errors carry a protobuf
`google.rpc.Status` body. Other paths answer 404; other methods 405.

**Partial success.** Records the source does not project (not allowlisted,
wrong kind or severity, no `service.name`, derived, unsupported metric type,
impossible timestamp) or drops at a cap are counted in `partial_success`
(`rejected_data_points`, `rejected_spans`, `rejected_log_records`). Exporters
must not retry them.

**Status codes.**

| Condition | gRPC | HTTP | Retry |
|---|---|---|---|
| Missing or wrong token | `UNAUTHENTICATED` | 401 | no |
| Rate limited | `RESOURCE_EXHAUSTED` | 429 + `Retry-After: 1` | yes |
| Paused, stopping, overloaded, queue full at deadline | `UNAVAILABLE` | 503 + `Retry-After: 1` | yes |
| Request larger than `maxMessageBytes` | `OUT_OF_RANGE` | 413 | no |
| Body not read before `timeoutMs` | — | 408 | yes |
| Wrong `Content-Type` or `Content-Encoding` | — | 415 | no |
| Malformed protobuf or gzip | decode error | 400 | no |

A retried export after a refusal is safe: elements admitted before the refusal
are updated again, not duplicated.

## External prerequisites

Any OTLP exporter speaking OTLP 1.x: the OpenTelemetry Collector (`otlp` or
`otlphttp` exporter with `encoding: proto`) or an SDK. Resources must carry
`service.name`. Spans need the configured destination attribute (most SDKs
set `peer.service` or `server.address` on client spans).

## Update rules

See the update boundary columns. Pause-boundary changes apply to the next
export without rebinding. Stop-boundary changes rebind the listeners at the
next Start. Changing `stream` or `metricIdentityAttributes` requires
reconstruction, which starts with empty projection state under a new volatile
producer identity.

## Removal and erase

The source owns no persistent state. Removal with erase succeeds and is
idempotent; nothing outside the process is touched.

## Operations runbook

| Symptom | Check |
|---|---|
| Exporter: connection refused | The source is started; `protocols` includes the transport; the port matches; `host` is reachable (loopback by default). |
| Exporter: `UNAUTHENTICATED` / 401 | The exporter sends `authorization: Bearer <token>` matching `authToken` or `authTokenSecondary`. |
| Exporter: 415 | Use `encoding: proto` (OTLP/JSON is not supported) and gzip or no compression. |
| Exporter: repeated `UNAVAILABLE` / 503 | The source is paused, or the graph is not consuming and the queue is full; check downstream progress and `ingressCapacity`. |
| Metric never appears | Its name matches `metricAllowlist` (empty admits none); the metric is a gauge or sum; the resource has `service.name`; check `partial_success`. |
| No `DEPENDS_ON` | The span kind is in `spanKinds`; a `destinationAttributes` key is present and nonempty. |
| `DEPENDS_ON` disappears | No refreshing span arrived within `dependencyTtlSecs` of receipt. |
| New services or metrics stop appearing | A cap was reached; the Stop log reports `dropped`. Raise the cap or narrow the allowlist. |
| Duplicate-looking Metric nodes | Points differ in a `metricIdentityAttributes` attribute; that is intended. |
| Graph keeps elements after a source restart | A reconstructed source starts with empty state and a new producer identity; downstream handling of volatile producer epochs applies. |
