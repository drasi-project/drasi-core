# gRPC Reaction

`drasi-reaction-grpc` streams Drasi continuous-query results to a downstream gRPC service using the `ReactionService` defined in [`proto/drasi/v1/reaction.proto`](proto/drasi/v1/reaction.proto).

It supports two batching strategies (selected by configuration) and an optional Handlebars-based output template system that lets you reshape each result item before it is sent on the wire.

> **⚠️ Breaking change (v0.4.0).** This crate replaces both the previous `drasi-reaction-grpc` and the now-removed `drasi-reaction-grpc-adaptive` crate. The configuration schema has been redesigned (camelCase, batching mode selector, output templates). There is no backward compatibility with previous configurations. See [`CHANGELOG.md`](CHANGELOG.md).

## Features

- **Streaming gRPC delivery** to a configurable endpoint, with retry/reconnect.
- **Two batching modes**:
  - **`fixed`** — flush when `batchSize` items have accumulated or `batchFlushTimeoutMs` elapses (whichever comes first).
  - **`adaptive`** — throughput-aware batching that scales batch size between `adaptiveMinBatchSize` and `adaptiveMaxBatchSize` based on observed event rate.
- **Optional output templates** (Handlebars) per query / per operation (`added`, `updated`, `deleted`). When templates are not configured, the raw `before`/`after` payload is forwarded as-is.
- **Per-call gRPC metadata** for authentication or routing headers.
- Conforms to the modern Drasi reaction plugin patterns: camelCase typed config, OpenAPI schema descriptor, builder API, and optional `dynamic-plugin` feature.

## Configuration

Fields are camelCase in JSON / YAML. Defaults are shown in parentheses.

| Field | Type | Default | Description |
|---|---|---|---|
| `endpoint` | `String` | `grpc://localhost:50052` | gRPC server endpoint to connect to. |
| `timeoutMs` | `u64` | `5000` | Per-RPC timeout. |
| `maxRetries` | `u32` | `3` | Maximum retry attempts per batch send. |
| `connectionRetryAttempts` | `u32` | `5` | Maximum attempts when (re)establishing the channel. |
| `initialConnectionTimeoutMs` | `u64` | `10000` | Timeout for each connection attempt. |
| `metadata` | `Map<String,String>` | `{}` | Static gRPC metadata headers added to every RPC. |
| `batching` | `BatchingConfig` | `{ mode: "fixed", ... }` | Batching strategy — see below. |
| `outputTemplates` | `OutputTemplates?` | _unset_ | Optional Handlebars output templates — see below. |

### Batching

The `batching` object is discriminated by `mode`.

**Fixed batching:**

```yaml
batching:
  mode: fixed
  batchSize: 100              # default 100
  batchFlushTimeoutMs: 1000   # default 1000
```

**Adaptive batching** (camelCase fields, matching the rest of the config schema):

```yaml
batching:
  mode: adaptive
  adaptiveMinBatchSize: 10          # default 1
  adaptiveMaxBatchSize: 1000        # default 100
  adaptiveWindowSize: 10            # default 10 (= 1 second; units are 100 ms)
  adaptiveBatchTimeoutMs: 500       # default 1000
```

### Output Templates (optional)

When `outputTemplates` is set, the reaction renders the configured Handlebars template for each diff and uses the rendered JSON as the `data` payload on the wire. If a template is empty, parsing fails, or rendering fails, the reaction logs a warning and falls back to the raw payload — events are never dropped.

```yaml
outputTemplates:
  defaultTemplate:
    added:   { template: '{"event":"add","row":{{json after}}}' }
    updated: { template: '{"event":"update","row":{{json after}},"prev":{{json before}}}' }
    deleted: { template: '{"event":"delete","row":{{json before}}}' }
  routes:
    "high-volume-query":
      added:   { template: '{"id":"{{after.id}}"}' }
```

Handlebars context fields available in a template:

| Field | Description |
|---|---|
| `before` | The previous row state (object / map, absent for `ADD`). |
| `after`  | The new row state (object / map, absent for `DELETE`). |
| `data`   | The full diff payload as JSON (the same shape that would be sent without a template). |
| `operation` | `ADD`, `UPDATE`, `DELETE`, `AGGREGATION`, or `NOOP`. |
| `query_id` | The originating query id. |

A `json` helper is registered so templates can emit a nested object/array as JSON (e.g. `{{json after}}`).

If `outputTemplates` is not set at all, the original payload is forwarded with no rendering overhead.

## Example YAML

**Fixed batching, no templates:**

```yaml
endpoint: grpc://collector.observability.svc.cluster.local:50052
timeoutMs: 2000
metadata:
  x-api-key: "${API_KEY}"
batching:
  mode: fixed
  batchSize: 250
  batchFlushTimeoutMs: 500
```

**Adaptive batching with templates:**

```yaml
endpoint: grpc://collector:50052
batching:
  mode: adaptive
  adaptiveMinBatchSize: 50
  adaptiveMaxBatchSize: 2000
  adaptiveWindowSize: 20
  adaptiveBatchTimeoutMs: 250
outputTemplates:
  defaultTemplate:
    added:   { template: '{"op":"add","row":{{json after}}}' }
    updated: { template: '{"op":"upd","row":{{json after}}}' }
    deleted: { template: '{"op":"del","row":{{json before}}}' }
```

## Builder API

```rust,ignore
use drasi_reaction_grpc::{
    GrpcReaction, GrpcReactionConfig, BatchingConfig, OutputTemplates,
};
use drasi_lib::reactions::common::{AdaptiveBatchConfig, QueryConfig, TemplateSpec};

// Fixed batching (default).
let reaction = GrpcReaction::builder("my-grpc-reaction")
    .with_endpoint("grpc://collector:50052")
    .with_fixed_batching(/* batch_size */ 200, /* flush_ms */ 500)
    .with_metadata("x-api-key", "abc")
    .build()?;

// Adaptive batching with per-query templates.
let templates = OutputTemplates {
    default_template: Some(QueryConfig {
        added: Some(TemplateSpec::new(
            r#"{"op":"add","row":{{json after}}}"#,
        )),
        ..Default::default()
    }),
    ..Default::default()
};

let reaction = GrpcReaction::builder("my-grpc-reaction")
    .with_endpoint("grpc://collector:50052")
    .with_adaptive_batching(AdaptiveBatchConfig::default())
    .with_output_templates(templates)
    .build()?;
```

## Operational Notes

- The reaction sends each batch as a unary `ProcessResults` RPC on the `ReactionService`. On a connection-level error it reconnects with exponential backoff bounded by `connectionRetryAttempts` × `initialConnectionTimeoutMs`.
- `maxRetries` controls per-batch retry on transient send errors. Batches that exhaust retries are logged and dropped, and the client reconnects before the next batch.
- All event/result delivery is best-effort at-least-once at the gRPC level; downstream consumers should be idempotent.
- The reaction processing task is supervised by the host runtime via `ComponentStatusHandle`; a panic surfaces as a component failure.

## Building as a Dynamic Plugin

```bash
cargo build -p drasi-reaction-grpc --release --features dynamic-plugin
```

The produced `cdylib` exports the standard Drasi plugin entry point.

## License

Apache-2.0 — see the workspace [`LICENSE`](../../../LICENSE).
