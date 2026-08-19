# OTel source tests

## Unit tests

```bash
cargo test -p drasi-source-otel
```

Covers mapping, lifecycle Insert/Update/TTL, config validation, builder construction, bind-before-Running failure, WAL `resume_from` validation, and `PositionUnavailable`.

## Integration test

Requires Docker.

```bash
cargo test -p drasi-source-otel --test integration_test -- --ignored --nocapture
# or
make -C components/sources/otel integration-test
```

Starts `otel/opentelemetry-collector:0.136.0` via testcontainers and sends OTLP to the Collector, which forwards to `drasi-source-otel`:

1. Gauge `latency_p99_ms=920` → metric **Add**
2. Gauge `latency_p99_ms=700` → metric **Update**
3. CLIENT span `peer.service=payments` → `DEPENDS_ON` **Add**
4. ERROR log `payment_failed` → `LogEvent` **Add**
5. Wait for `dependencyTtlSecs=2` → `DEPENDS_ON` **Delete**
