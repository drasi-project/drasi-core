# OTel source tests

## Unit tests

```bash
cargo test -p drasi-source-otel
```

Covers mapping, lifecycle Insert/Update/TTL, config validation, builder construction, bind-before-Running failure, WAL `resume_from` validation, and `PositionUnavailable`.

## Integration test

```bash
cargo test -p drasi-source-otel -- --ignored --nocapture
```

Client harness (no Docker):

1. Start `OtelSource` on ephemeral gRPC/HTTP ports
2. Send an allowlisted OTLP gauge (`920`) and assert query **Add**
3. Send an updated gauge (`700`) and assert **Update**
4. Send a CLIENT span with `peer.service=payments` and assert **Add** on `DEPENDS_ON`
5. Wait for `dependency_ttl_secs=2` and assert **Delete**
