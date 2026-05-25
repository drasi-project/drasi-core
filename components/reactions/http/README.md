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
- Handlebars templates for the URL, method, body, and headers — per operation (`add`, `update`, `delete`, `control`).
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
      adaptive_min_batch_size: 50
      adaptive_max_batch_size: 2000
      adaptive_window_size: 100
      adaptive_batch_timeout_ms: 500
    queries:
      - queryId: orders-by-region
```

---

## Configuration reference

All top-level fields are camelCase. `config_version` is `2.0.0`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `baseUrl` | `string` | _required_ | Base URL prepended to every per-call URL. |
| `token` | `string` | _none_ | Bearer token sent as `Authorization: Bearer <token>`. |
| `timeoutMs` | `u64` | `5000` | Per-request HTTP timeout. |
| `outputTemplates` | object | _none_ | Shared per-operation templates (see [Templating](#templating)). |
| `batchEndpoint` | `string` | _none_ | When set **and** the adaptive loop sees a batch with results from a query >1, POST the entire batch to `{baseUrl}{batchEndpoint}` as a single coalesced payload. |
| `adaptive` | object | _none_ | When present, enables adaptive batching (see [Adaptive batching](#adaptive-batching)). |
| `queries` | array | `[]` | Per-query overrides (see [Per-query routing](#per-query-routing)). |

### `outputTemplates` and per-query `outputTemplates`

| Field | Type | Description |
|-------|------|-------------|
| `add` | `TemplateSpec<HttpCallExt>` | Template applied when a result is added. |
| `update` | `TemplateSpec<HttpCallExt>` | Template applied when a result is updated. |
| `delete` | `TemplateSpec<HttpCallExt>` | Template applied when a result is deleted. |
| `control` | `TemplateSpec<HttpCallExt>` | Template applied for control events. |

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

Templates use [Handlebars](https://handlebarsjs.com/) syntax. The following data is available in the template context:

| Variable | Meaning |
|----------|---------|
| `queryId` | The query identifier. |
| `sequence` | Result sequence number. |
| `timestamp` | Result timestamp (RFC 3339). |
| `data` | The result row (`Add`/`Update`/`Delete` payload). |

Example body template:

```handlebars
{
  "queryId": "{{queryId}}",
  "sequence": {{sequence}},
  "data": {{json data}}
}
```

When `url` or `method` or `headers` are templated, the same context applies.

---

## Per-query routing

The `queries` array lets each query opt into per-query template overrides:

```yaml
queries:
  - queryId: orders-by-region
    outputTemplates:
      add:
        template: '{"event":"created","order":{{json data}}}'
        url: "/orders"
        method: POST
      delete:
        template: '{"event":"cancelled","orderId":"{{data.id}}"}'
        url: "/orders/{{data.id}}"
        method: DELETE
  - queryId: shipments
    # falls back to top-level `outputTemplates`
```

If neither a per-query template nor a top-level template matches the operation, the reaction falls back to a built-in default: POST the raw diff to `{baseUrl}/changes/{queryId}`.

---

## Adaptive batching

Set the `adaptive` block to enable an internal batcher that dynamically scales batch sizes based on observed throughput.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `adaptive_min_batch_size` | `usize` | `1` | Lower bound on batch size. |
| `adaptive_max_batch_size` | `usize` | `100` | Upper bound on batch size. |
| `adaptive_window_size` | `usize` | `10` | Throughput sample window. |
| `adaptive_batch_timeout_ms` | `u64` | `100` | Maximum time to wait before flushing a partial batch. |

The adaptive loop:

1. Pulls query results off the internal channel.
2. Groups results into batches whose size is decided by the batcher's throughput monitor.
3. Delivers each batch either (a) coalesced to `batchEndpoint`, or (b) fanned out per-route using the same templating rules as the standard loop.

> **Tip** — when both `adaptive` and `batchEndpoint` are set, the loop will still fan out per-route if every query in a batch only contributed one result (single-result batches don't benefit from coalescing).

---

## Batch endpoint coalescing

When `batchEndpoint` is set, the adaptive loop POSTs a single JSON payload like:

```json
{
  "batchSize": 12,
  "results": [
    { "queryId": "orders-by-region", "sequence": 41, "data": [...] },
    { "queryId": "orders-by-region", "sequence": 42, "data": [...] },
    ...
  ]
}
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
use drasi_reaction_http::HttpReactionBuilder;
use drasi_lib::reactions::common::config::OperationType;

let reaction = HttpReactionBuilder::new("my-http")
    .with_base_url("http://localhost:8080".to_string())
    .with_timeout_ms(5_000)
    .with_token(Some("secret".to_string()))
    .with_default_template(
        OperationType::Add,
        /* template */ r#"{"data":{{json data}}}"#.to_string(),
        /* url */ Some("/events".to_string()),
        /* method */ Some("POST".to_string()),
        /* headers */ None,
    )
    .with_query_template(
        "orders",
        OperationType::Delete,
        r#"{"orderId":"{{data.id}}"}"#.to_string(),
        Some("/orders/{{data.id}}".to_string()),
        Some("DELETE".to_string()),
        None,
    )
    .with_adaptive(true)
    .with_min_batch_size(10)
    .with_max_batch_size(500)
    .with_window_size(50)
    .with_batch_timeout_ms(250)
    .with_batch_endpoint(Some("/events/batch".to_string()))
    .build()?;
```

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
