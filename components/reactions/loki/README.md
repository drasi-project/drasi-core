# Loki Reaction

A Drasi reaction plugin that pushes continuous query result changes to Grafana Loki.

## Overview

The Loki reaction sends ADD/UPDATE/DELETE query result diffs to Loki using the HTTP Push API (`POST /loki/api/v1/push`).  
It supports:

- Per-query template routing
- Default templates
- Dynamic labels rendered via Handlebars
- Bearer token, Basic auth, and `X-Scope-OrgID`

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

## Makefile Targets

- `make build`
- `make test`
- `make integration-test`
- `make lint`

## Known Limitations

- No retry queue if Loki is unavailable
- Aggregation and Noop diffs are ignored
- High-cardinality dynamic labels can degrade Loki performance
