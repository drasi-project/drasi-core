# drasi-source-otel

Receives OpenTelemetry Protocol (OTLP) data and projects an allowlisted subset into Drasi's live graph. This is a correlation input, not a telemetry backend.

Every record needs Resource `service.name`. That becomes a `Service` node. Metrics, spans, and logs hang off that service.

## Prerequisites

- Rust 1.95+ (workspace `rust-toolchain.toml`)
- An OTLP exporter (OpenTelemetry Collector, SDK, or the test harness)
- For the getting-started example: Docker, so telemetry can go through a Collector container
- `protoc` is **not** required; the crate vendors proto files and `protoc-bin-vendored`

## Quick start

```yaml
kind: otel
name: otel
properties:
  grpcBind: "0.0.0.0:4317"
  metricAllowlist:
    - latency_p99_ms
```

Point an OTLP exporter at `http://<host>:4317`. Send a gauge named `latency_p99_ms` with Resource `service.name=checkout`. The graph gets:

```text
(:Service {name: "checkout"})-[:REPORTS]->(:Metric {name: "latency_p99_ms", value: 920})
```

The next gauge with the same service and metric name updates that Metric node's `value`. It does not create a second Metric.

## Configuration reference

YAML keys are camelCase. Builder methods use snake_case (`with_grpc_bind`, `with_metric_allowlist`, ...).

Empty `metricAllowlist` rejects every metric. TTL is measured from **receipt time** (when the export arrives), not the OTLP timestamp. Unset TLS is the local-demo plaintext exception.

### Listeners

#### `grpcBind`

OTLP/gRPC listen address (`host:port`). Default: `0.0.0.0:4317` (standard Collector port). Set to `""` to disable gRPC. At least one of `grpcBind` or `httpBind` must be set.

```yaml
grpcBind: "0.0.0.0:4317"
```

Use `127.0.0.1:4317` to stay on loopback. Use `0.0.0.0` if a Collector in Docker must reach the process.

#### `httpBind`

Optional OTLP/HTTP protobuf listener. Unset disables HTTP. Paths: `POST /v1/traces`, `/v1/metrics`, `/v1/logs` with `Content-Type: application/x-protobuf`. OTLP/JSON is not supported.

```yaml
httpBind: "0.0.0.0:4318"
```

#### `tlsCertPath` / `tlsKeyPath`

PEM server certificate and private key. Both must be set together, or both unset. Unset means plaintext (local demo only).

```yaml
tlsCertPath: /certs/server.crt
tlsKeyPath: /certs/server.key
```

#### `tlsClientCaPath`

Optional PEM CA used to verify client certificates (mTLS). Requires the server cert/key pair.

```yaml
tlsClientCaPath: /certs/clients.pem
```

#### `authToken`

Static inbound bearer token. Clients must send `Authorization: Bearer <token>`. If an identity provider is attached, its Token or Basic credential wins over this field.

```yaml
authToken: "s3cret"
# or
authToken:
  kind: secret
  name: otel-ingest-token
```

With no token and no identity provider, the listener is open. That is the local-demo exception, not a production default.

#### `maxRequestBytes`

Maximum decoded OTLP request size for gRPC and HTTP. Default: `4194304` (4 MiB). HTTP bodies larger than this get `413`. gRPC uses `max_decoding_message_size`.

```yaml
maxRequestBytes: 8388608
```

### Metrics

Only **gauge** and **sum** data points are projected. Histogram, summary, and exponential histogram are rejected; send a pre-aggregated gauge (for example p99) instead.

#### `metricAllowlist`

Metric names that become `Metric` nodes. **Empty list rejects every metric.** `*` allows all. Only `*` wildcards work (`latency_*`, `*_p99`, `*lat*`). `?` and `**` are not supported.

```yaml
metricAllowlist:
  - latency_p99_ms
  - "*_p99"
```

Example: Resource `service.name=checkout` and gauge `latency_p99_ms=920` become one Service, one Metric, and a `REPORTS` relationship:

```text
(:Service {name: "checkout"})-[:REPORTS]->(:Metric {name: "latency_p99_ms", value: 920})
```

A later point `latency_p99_ms=700` from the same service **updates** that Metric. A different name (`error_rate`) is a second Metric with its own `REPORTS` edge.

#### `metricIdentityAttributes`

By default, one Metric node per (service, metric name). If the same name is reported with different dimensions (for example `region=us` vs `region=eu`), those collapse into one node unless you list the dimension here.

Listed keys are copied into the Metric's identity so each combination is its own node:

```yaml
metricIdentityAttributes:
  - region
```

Then `latency_p99_ms` with `region=us` and `region=eu` are two Metric nodes, both `REPORTS` from checkout. Attributes you do not list are ignored and do not split series.

```yaml
metricIdentityAttributes:
  - region
  - deployment.environment
```

### Traces (`DEPENDS_ON`)

A matching span upserts:

```text
(:Service {name: "<caller>"})-[:DEPENDS_ON]->(:Service {name: "<callee>"})
```

The caller is Resource `service.name`. The callee is not a standard span field; see `destinationAttributes`. If the span does not match `spanKinds` or has no destination attribute, it is dropped.

#### `spanKinds`

Which span kinds are considered. Default: `["CLIENT"]` (outbound calls). Values: `CLIENT`, `SERVER`, `PRODUCER`, `CONSUMER`, `INTERNAL`.

```yaml
spanKinds:
  - CLIENT
```

CLIENT is the usual choice for "who does this service call?". SERVER would treat inbound spans as dependencies and is rarely what you want.

#### `destinationAttributes`

Ordered list of **span attribute keys** used to name the callee. The first non-empty value wins. Default: `["peer.service"]`.

OpenTelemetry does not have one required field for "remote service". SDKs put it on `peer.service`, `server.address`, `net.peer.name`, and others. This list is how you tell Drasi which key to read.

```yaml
destinationAttributes:
  - peer.service
  - server.address
```

Example: checkout emits a CLIENT span with `peer.service=payments`. Graph:

```text
(:Service {name: "checkout"})-[:DEPENDS_ON]->(:Service {name: "payments"})
```

If the caller has `service.namespace=shop` and the dest value is a bare name (`payments`), the destination Service is `{namespace: "shop", name: "payments"}`. A value that already contains `/` (`shop/payments`) is split into namespace and name as-is.

If none of the keys are present, that span produces no edge.

#### `dependencyTtlSecs`

How long a `DEPENDS_ON` edge lives without a refreshing span. Default: `300`. Must be > 0. Clock starts when the export is **received**, not from `span.start_time`.

```yaml
dependencyTtlSecs: 300
```

A late Collector batch with an old span timestamp does not expire immediately. If no matching CLIENT span arrives within this window, the sweeper deletes the edge.

### Heartbeats

Optional liveness node `(:Service)-[:HEARTBEAT]->(:Heartbeat {lastSeen})`. Unset means no heartbeat graph.

#### `heartbeatMetric`

Metric name that refreshes `Heartbeat.lastSeen`. Does not have to be on `metricAllowlist`. If it is also allowlisted, it still counts as one accepted record and can create both Heartbeat and Metric.

```yaml
heartbeatMetric: health.heartbeat
```

#### `heartbeatEventName`

Log `event_name` that refreshes the same Heartbeat. Useful when liveness is a log event rather than a metric.

```yaml
heartbeatEventName: health.heartbeat
```

### Logs (`LogEvent`)

An admitted log becomes `(:Service)-[:EMITS]->(:LogEvent)` and is deleted after `logEventTtlSecs`.

#### `logMinSeverity`

Minimum severity for LogEvent admission. Default: `ERROR`. Values: `TRACE`, `DEBUG`, `INFO`, `WARN` / `WARNING`, `ERROR`, `FATAL`. Unknown names are treated as `ERROR`.

```yaml
logMinSeverity: ERROR
```

INFO logs are dropped unless you lower this.

#### `logEventNameAllowlist`

If non-empty, only logs whose `event_name` matches become LogEvent nodes. Same `*` glob rules as `metricAllowlist`. Empty list means any event name is allowed (severity still applies).

```yaml
logEventNameAllowlist:
  - payment_failed
  - "auth_*"
```

A log with no `event_name` uses a stable hash of the body in the node id.

#### `logEventTtlSecs`

How long LogEvent nodes (and their `EMITS` edges) live. Default: `60`. Must be > 0. Receipt time, not log timestamp.

```yaml
logEventTtlSecs: 60
```

### Bounds and safety

#### `maxServices` / `maxMetrics` / `maxDependencies` / `maxLogEvents`

Caps on distinct live ids. Defaults: 1000 / 2000 / 5000 / 5000. A new group that would exceed a cap is dropped as a whole (service + metric together, not a dangling REPORTS edge). Existing ids can still update.

```yaml
maxServices: 1000
maxMetrics: 2000
maxDependencies: 5000
maxLogEvents: 5000
```

#### `rejectDerived`

Drop records whose Resource or attributes include `drasi.source.origin=derived`. Default: `true`. Stops a reaction that exports OTLP from feeding back into this source.

```yaml
rejectDerived: true
```

#### `durability`

Optional WAL of **projected** SourceChanges (not raw OTLP). Default: off. When enabled, `supports_replay()` is true and subscribers can `resume_from` a WAL position.

```yaml
durability:
  enabled: true
```

## Full example

```yaml
kind: otel
name: otel
properties:
  grpcBind: "0.0.0.0:4317"
  httpBind: "0.0.0.0:4318"
  metricAllowlist:
    - latency_p99_ms
    - "*_p99"
  metricIdentityAttributes:
    - region
  heartbeatMetric: health.heartbeat
  destinationAttributes:
    - peer.service
    - server.address
  spanKinds:
    - CLIENT
  logMinSeverity: ERROR
  logEventNameAllowlist:
    - payment_failed
  dependencyTtlSecs: 300
  logEventTtlSecs: 60
  rejectDerived: true
  maxRequestBytes: 4194304
```

Builder equivalent:

```rust
use drasi_source_otel::OtelSource;

let source = OtelSource::builder("otel")
    .with_grpc_bind("0.0.0.0:4317")
    .with_metric_allowlist(["latency_p99_ms", "*_p99"])
    .with_heartbeat_metric("health.heartbeat")
    .with_dependency_ttl_secs(300)
    .build()?;
```

## Data mapping

See [GRAPH_SCHEMA.md](GRAPH_SCHEMA.md). OTLP timestamps are nanoseconds and are converted to millisecond `effective_from` values.

## Integration test

Requires Docker.

```bash
cargo test -p drasi-source-otel --test integration_test -- --ignored --nocapture
# or
make -C components/sources/otel integration-test
```

Starts `otel/opentelemetry-collector` via testcontainers and sends metrics, traces, and logs through it.

## Operations

This is an **ingress** source. There is no upstream poller, so there is no reconnect loop. The Collector or SDK retries failed OTLP exports.

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
| Source is running but no metric updates | Name on `metricAllowlist`? Resource has `service.name`? Empty allowlist denies all |
| No DEPENDS_ON | Span kind in `spanKinds` (default CLIENT)? A `destinationAttributes` key present on the span? |
| Edges never disappear | `dependencyTtlSecs` and sweeper; TTL is receipt time |
| `effective_from` rejected | Source converts nanos to millis in mapping |
