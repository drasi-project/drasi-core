# Drasi HTTP Reaction

`drasi-reaction-http` is the HTTP reaction for the [Drasi](https://drasi.io) embedded
runtime ([`drasi-lib`](../../../lib)). It forwards the result changes produced by Drasi
continuous queries to HTTP endpoints.

Changes can be distributed two ways:

- **Single notifications** (default) — each added / updated / deleted result row is delivered
  as its own HTTP request.
- **Adaptive batching** (optional) — result changes are coalesced into batches whose size
  scales automatically with incoming load, trading a small amount of latency for higher
  throughput and reduced network traffic. Enabled by adding an `AdaptiveBatchConfig` and
  a `batchEndpoint`.

Single-notification mode supports [Handlebars](https://handlebarsjs.com/) templating of
the request URL, body, and headers, plus per-query routing and bearer-token authentication.
Adaptive batch mode coalesces changes into a canonical `BatchEnvelope` POSTed to
`batchEndpoint`; per-query **body** templates still apply to each item (the URL, method, and
headers do not, because a batch is a single request).

The reaction is built in Rust with `HttpReactionBuilder` and attached to a `DrasiLib`
instance either at build time (`DrasiLib::builder().with_reaction(...)`) or at runtime
(`core.add_reaction(...)`). See
[Adding the reaction to a DrasiLib instance](#adding-the-reaction-to-a-drasilib-instance).

---

## Table of contents

1. [When to use this reaction](#when-to-use-this-reaction)
2. [Quick start](#quick-start)
3. [Adding the reaction to a DrasiLib instance](#adding-the-reaction-to-a-drasilib-instance)
4. [Configuration reference](#configuration-reference)
   - [Declarative (dynamic-plugin) configuration](#declarative-dynamic-plugin-configuration)
5. [Output templates and per-query routing](#output-templates-and-per-query-routing)
6. [Templating](#templating)
7. [Output payload schema](#output-payload-schema)
   - [`DefaultChangeNotification` envelope (single-notification path)](#defaultchangenotification-envelope-single-notification-path)
   - [`BatchEnvelope` (batch-endpoint path)](#batchenvelope-batch-endpoint-path)
8. [Adaptive batching](#adaptive-batching)
9. [Render-error fallback](#render-error-fallback)
10. [Delivery failure handling](#delivery-failure-handling)
11. [Authentication](#authentication)
12. [Examples](#examples)
13. [Plugin metadata](#plugin-metadata)
14. [Testing](#testing)
15. [Changelog](#changelog)

---

## When to use this reaction

Use the HTTP reaction when you want Drasi continuous-query result changes delivered to an
**HTTP endpoint** — a webhook, a REST API, a serverless function, or any service that
accepts HTTP requests. Typical uses:

- **Webhooks / event notifications** — POST each change to a callback URL.
- **REST integration** — map `added` / `updated` / `deleted` to `POST` / `PUT` / `DELETE`
  against a resource API, shaping the URL and body per query.
- **High-throughput ingestion** — coalesce many changes into batched POSTs to a bulk
  endpoint with [adaptive batching](#adaptive-batching).

Choose **single-notification** mode (the default) when each change should be its own
request and may need its own URL, method, headers, or body. Choose **adaptive batching**
when the receiver accepts many changes per request and per-request overhead is the
bottleneck.

If HTTP is not the right transport, reach for a protocol-specific reaction instead — for
example [gRPC](../grpc), [SSE](../sse), [RabbitMQ](../rabbitmq), or [AWS SQS](../aws-sqs).

---

## Quick start

Here is a complete example of how to use the HTTP Reaction. It uses a `MockSource` to
generate simulated sensor data, a Cypher continuous query that surfaces hot sensors,
and the HTTP reaction that forwards each result change to an HTTP endpoint.

Add the crates to your `Cargo.toml`:

```toml
[dependencies]
drasi-lib            = { path = "../drasi-core/lib" }
drasi-source-mock    = { path = "../drasi-core/components/sources/mock" }
drasi-reaction-http  = { path = "../drasi-core/components/reactions/http" }

anyhow      = "1"
tokio       = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
env_logger  = "0.11"
```

Rust code for the example:

```rust
use std::sync::Arc;

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_http::HttpReactionBuilder;
use drasi_source_mock::{DataType, MockSource};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // 1. Source: emit a new SensorReading for each of 3 sensors every 2 seconds.
    let source = MockSource::builder("sensor-source")
        .with_data_type(DataType::sensor_reading(3))
        .with_interval_ms(2_000)
        .build()?;

    // 2. Continuous query: surface sensors whose temperature crosses above 25 °C.
    let hot_sensors = Query::cypher("hot-sensors")
        .query(
            r#"
            MATCH (s:SensorReading)
            WHERE s.temperature > 25
            RETURN s.sensor_id   AS sensor_id,
                   s.temperature AS temperature,
                   s.humidity    AS humidity
            "#,
        )
        .from_source("sensor-source")
        .enable_bootstrap(true)
        .auto_start(true)
        .build();

    // 3. Reaction: POST each result change to https://example.com/api/changes/hot-sensors
    //    (the built-in default endpoint when no output templates are configured).
    let reaction = HttpReactionBuilder::new("hot-sensors-webhook")
        .with_queries(vec!["hot-sensors".to_string()])
        .with_base_url("https://example.com/api")
        .with_timeout_ms(5_000)
        .build()?;

    // 4. Wire everything into a DrasiLib instance and start.
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("http-reaction-quickstart")
            .with_source(source)
            .with_query(hot_sensors)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    core.start().await?;

    println!("HTTP reaction running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
```

With no `output_templates` configured, each result row is POSTed to
`{base_url}/changes/{query_id}` — `https://example.com/api/changes/hot-sensors` in the
example above (see [Output payload schema](#output-payload-schema)).

To switch the same reaction to adaptive batched delivery, add an `AdaptiveBatchConfig`
and a batch endpoint:

```rust
use drasi_reaction_http::{AdaptiveBatchConfig, HttpReactionBuilder};

let reaction = HttpReactionBuilder::new("hot-sensors-webhook")
    .with_queries(vec!["hot-sensors".to_string()])
    .with_base_url("https://example.com/api")
    .with_adaptive(AdaptiveBatchConfig {
        adaptive_min_batch_size: 50,
        adaptive_max_batch_size: 2_000,
        adaptive_window_size: 100,
        adaptive_batch_timeout_ms: 500,
    })
    .with_batch_endpoint("/events/batch")
    .build()?;
```

---

## Adding the reaction to a DrasiLib instance

Build the reaction with `HttpReactionBuilder` (see
[Configuration reference](#configuration-reference)), then attach it to a `DrasiLib`
instance one of two ways.

**At build time** — register it before the instance starts:

```rust
let reaction = HttpReactionBuilder::new("orders-webhook")
    .with_query("orders-by-region")
    .with_base_url("https://example.com/api")
    .build()?;

let core = DrasiLib::builder()
    .with_id("my-app")
    .with_source(source)
    .with_query(query)
    .with_reaction(reaction)
    .build()
    .await?;
core.start().await?;
```

**At runtime** — add it to an already-built, running instance (it auto-starts unless
`with_auto_start(false)` was set):

```rust
let reaction = HttpReactionBuilder::new("orders-webhook")
    .with_query("orders-by-region")
    .with_base_url("https://example.com/api")
    .build()?;
core.add_reaction(reaction).await?;
```

The reaction subscribes to the query IDs passed to `with_query(...)` / `with_queries(...)`;
the runtime forwards each matching `QueryResult` to the reaction. You never subscribe to
queries yourself.

---

## Configuration reference

`HttpReactionConfig` is the struct populated by `HttpReactionBuilder`. All fields are
optional unless noted; defaults are applied by the builder.

> The `Field` column below uses the **Rust** struct/builder field names (snake_case). In
> **declarative** (JSON/YAML) plugin config these same fields are **camelCase**
> (`baseUrl`, `timeoutMs`, `outputTemplates`, `batchEndpoint`, plus the declarative-only
> `priorityQueueCapacity`) — see
> [Declarative (dynamic-plugin) configuration](#declarative-dynamic-plugin-configuration).

| Field | Type | Default | Builder setter | Description |
|-------|------|---------|----------------|-------------|
| `base_url` | `String` | `http://localhost` | `with_base_url` | Base URL prepended to every relative per-call URL. |
| `token` | `Option<String>` | `None` | `with_token` | Bearer token sent as `Authorization: Bearer <token>` on every request. |
| `timeout_ms` | `u64` | `5000` | `with_timeout_ms` | Per-request HTTP timeout in milliseconds. |
| `output_templates` | `Option<HttpOutputTemplates>` | `None` | `with_default_template`, `with_query_template`, `with_output_templates` | Per-query and default templates. In single-notification mode the URL, body, and headers are templated; in adaptive mode only the per-query **body** template applies to each batched item. See [Output templates](#output-templates-and-per-query-routing). |
| `adaptive` | `Option<AdaptiveBatchConfig>` | `None` | `with_adaptive` | When `Some`, enables [adaptive batching](#adaptive-batching) and requires `batch_endpoint`. When `None`, the reaction delivers one HTTP request per result. |
| `batch_endpoint` | `Option<String>` | `None` | `with_batch_endpoint` | Path appended to `base_url`. Required with `adaptive`; every coalesced batch is POSTed to `{base_url}{batch_endpoint}` as a single payload. Invalid combinations fail at `build()` time. |
| `recovery_policy` | `Option<ReactionRecoveryPolicy>` | `None` (defaults to `Strict`) | `with_recovery_policy` | Behavior on a **sustained** delivery failure. `strict` (default) fail-stops the reaction (`Error`) without advancing the checkpoint, so the un-acked work replays from the query outbox on restart; `auto_skip_gap` skips the failed batch and keeps running. In declarative config the key is `recoveryPolicy` (`strict` \| `auto_skip_gap`). |

Builder-only settings (not stored on `HttpReactionConfig`):

| Setter | Default | Description |
|--------|---------|-------------|
| `with_queries(Vec<String>)` | _empty_ | Replace the subscribed-query list. |
| `with_query(...)` / `from_query(...)` | _empty_ | Append one subscribed query id (`from_query` is an alias that reads naturally at call sites). |
| `with_priority_queue_capacity(usize)` | `10000` | Capacity of the inbound queue that buffers query results before processing. Tune for high-throughput sources. |
| `with_auto_start(bool)` | `true` | Whether the reaction starts automatically when `DrasiLib` is running. |
| `with_recovery_policy(ReactionRecoveryPolicy)` | `Strict` | Recovery policy for this reaction instance. The HTTP reaction maintains a per-query checkpoint advanced on successful (acked) delivery for at-least-once semantics. Under `Strict` (default) a sustained delivery failure fail-stops the reaction so the un-acked work replays from the query outbox on restart; under `AutoSkipGap` the failed batch is skipped and the reaction keeps running. Auth/permission rejections (401/403/407) are treated as sustained failures (subject to the policy), **not** dropped — an expired or rotated credential cannot silently lose events. Only genuinely permanent failures (other 4xx such as 400/404/405/422, SSRF-blocked/unresolvable URLs, invalid method/auth-token config) are dropped and never fail-stop. |
| `with_config(HttpReactionConfig)` | — | Replace the entire runtime config in one call. |
| `with_adaptive_defaults()` | — | Convenience for `with_adaptive(AdaptiveBatchConfig::default())`; still requires `with_batch_endpoint(...)`. |

### `HttpOutputTemplates`

| Field | Type | Description |
|-------|------|-------------|
| `default_template` | `Option<HttpQueryConfig>` | Applied to any query without a matching `routes` entry. |
| `routes` | `HashMap<String, HttpQueryConfig>` | Per-query overrides keyed by query id. |

### `HttpQueryConfig`

| Field | Type | Description |
|-------|------|-------------|
| `added` | `Option<HttpCallSpec>` | Template used when a result row is added. |
| `updated` | `Option<HttpCallSpec>` | Template used when a result row is updated. |
| `deleted` | `Option<HttpCallSpec>` | Template used when a result row is deleted. |

### `HttpCallSpec`

`HttpCallSpec = TemplateSpec<HttpCallExt>` — a Handlebars body `template` plus an
`HttpCallExt { url, method, headers }`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `template` | `String` | _empty_ | Handlebars template for the request body. When empty, the standard [`DefaultChangeNotification`](#defaultchangenotification-envelope-single-notification-path) envelope is sent. |
| `extension.url` | `String` | _empty_ | Handlebars template for the URL. A relative value is appended to `base_url`; an absolute `http(s)://` value is allowed only if its scheme, host, and port match `base_url`. |
| `extension.method` | `String` | `POST` | HTTP method: `GET`, `POST`, `PUT`, `PATCH`, or `DELETE` (case-insensitive). |
| `extension.headers` | `HashMap<String, String>` | _empty_ | Additional headers. Values support Handlebars templates. |

### `AdaptiveBatchConfig`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `adaptive_min_batch_size` | `usize` | `1` | Lower bound on batch size, used during idle/low traffic. |
| `adaptive_max_batch_size` | `usize` | `100` | Upper bound on batch size, used during bursts. |
| `adaptive_window_size` | `usize` | `10` | Throughput sample window in 100&nbsp;ms units (`10` = 1&nbsp;s, `50` = 5&nbsp;s, `100` = 10&nbsp;s). Range 1–255. |
| `adaptive_batch_timeout_ms` | `u64` | `1000` | Maximum time to wait before flushing a partial batch. |

### Declarative (dynamic-plugin) configuration

When the reaction is loaded as a [dynamic plugin](#plugin-metadata) (kind `http`, config
schema `reaction.http.HttpReactionConfig`), it is configured with JSON/YAML instead of the
Rust builder. Field names are **camelCase** and map one-to-one to the builder fields.
Connection fields accept [`ConfigValue`](#authentication) forms: a literal value, an
environment-variable reference (`"${VAR}"` or `{ "kind": "EnvironmentVariable", "name": "VAR", "default": "…" }`),
or a secret reference (`{ "kind": "Secret", "name": "…" }`). Unknown fields are rejected.

The subscribed query IDs are **not** part of this config schema — the host supplies them
when it creates the reaction (the equivalent of `with_queries(...)`).

| JSON field | Type | Builder equivalent | Notes |
|---|---|---|---|
| `baseUrl` | string \| ConfigValue | `with_base_url` | Default `http://localhost`. |
| `token` | string \| ConfigValue | `with_token` | Bearer token. |
| `timeoutMs` | number \| ConfigValue | `with_timeout_ms` | Default `5000`. |
| `priorityQueueCapacity` | number \| ConfigValue | `with_priority_queue_capacity` | Inbound queue capacity (default `10000`). Declarative-only — a `ReactionBase` parameter, not a field of `HttpReactionConfig`. |
| `outputTemplates` | object | `with_output_templates` | `defaultTemplate` + `routes`; each entry has `added` / `updated` / `deleted`, each with `template` / `url` / `method` / `headers`. |
| `adaptive` | object | `with_adaptive` | `adaptiveMinBatchSize`, `adaptiveMaxBatchSize`, `adaptiveWindowSize`, `adaptiveBatchTimeoutMs`. |
| `batchEndpoint` | string \| ConfigValue | `with_batch_endpoint` | Required whenever `adaptive` is set. |

Single-notification example (per-query REST routing, token from an environment variable):

```json
{
  "baseUrl": "https://example.com/api",
  "token": "${ORDERS_API_TOKEN}",
  "timeoutMs": 5000,
  "outputTemplates": {
    "routes": {
      "orders-by-region": {
        "added":   { "template": "{{json after}}", "url": "/orders", "method": "POST" },
        "updated": { "template": "{{json after}}", "url": "/orders/{{after.id}}", "method": "PUT" },
        "deleted": { "url": "/orders/{{before.id}}", "method": "DELETE" }
      }
    }
  }
}
```

Adaptive (batched) example:

```json
{
  "baseUrl": "https://example.com/api",
  "adaptive": {
    "adaptiveMinBatchSize": 50,
    "adaptiveMaxBatchSize": 2000,
    "adaptiveWindowSize": 100,
    "adaptiveBatchTimeoutMs": 500
  },
  "batchEndpoint": "/events/batch"
}
```

---

## Output templates and per-query routing

`output_templates` lets each query customise how its result changes are delivered. For each
result, the reaction selects a template in this order:

1. `output_templates.routes[<query_id>]` — a per-query override keyed by the full query id.
2. `output_templates.routes[<last dotted segment>]` — for a query id such as `source.orders`,
   `routes.orders` is accepted when no full-id route matches.
3. `output_templates.default_template` — a shared fallback, if present.
4. A built-in default: `POST {base_url}/changes/{query_id}` with the standard
   [`DefaultChangeNotification`](#defaultchangenotification-envelope-single-notification-path)
   envelope as the body (used when neither of the above matches the operation).

Within the selected `HttpQueryConfig`, the `added` / `updated` / `deleted` spec matching
the result's operation is used. If that specific operation has no spec, the built-in
default applies for that operation.

```rust
use std::collections::HashMap;
use drasi_reaction_http::{HttpCallExt, HttpQueryConfig, TemplateSpec};

let orders = HttpQueryConfig {
    added: Some(TemplateSpec {
        template: r#"{"event":"created","order":{{json after}}}"#.to_string(),
        extension: HttpCallExt {
            url: "/orders".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    }),
    deleted: Some(TemplateSpec {
        template: r#"{"event":"cancelled","orderId":"{{before.id}}"}"#.to_string(),
        extension: HttpCallExt {
            url: "/orders/{{before.id}}".to_string(),
            method: "DELETE".to_string(),
            headers: HashMap::new(),
        },
    }),
    ..Default::default()
};
```

Route keys are validated at build time against the subscribed query ids and their last
dotted segments.

---

## Templating

Templates use [Handlebars](https://handlebarsjs.com/) syntax and apply to the body
`template`, the `url`, and header values. HTTP methods are static and validated at build
time. The render context depends on the operation:

| Variable | Available for | Meaning |
|----------|---------------|---------|
| `after` | added, updated | The new / current row payload. |
| `before` | updated, deleted | The previous row payload. |
| `data` | updated | The raw `data` payload of the update diff. |
| `query_name` | all | The query identifier. |
| `query_id` | all | Alias of `query_name`. |
| `operation` | all | The operation: `ADD`, `UPDATE`, or `DELETE`. |
| `timestamp` | all | RFC 3339 timestamp of the query emission. |
| `metadata` | all | The query result's metadata map (an object; may be empty). |

A `json` helper is registered for embedding a value as JSON:

```handlebars
{
  "query": "{{query_name}}",
  "operation": "{{operation}}",
  "row": {{json after}}
}
```

---

## Output payload schema

The reaction emits two JSON shapes: a per-change `DefaultChangeNotification` (single-notification
mode) and a `BatchEnvelope` (adaptive mode). **To change the structure of what is sent**,
configure [output templates](#output-templates-and-per-query-routing): when no body template
applies the reaction emits the default envelopes documented below, and a non-empty body
`template` replaces the body with your own rendered JSON (per query and per operation). The
`url`, `method`, and `headers` are likewise template-controlled in single-notification mode.

Both shapes are formally defined as JSON Schema and committed at
[`schema/output.schema.json`](./schema/output.schema.json). The file is regenerated from the
Rust types in `src/output.rs` by `make update-schema`, and a CI test (`make schema`) fails
if the file drifts from the types. Use it to generate clients or validate receiver behaviour.

### `DefaultChangeNotification` envelope (single-notification path)

In single-notification mode, each result is delivered as a `DefaultChangeNotification`
JSON object:

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `operation` | `string` enum: `"ADD"` &#124; `"UPDATE"` &#124; `"DELETE"` | yes | What kind of change this notification represents. |
| `queryId` | `string` | yes | The continuous query that produced the change. |
| `sequenceId` | `number` | yes | Monotonic per-query sequence number identifying this emission. |
| `timestamp` | `string` (RFC 3339) | yes | UTC timestamp of the originating query emission. |
| `before` | object | no — omitted for `ADD` and for the first emission of an aggregation group | Row state before the change. |
| `after` | object | no — omitted for `DELETE` | Row state after the change. |
| `metadata` | object | no — omitted when empty | Source/query metadata carried by the originating result. |

Mapping from the engine's `ResultDiff` variants:

| `ResultDiff` variant | `operation` | `before` | `after` | HTTP request? |
|---|---|---|---|---|
| `Add { data }` | `"ADD"` | _omitted_ | `data` | yes |
| `Delete { data }` | `"DELETE"` | `data` | _omitted_ | yes |
| `Update { before, after }` | `"UPDATE"` | `before` | `after` | yes |
| `Aggregation { before, after }` | `"UPDATE"` | `before` (omitted on first emission) | `after` | yes |
| `Noop` | — | — | — | **no** (silently skipped) |

Aggregation results are delivered using the `UPDATE` operation, matching the gRPC, Azure
Storage, and RabbitMQ reactions. `Noop` results are dropped without emitting any request.

#### Wire examples

```http
POST /changes/hot-sensors HTTP/1.1
Content-Type: application/json

{"operation":"ADD","queryId":"hot-sensors","sequenceId":42,"timestamp":"2026-01-01T00:00:00+00:00","after":{"sensor_id":"s1","temperature":27.5}}
```

```http
POST /changes/hot-sensors HTTP/1.1
Content-Type: application/json

{"operation":"DELETE","queryId":"hot-sensors","sequenceId":43,"timestamp":"2026-01-01T00:00:00+00:00","before":{"sensor_id":"s1","temperature":27.5}}
```

```http
POST /changes/hot-sensors HTTP/1.1
Content-Type: application/json

{"operation":"UPDATE","queryId":"hot-sensors","sequenceId":44,"timestamp":"2026-01-01T00:00:00+00:00","before":{"sensor_id":"s1","temperature":25.0},"after":{"sensor_id":"s1","temperature":27.5}}
```

```http
POST /changes/hot-sensors HTTP/1.1
Content-Type: application/json

{"operation":"UPDATE","queryId":"hot-sensors","sequenceId":45,"timestamp":"2026-01-01T00:00:00+00:00","after":{"region":"north","count":1}}
```
*(Aggregation, first emission of a grouping key — note the omitted `before`.)*

#### Delivery details

- **URL** — the rendered template `url` appended to `base_url`, or `{base_url}/changes/{queryId}`
  when no per-query template applies.
- **Method** — the spec's `method` (default `POST`).
- **Headers** — `Content-Type: application/json`, the optional bearer token, and any
  rendered custom headers.
- **Body** — the rendered `template`. When the template is empty (including the synthesized
  default), the body is the `DefaultChangeNotification` envelope shown above.

### `BatchEnvelope` (batch-endpoint path)

In adaptive mode **with** `batch_endpoint` set, each coalesced batch is POSTed to
`{base_url}{batchEndpoint}` as a single JSON object (the canonical Pattern C container)
whose `batch` field is an array of items. Each item is either a rendered per-query **body**
template or — when no body template applies — the default `DefaultChangeNotification`
envelope (carrying its own `queryId`, `sequenceId`, and `timestamp`), so a single batch may
interleave items from several queries:

| Field | Type | Description |
|-------|------|-------------|
| `batch` | array of items | The coalesced items, in order: rendered body templates or `DefaultChangeNotification` envelopes. Always an array, even for a single item. |

```json
{
  "batch": [
    {"operation":"ADD","queryId":"orders-by-region","sequenceId":100,"timestamp":"…","after":{"id":1}},
    {"operation":"UPDATE","queryId":"orders-by-region","sequenceId":101,"timestamp":"…","before":{"id":2},"after":{"id":2}}
  ]
}
```

The batch endpoint must accept `POST`.

---

## Adaptive batching

Adding an `AdaptiveBatchConfig` switches the reaction from single-notification delivery to
an internal batcher that coalesces result changes and scales batch size based on observed
throughput. The batcher:

1. Pulls query results off the internal queue.
2. Renders each change to a batch item — a rendered per-query **body** template, or the
   default `DefaultChangeNotification` envelope when no body template applies — and groups
   items into batches whose size adapts between `adaptive_min_batch_size` and
   `adaptive_max_batch_size` according to recent throughput, flushing a partial batch
   after `adaptive_batch_timeout_ms`.
3. Delivers each batch to `batch_endpoint` as one request.

Use adaptive batching when your receiver can accept many changes in one request and the
per-request overhead is the bottleneck. Per-query **body** templates still apply to each
batched item; if each change must go to its own templated URL/method/headers, use
single-notification mode instead.

---

## Render-error fallback

If a per-result template fails to render — for example because it references a field that
is missing from the payload — the reaction handles the failure by template kind:

1. URL render failure: logs a warning and uses the default `/changes/{queryId}` URL.
2. Body render failure: logs a warning and sends the standard
   [`DefaultChangeNotification`](#defaultchangenotification-envelope-single-notification-path)
   envelope as the body on the configured route.
3. Header render failure or invalid rendered header value: logs a warning and drops only
   that header.

This guarantees changes are never silently dropped because of a template error. The
behaviour is always on and cannot be disabled.

---

## Delivery failure handling

The HTTP reaction is fire-and-forget: it does not persist delivery checkpoints or replay
requests after process failure. Each outbound request is attempted up to three times for
transient failures (network errors, 408, 409, 425, 429, and 5xx responses) with short
exponential backoff. Non-retryable HTTP failures are logged as permanent failures and the
reaction continues processing later events.

---

## Authentication

Call `with_token(...)` to send a bearer token on every request:

```rust
let reaction = HttpReactionBuilder::new("my-http-reaction")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_base_url("https://example.com/api")
    .with_token(std::env::var("MY_API_TOKEN")?)
    .build()?;
```

Embedded builder callers supply the resolved token value (for example from `std::env` or
their application's secret manager). Dynamic-plugin config accepts `ConfigValue` forms for
connection fields, including environment-variable and secret references, and resolves them
through the plugin SDK's configured resolver.

---

## Examples

### 1. Single notifications to the built-in default endpoint

Each result POSTed as a [`DefaultChangeNotification`](#defaultchangenotification-envelope-single-notification-path)
envelope to `https://example.com/api/changes/<query_id>`:

```rust
let reaction = HttpReactionBuilder::new("orders-webhook")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_base_url("https://example.com/api")
    .build()?;
core.add_reaction(reaction).await?;
```

### 2. Single notifications with a REST-style per-query template

Map each operation to a different verb and path:

```rust
use std::collections::HashMap;
use drasi_reaction_http::{HttpCallExt, HttpQueryConfig, HttpReactionBuilder, TemplateSpec};

let orders = HttpQueryConfig {
    added: Some(TemplateSpec {
        template: r#"{{json after}}"#.to_string(),
        extension: HttpCallExt {
            url: "/orders".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    }),
    updated: Some(TemplateSpec {
        template: r#"{{json after}}"#.to_string(),
        extension: HttpCallExt {
            url: "/orders/{{after.id}}".to_string(),
            method: "PUT".to_string(),
            headers: HashMap::new(),
        },
    }),
    deleted: Some(TemplateSpec {
        template: String::new(),
        extension: HttpCallExt {
            url: "/orders/{{before.id}}".to_string(),
            method: "DELETE".to_string(),
            headers: HashMap::new(),
        },
    }),
};

let reaction = HttpReactionBuilder::new("orders-rest")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_base_url("https://example.com/api")
    .with_query_template("orders-by-region", orders)
    .build()?;
```

### 3. Adaptive batching, coalesced to one batch endpoint (max throughput)

```rust
use drasi_reaction_http::{AdaptiveBatchConfig, HttpReactionBuilder};

let reaction = HttpReactionBuilder::new("orders-batched")
    .with_queries(vec!["orders-by-region".to_string(), "shipments".to_string()])
    .with_base_url("https://example.com/api")
    .with_adaptive(AdaptiveBatchConfig {
        adaptive_min_batch_size: 50,
        adaptive_max_batch_size: 2_000,
        adaptive_window_size: 100,       // 10 s throughput window
        adaptive_batch_timeout_ms: 500,
    })
    .with_batch_endpoint("/events/batch")
    .build()?;
```

### 4. Authenticated requests with custom headers

```rust
use std::collections::HashMap;
use drasi_reaction_http::{HttpCallExt, HttpQueryConfig, HttpReactionBuilder, TemplateSpec};

let mut headers = HashMap::new();
headers.insert("X-Query".to_string(), "{{query_name}}".to_string());
headers.insert("X-Operation".to_string(), "{{operation}}".to_string());

let default_tmpl = HttpQueryConfig {
    added: Some(TemplateSpec {
        template: r#"{{json after}}"#.to_string(),
        extension: HttpCallExt {
            url: "/events".to_string(),
            method: "POST".to_string(),
            headers,
        },
    }),
    ..Default::default()
};

let reaction = HttpReactionBuilder::new("orders-authed")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_base_url("https://example.com/api")
    .with_token(std::env::var("MY_API_TOKEN")?)
    .with_default_template(default_tmpl)
    .build()?;
```

---

## Plugin metadata

The reaction also ships as a Drasi dynamic plugin. The plugin descriptor publishes named
OpenAPI sub-schemas for the input config (`HttpReactionConfig`, `HttpOutputTemplates`,
`HttpQueryConfig`, `HttpCallSpec`, and `AdaptiveBatchConfig`) plus `SchemaUiAnnotator`
groupings so management UIs can render the configuration form. The wire-format **output**
schemas (`DefaultChangeNotification`, `Operation`, `BatchEnvelope`) are published as a
standalone JSON Schema file at [`schema/output.schema.json`](./schema/output.schema.json).

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

> **Packaging note:** `Cargo.toml` currently uses workspace-inherited Drasi dependencies
> for convenience while this reaction lives inside the `drasi-core` repository. That is
> intentionally left uncompliant with the standalone reaction-plugin packaging guidance
> for now. Before publishing or building this reaction outside the workspace, update it
> to depend on pinned crates.io versions of `drasi-lib`, `drasi-plugin-sdk`, and related
> Drasi crates. Tracked by
> [drasi-project/drasi-core#574](https://github.com/drasi-project/drasi-core/issues/574).

---

## Testing

```bash
cargo test -p drasi-reaction-http
```

Unit tests cover config defaults and validation, builder and full `Reaction`-trait
semantics, template routing and rendering, lifecycle, and adaptive runtime conversion.
Integration tests use [`wiremock`](https://crates.io/crates/wiremock) as the receiving
endpoint to exercise, among others:

- Single-notification delivery: default fallback, per-query templates (URL / method /
  headers / body), and the per-operation render-error fallbacks.
- Bearer-token injection and the SSRF guard for absolute URLs.
- Delivery failure handling: transient-retry-then-success, retry exhaustion, auth-rejection
  fail-stop (401/403/407), and permanent-4xx drop-and-continue.
- Adaptive mode: coalesced batch POSTs, per-item body templating, partial-batch flush, and
  the max-batch-size cap.
- `DrasiLib` end-to-end wiring (source → query → reaction) for both single and batched
  delivery.

---

## Changelog

See [CHANGELOG.md](./CHANGELOG.md).
