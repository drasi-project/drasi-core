# OpenTelemetry getting started

This example runs an OpenTelemetry Collector in Docker and a Drasi OTLP source on the host.

```text
send-otlp  -->  Collector :4317  -->  Drasi source :14317  -->  log reaction
```

The Collector is the OTLP endpoint you send to. It forwards traces, metrics, and logs to `drasi-source-otel`, which projects allowlisted telemetry into the live graph.

Config keys are documented in [`components/sources/otel/README.md`](../../../components/sources/otel/README.md).

## Prerequisites

- Docker + Docker Compose
- Rust 1.95+

## How to verify it's working

1. `./quickstart.sh` (starts the Collector, then the example app)
2. In another terminal: `./test-updates.sh`
3. The example process should log an **Added** row for `checkout` / `920`, then an **Updated** row for `700`.

## Ports

| Process | Address | Role |
| --- | --- | --- |
| Collector OTLP/gRPC | `127.0.0.1:4317` | Inbound telemetry (what you export to) |
| Collector OTLP/HTTP | `127.0.0.1:4318` | Optional HTTP protobuf ingest |
| Collector health | `127.0.0.1:13133` | Readiness check |
| Drasi source | `0.0.0.0:14317` | Receives forwarded OTLP from the Collector |

## Helper scripts

| Script | Purpose |
| --- | --- |
| `setup.sh` | Start the Collector and build the example |
| `quickstart.sh` | Setup, then run the Drasi listener |
| `diagnose.sh` | Check Collector health and both OTLP ports |
| `test-updates.sh` | Send CREATE/UPDATE gauge and a client span to the Collector |

## Troubleshooting

- `Connection refused` on 4317: run `./setup.sh` so the Collector container is up.
- Collector is healthy but no query output: start `./quickstart.sh` so Drasi is listening on 14317. The Collector retries until the source is up.
- Collector logs `gzip which isn't supported`: restart the example so it picks up gzip-enabled gRPC.
- No query output: the metric name must be `latency_p99_ms` and the Resource must include `service.name=checkout`.
- Linux Docker cannot reach the source: the example binds `0.0.0.0:14317` and compose sets `host.docker.internal:host-gateway`.
