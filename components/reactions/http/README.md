# Drasi HTTP Reaction

`drasi-reaction-http` is a unified HTTP reaction for [Drasi](https://drasi.io). It forwards continuous query results to one or more HTTP endpoints, with full support for Handlebars templating, per-query routing, and an optional **adaptive batching** mode that automatically scales batch sizes to match incoming load.

This crate replaces the previous `drasi-reaction-http-adaptive` crate; both standard per-result delivery and adaptive batched delivery are now driven by the same configuration schema.

> **Note** — version `0.3.0` is a breaking change. The configuration schema, builder API, and runtime types have all changed. There is no compatibility shim with `0.2.x` configurations or with `drasi-reaction-http-adaptive`.

---

## Table of contents

1. [Features](#features)
2. [Quick start](#quick-start)
3. [Configuration reference](#configuration-reference)
4. [Templating](#templating)
5. [Per-query routing](#per-query-routing)
6. [Adaptive batching](#adaptive-batching)
7. [Batch endpoint coalescing](#batch-endpoint-coalescing)
8. [Render-error fallback](#render-error-fallback)
9. [Authentication](#authentication)
10. [Builder API](#builder-api)
11. [Plugin metadata](#plugin-metadata)
12. [Testing](#testing)
13. [Changelog](#changelog)

---

## Features

- Forward Drasi continuous-query result diffs to any HTTP endpoint.
- Handlebars templates for the URL, method, body, and headers — per operation (`added`, `updated`, `deleted`).
- Per-query template overrides plus a shared `outputTemplates` default.
- Optional **adaptive batching** that dynamically scales batch size between configurable min/max bounds based on observed throughput.
- Optional **batch endpoint** that coalesces multi-result batches into a single POST, reducing overhead for fan-out scenarios.
- Always-on HTTP/2 connection pooling.
- Render-error fallback that POSTs the raw diff to `/changes/{queryId}` instead of dropping events.
- Bearer-token authentication.
- Ships as both an embedded Rust dependency and a dynamic plugin (`--features dynamic-plugin`).

---

## Quick start

Standard per-result delivery:

```yaml
apiVersion: v1
kind: Reaction
name: my-http-reaction
spec:
  kind: http
  properties:
    baseUrl: "https://example.com/api"
    timeoutMs: 5000
    queries:
      - queryId: orders-by-region
```

Adaptive batched delivery to a coalesced batch endpoint:

```yaml
apiVersion: v1
kind: Reaction
name: my-adaptive-http-reaction
spec:
  kind: http
  properties:
    baseUrl: "https://example.com/api"
    timeoutMs: 5000
    batchEndpoint: "/events/batch"
    adaptive:
      adaptiveMinBatchSize: 50
      adaptiveMaxBatchSize: 2000
      adaptiveWindowSize: 100
      adaptiveBatchTimeoutMs: 500
    queries:
      - queryId: orders-by-region
```

---

## Configuration reference

All top-level fields are camelCase.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `baseUrl` | `string` | `http://localhost` | Base URL prepended to every relative per-call URL. Optional. |
| `token` | `string` | _none_ | Bearer token sent as `Authorization: Bearer <token>`. |
| `timeoutMs` | `u64` | `5000` | Per-request HTTP timeout. |
| `outputTemplates` | object | _none_ | Shared per-operation templates and per-query routes (see [Templating](#templating)). |
| `batchEndpoint` | `string` | _none_ | When set, `adaptive` **must** also be enabled; every coalesced batch is POSTed to `{baseUrl}{batchEndpoint}` as a single payload. If `batchEndpoint` is set without `adaptive`, the reaction fails to start. |
| `adaptive` | object | _none_ | When present, enables adaptive batching (see [Adaptive batching](#adaptive-batching)). |

### `outputTemplates` (and per-query `routes`)

| Field | Type | Description |
|-------|------|-------------|
| `added` | `TemplateSpec<HttpCallExt>` | Template applied when a result row is added. |
| `updated` | `TemplateSpec<HttpCallExt>` | Template applied when a result row is updated. |
| `deleted` | `TemplateSpec<HttpCallExt>` | Template applied when a result row is deleted. |

Each `TemplateSpec<HttpCallExt>` looks like:

```yaml
template: "<Handlebars body template>"
url: "<Handlebars URL template>"
method: POST              # GET | POST | PUT | PATCH | DELETE
headers:
  Content-Type: "application/json"
  X-Custom-Header: "value"
```

---

## Templating

Templates use [Handlebars](https://handlebarsjs.com/) syntax. The context exposes the following variables, depending on the operation:

| Variable | Available for | Meaning |
|----------|---------------|---------|
| `after` | added, updated | The new/current row payload. |
| `before` | updated, deleted | The previous row payload. |
| `data` | updated, and non-row results | The raw diff payload. |
| `query_name` | all | The query identifier. |
| `operation` | all | The operation: `ADD`, `UPDATE`, or `DELETE`. |

Example body template:

```handlebars
{
  "query": "{{query_name}}",
  "operation": "{{operation}}",
  "row": {{json after}}
}
```

When `url` or `method` or `headers` are templated, the same context applies.

---

## Per-query routing

The `outputTemplates.routes` map lets each query opt into per-query template
overrides, keyed by query id; `outputTemplates.defaultTemplate` applies to any
query without a matching route:

```yaml
outputTemplates:
  defaultTemplate:
    added:
      template: '{{json after}}'
      url: "/events"
      method: POST
  routes:
    orders-by-region:
      added:
        template: '{"event":"created","order":{{json after}}}'
        url: "/orders"
        method: POST
      deleted:
        template: '{"event":"cancelled","orderId":"{{before.id}}"}'
        url: "/orders/{{before.id}}"
        method: DELETE
```

Route keys are matched against the full query id only. If neither a route nor a
default template matches the operation, the reaction falls back to a built-in
default: POST the raw diff to `{baseUrl}/changes/{queryId}`.

---

## Adaptive batching

Set the `adaptive` block to enable an internal batcher that dynamically scales batch sizes based on observed throughput.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `adaptiveMinBatchSize` | `usize` | `1` | Lower bound on batch size. |
| `adaptiveMaxBatchSize` | `usize` | `100` | Upper bound on batch size. |
| `adaptiveWindowSize` | `usize` | `10` | Throughput sample window in 100 ms units (e.g. `10` = 1 s, `50` = 5 s, `100` = 10 s). |
| `adaptiveBatchTimeoutMs` | `u64` | `100` | Maximum time to wait before flushing a partial batch. |

The adaptive loop:

1. Pulls query results off the internal channel.
2. Groups results into batches whose size is decided by the batcher's throughput monitor.
3. Delivers each batch either (a) coalesced to `batchEndpoint`, or (b) fanned out per-route using the same templating rules as the standard loop.

> **Tip** — when both `adaptive` and `batchEndpoint` are set, every coalesced batch is POSTed to `batchEndpoint` as a single payload. When `batchEndpoint` is not set, each batch is fanned out per-route instead.

---

## Batch endpoint coalescing

When `batchEndpoint` is set, the adaptive loop POSTs a single JSON array, one
entry per coalesced query:

```json
[
  {
    "query_id": "orders-by-region",
    "results": [ { "type": "ADD", "data": { } } ],
    "timestamp": "2026-01-01T00:00:00+00:00",
    "count": 1
  }
]
```

The URL is `{baseUrl}{batchEndpoint}`. The endpoint must accept POST.

---

## Render-error fallback

If a per-result template (URL, body, or headers) fails to render — for example, because a template references a field that's missing from the payload — the reaction:

1. Logs a warning with the query ID and the rendering error.
2. POSTs the raw diff JSON to `{baseUrl}/changes/{queryId}` as a fallback so events are not silently dropped.

This behavior is always on; there is no configuration to disable it.

---

## Authentication

Set `token` to send a bearer token on every request:

```yaml
properties:
  baseUrl: https://example.com
  token: ${SECRET:my-api-token}
```

Drasi's secret-store integrations (`file`, `keyring`, Azure Key Vault) all work transparently.

---

## Builder API

Use `HttpReactionBuilder` to construct an `HttpReaction` programmatically (for embedded use or tests):

```rust
use drasi_reaction_http::{
    AdaptiveBatchConfig, HttpCallExt, HttpQueryConfig, HttpReactionBuilder, TemplateSpec,
};
use std::collections::HashMap;

let reaction = HttpReactionBuilder::new("my-http")
    .with_queries(vec!["orders".to_string()])
    .with_base_url("http://localhost:8080")
    .with_timeout_ms(5_000)
    .with_token("secret")
    .with_default_template(HttpQueryConfig {
        added: Some(TemplateSpec {
            template: r#"{"data":{{json after}}}"#.to_string(),
            extension: HttpCallExt {
                url: "/events".to_string(),
                method: "POST".to_string(),
                headers: HashMap::new(),
            },
        }),
        ..Default::default()
    })
    .with_query_template(
        "orders",
        HttpQueryConfig {
            deleted: Some(TemplateSpec {
                template: r#"{"orderId":"{{before.id}}"}"#.to_string(),
                extension: HttpCallExt {
                    url: "/orders/{{before.id}}".to_string(),
                    method: "DELETE".to_string(),
                    headers: HashMap::new(),
                },
            }),
            ..Default::default()
        },
    )
    .with_adaptive(AdaptiveBatchConfig {
        adaptive_min_batch_size: 10,
        adaptive_max_batch_size: 500,
        adaptive_window_size: 50,
        adaptive_batch_timeout_ms: 250,
    })
    .with_batch_endpoint("/events/batch")
    .build()?;
```

> The convenience setters (`with_min_batch_size`, `with_max_batch_size`,
> `with_window_size`, `with_batch_timeout_ms`) enable adaptive mode implicitly
> if it is not already enabled.

---

## Plugin metadata

| Property | Value |
|----------|-------|
| Plugin kind | `http` |
| Config version | `2.0.0` |
| Crate version | `0.3.0` |
| Dynamic plugin feature | `dynamic-plugin` |

Build as a dynamic plugin:

```bash
cargo build -p drasi-reaction-http --features dynamic-plugin
```

The descriptor publishes named OpenAPI sub-schemas for `HttpReactionConfig`, `HttpOutputTemplates`, `HttpTemplateSpec`, `HttpQueryConfig`, and `AdaptiveBatchConfig`, plus `SchemaUiAnnotator` groupings so management UIs can render the unified form correctly.

---

## Testing

```bash
cargo test -p drasi-reaction-http
```

Unit tests cover config defaults, builder semantics, template routing, lifecycle, and runtime conversion. Integration tests use [`wiremock`](https://crates.io/crates/wiremock) to exercise:

- Standard per-result delivery with a per-query template.
- Bearer-token header injection.
- Render-error fallback to `/changes/{queryId}`.
- Default fallback when no template matches.
- Adaptive mode with `batchEndpoint` set — coalesced batch POST.
- Adaptive mode without `batchEndpoint` — per-route fan-out.

---

## Changelog

See [CHANGELOG.md](./CHANGELOG.md). The `0.3.0` entry documents the breaking unification of `drasi-reaction-http` and `drasi-reaction-http-adaptive`.
