# OpenTelemetry getting started

This example starts `drasi-source-otel` on `127.0.0.1:4317` and logs query results for checkout p99 latency.

## How to verify it's working

1. `./quickstart.sh`
2. In another terminal: `./test-updates.sh`
3. The example process should log an **Added** row for `checkout` / `920`, then an **Updated** row for `700`.

## Helper scripts

| Script | Purpose |
| --- | --- |
| `setup.sh` | Build the example (up to 60 attempts, 2s apart) |
| `quickstart.sh` | Build and run the listener |
| `diagnose.sh` | Check that port 4317 is open |
| `test-updates.sh` | Send CREATE/UPDATE gauge and a client span |

## Troubleshooting

- `Connection refused`: start `./quickstart.sh` first.
- No query output: the metric name must be `latency_p99_ms` and the Resource must include `service.name=checkout`.
- Optional Collector path: `otel/opentelemetry-collector-contrib:0.136.0` exporting OTLP/gRPC to `host.docker.internal:4317` with `tls.insecure: true`.
