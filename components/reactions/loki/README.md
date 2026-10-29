# Loki Reaction

A Drasi reaction plugin that pushes continuous query result changes to Grafana Loki.

## Overview

The Loki reaction sends ADD/UPDATE/DELETE query result diffs to Loki using the HTTP Push API (`POST /loki/api/v1/push`).  
It supports:

- Per-query template routing
- Default templates
- Dynamic labels rendered via Handlebars
- Bearer token, Basic auth, and `X-Scope-OrgID`

## Lifecycle and delivery boundary

Startup and stop are serialized. The HTTP client is built and the processor is
registered before spawning and publishing Running. A failed client build returns
its original error. Running is local readiness, not proof of Loki availability.

Stop signals the owned processor and waits without aborting an in-flight push or
response-body read. Cancellation or the two-second join deadline reports
incomplete cleanup and retains the worker. Retry stop after that work completes;
restart remains blocked until processor and base cleanup both finish. Status
changes alone cannot discard input already dequeued before shutdown signalling.

The legacy graph boundary remains **Accepted**, not completed remote handling.
Existing render/push failure logging and skip/fallback behavior are unchanged.
There is no durable retry queue, consumer checkpoint or atomic effect/receipt
transaction, and stopping does not promise to drain the entire queued backlog.

## Configuration

### Builder Example

```rust
use drasi_reaction_loki::{LokiReaction, QueryConfig, TemplateSpec};

let reaction = LokiReaction::builder("loki-reaction")
    .with_query("hot-sensors")
    .with_endpoint("http://localhost:3100")
    .with_label("job", "drasi")
    .with_label("sensor_type", "{{after.type}}")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new(r#"{"event":"ADD","id":"{{after.id}}","temp":{{after.temperature}}}"#)),
        updated: Some(TemplateSpec::new(r#"{"event":"UPDATE","id":"{{after.id}}","before":{{before.temperature}},"after":{{after.temperature}}}"#)),
        deleted: Some(TemplateSpec::new(r#"{"event":"DELETE","id":"{{before.id}}"}"#)),
    })
    .build()?;
```

### Template Context

Templates can use:

- `after` (ADD, UPDATE)
- `before` (UPDATE, DELETE)
- `data` (UPDATE)
- `query_name`
- `operation` (`ADD`, `UPDATE`, `DELETE`)
- `timestamp` (RFC3339 string)

## Integration Test

Run integration test (requires Docker):

```bash
cargo test -p drasi-reaction-loki -- --ignored --nocapture
```

The test uses `grafana/loki:3.4.3` and verifies INSERT, UPDATE, DELETE events by querying Loki APIs.
It awaits reaction readiness and permanent graph/container cleanup. The runtime
parity runner includes this container test. Local current-thread lifecycle tests
hold actual HTTP replies and partial error bodies through cancelled/timed-out
stop, cover three restarts, and check registration, status races and typed panic
cleanup.

## Makefile Targets

- `make build`
- `make test`
- `make integration-test`
- `make lint`

## Known Limitations

- No retry queue if Loki is unavailable
- Aggregation diffs use the UPDATE path; Noop diffs are ignored
- High-cardinality dynamic labels can degrade Loki performance
