# Drasi HTTP Reaction

`drasi-reaction-http` is the HTTP reaction for the [Drasi](https://drasi.io) embedded
runtime ([`drasi-lib`](../../../lib)). It forwards the result changes produced by Drasi
continuous queries to HTTP endpoints.

Changes can be distributed two ways:

- **Single notifications** (default) — each added / updated / deleted result row is delivered
  as its own HTTP request.
- **Adaptive batching** (optional) — result changes are coalesced into batches whose size
  scales automatically with incoming load, trading a small amount of latency for higher
  throughput and reduced network traffic. Enabled by adding an `AdaptiveBatchConfig`.

Both modes support [Handlebars](https://handlebarsjs.com/) templating of the request URL,
method, body, and headers, plus per-query routing and bearer-token authentication.

The reaction is built in Rust with `HttpReactionBuilder` and added to a running `DrasiLib`
via `add_reaction(...)`. See [Examples](#examples).

---

## Table of contents

1. [Quick start](#quick-start)
2. [Configuration reference](#configuration-reference)
3. [Output templates and per-query routing](#output-templates-and-per-query-routing)
4. [Templating](#templating)
5. [Output payload schema](#output-payload-schema)
   - [Single-notification payloads](#single-notification-payloads)
   - [Batch payloads](#batch-payloads)
6. [Adaptive batching](#adaptive-batching)
7. [Render-error fallback](#render-error-fallback)
8. [Authentication](#authentication)
9. [Examples](#examples)
10. [Plugin metadata](#plugin-metadata)
11. [Testing](#testing)
12. [Changelog](#changelog)

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

## Configuration reference

`HttpReactionConfig` is the struct populated by `HttpReactionBuilder`. All fields are
optional unless noted; defaults are applied by the builder.

| Field | Type | Default | Builder setter | Description |
|-------|------|---------|----------------|-------------|
| `base_url` | `String` | `http://localhost` | `with_base_url` | Base URL prepended to every relative per-call URL. |
| `token` | `Option<String>` | `None` | `with_token` | Bearer token sent as `Authorization: Bearer <token>` on every request. |
| `timeout_ms` | `u64` | `5000` | `with_timeout_ms` | Per-request HTTP timeout in milliseconds. |
| `output_templates` | `Option<HttpOutputTemplates>` | `None` | `with_default_template`, `with_query_template`, `with_output_templates` | Per-query and default templates for the URL, method, body, and headers. See [Output templates](#output-templates-and-per-query-routing). |
| `adaptive` | `Option<AdaptiveBatchConfig>` | `None` | `with_adaptive` (or `with_min_batch_size` / `with_max_batch_size` / `with_window_size` / `with_batch_timeout_ms`) | When `Some`, enables [adaptive batching](#adaptive-batching). When `None`, the reaction delivers one HTTP request per result. |
| `batch_endpoint` | `Option<String>` | `None` | `with_batch_endpoint` | Path appended to `base_url`. When set, every coalesced batch is POSTed to `{base_url}{batch_endpoint}` as a single payload. **Requires `adaptive`** — if set without `adaptive`, the reaction fails to start (logs an error, sets status to `Stopped`, and returns an error). |

Builder-only settings (not stored on `HttpReactionConfig`):

| Setter | Default | Description |
|--------|---------|-------------|
| `with_queries(Vec<String>)` / `with_query(...)` | _empty_ | Query IDs this reaction subscribes to. |
| `with_priority_queue_capacity(usize)` | runtime default | Capacity of the inbound queue that buffers query results before processing. Tune for high-throughput sources. |
| `with_auto_start(bool)` | `true` | Whether the reaction starts automatically when `DrasiLib` is running. |

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
| `template` | `String` | _empty_ | Handlebars template for the request body. When empty, the raw result JSON is sent. |
| `extension.url` | `String` | _empty_ | Handlebars template for the URL. A relative value is appended to `base_url`; an absolute `http(s)://` value is allowed only if its host matches `base_url`'s host. |
| `extension.method` | `String` | `POST` | HTTP method: `GET`, `POST`, `PUT`, `PATCH`, or `DELETE` (case-insensitive). |
| `extension.headers` | `HashMap<String, String>` | _empty_ | Additional headers. Values support Handlebars templates. |

### `AdaptiveBatchConfig`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `adaptive_min_batch_size` | `usize` | `1` | Lower bound on batch size, used during idle/low traffic. |
| `adaptive_max_batch_size` | `usize` | `100` | Upper bound on batch size, used during bursts. |
| `adaptive_window_size` | `usize` | `10` | Throughput sample window in 100&nbsp;ms units (`10` = 1&nbsp;s, `50` = 5&nbsp;s, `100` = 10&nbsp;s). Range 1–255. |
| `adaptive_batch_timeout_ms` | `u64` | `1000` | Maximum time to wait before flushing a partial batch. |

---

## Output templates and per-query routing

`output_templates` lets each query customise how its result changes are delivered. For each
result, the reaction selects a template in this order:

1. `output_templates.routes[<query_id>]` — a per-query override, if present.
2. `output_templates.default_template` — a shared fallback, if present.
3. A built-in default: `POST {base_url}/changes/{query_id}` with the raw result JSON as the
   body (used when neither of the above matches the operation).

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

Route keys are matched against the full query id.

---

## Templating

Templates use [Handlebars](https://handlebarsjs.com/) syntax and apply to the body
`template`, the `url`, and header values. The render context depends on the operation:

| Variable | Available for | Meaning |
|----------|---------------|---------|
| `after` | added, updated | The new / current row payload. |
| `before` | updated, deleted | The previous row payload. |
| `data` | updated, and non-row results | The raw diff payload. |
| `query_name` | all | The query identifier. |
| `operation` | all | The operation: `ADD`, `UPDATE`, or `DELETE`. |

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

### Single-notification payloads

In single-notification mode (and in adaptive mode without a `batch_endpoint`), each result
is delivered by rendering its selected [`HttpCallSpec`](#httpcallspec):

- **Body** — the rendered `template`. When `template` is empty, the reaction sends raw JSON:
  - For **added** and **deleted** results, the body is the row object (the result's `data`).
  - For **updated** results, the body is the full result-diff object (see below).
- **URL** — the rendered `url` appended to `base_url`, or `{base_url}/changes/{query_id}`
  when no template applies.
- **Method** — the spec's `method` (default `POST`).
- **Headers** — `Content-Type: application/json`, the optional bearer token, and any
  rendered custom headers.

The raw result-diff object has a `type` discriminator:

```json
{ "type": "ADD",    "data": { }, "row_signature": 0 }
{ "type": "DELETE", "data": { }, "row_signature": 0 }
{ "type": "UPDATE", "data": { }, "before": { }, "after": { }, "row_signature": 0 }
```

### Batch payloads

In adaptive mode **with** `batch_endpoint` set, each coalesced batch is POSTed to
`{base_url}{batch_endpoint}` as a single JSON array. Each array element groups the results
of one query in that batch:

```json
[
  {
    "query_id": "orders-by-region",
    "results": [
      { "type": "ADD",    "data": { }, "row_signature": 0 },
      { "type": "UPDATE", "data": { }, "before": { }, "after": { }, "row_signature": 0 }
    ],
    "timestamp": "2026-01-01T00:00:00+00:00",
    "count": 2
  }
]
```

| Field | Type | Description |
|-------|------|-------------|
| `query_id` | `string` | The query whose results are in this group. |
| `results` | `array` | The coalesced result diffs (same shape as the raw diff above). |
| `timestamp` | `string` | RFC 3339 timestamp of when the batch was assembled. |
| `count` | `number` | Number of results in `results`. |

The batch endpoint must accept `POST`.

---

## Adaptive batching

Adding an `AdaptiveBatchConfig` switches the reaction from single-notification delivery to
an internal batcher that coalesces result changes and scales batch size based on observed
throughput. The batcher:

1. Pulls query results off the internal queue.
2. Groups them into batches whose size adapts between `adaptive_min_batch_size` and
   `adaptive_max_batch_size` according to recent throughput, flushing a partial batch
   after `adaptive_batch_timeout_ms`.
3. Delivers each batch either:
   - **coalesced** to `batch_endpoint` as one request (when `batch_endpoint` is set), or
   - **fanned out** per result using the same [templating and routing](#output-templates-and-per-query-routing)
     rules as single-notification mode (when `batch_endpoint` is not set).

Use a coalesced `batch_endpoint` when your receiver can accept many changes in one
request — this is the configuration that most reduces network traffic. Use adaptive without
a `batch_endpoint` when you want the batcher's throughput smoothing but still need each
change delivered to its own templated endpoint.

---

## Render-error fallback

If a per-result template (URL, body, or header) fails to render — for example because it
references a field that is missing from the payload — the reaction:

1. Logs a warning with the query id and the render error.
2. POSTs the raw result JSON to `{base_url}/changes/{query_id}` instead.

This guarantees changes are never silently dropped because of a template error. The
behaviour is always on and cannot be disabled.

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

The reaction does not resolve secret-store references itself; supply the resolved token
value (e.g. from `std::env`, a Drasi state store, or your application's secret manager).

---

## Examples

### 1. Single notifications to the built-in default endpoint

Each result POSTed as raw JSON to `https://example.com/api/changes/<query_id>`:

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

### 4. Adaptive batching, fanned out per result (no batch endpoint)

Throughput smoothing while still delivering each change to its templated endpoint:

```rust
use std::collections::HashMap;
use drasi_reaction_http::{
    AdaptiveBatchConfig, HttpCallExt, HttpQueryConfig, HttpReactionBuilder, TemplateSpec,
};

let default_tmpl = HttpQueryConfig {
    added: Some(TemplateSpec {
        template: r#"{{json after}}"#.to_string(),
        extension: HttpCallExt {
            url: "/events".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    }),
    ..Default::default()
};

let reaction = HttpReactionBuilder::new("orders-smoothed")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_base_url("https://example.com/api")
    .with_default_template(default_tmpl)
    .with_adaptive(AdaptiveBatchConfig {
        adaptive_min_batch_size: 1,
        adaptive_max_batch_size: 200,
        adaptive_window_size: 30,        // 3 s throughput window
        adaptive_batch_timeout_ms: 50,
    })
    .build()?;
```

### 5. Authenticated requests with custom headers

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
OpenAPI sub-schemas for `HttpReactionConfig`, `HttpOutputTemplates`, `HttpQueryConfig`,
`HttpCallSpec`, and `AdaptiveBatchConfig`, plus `SchemaUiAnnotator` groupings so management
UIs can render the configuration form.

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

---

## Testing

```bash
cargo test -p drasi-reaction-http
```

Unit tests cover config defaults, builder semantics, template routing, lifecycle, and
runtime conversion. Integration tests use [`wiremock`](https://crates.io/crates/wiremock) to
exercise:

- Single-notification delivery with a per-query template.
- Bearer-token header injection.
- Render-error fallback to `/changes/{query_id}`.
- Default fallback when no template matches.
- Adaptive mode with `batch_endpoint` set — coalesced batch POST.
- Adaptive mode without `batch_endpoint` — per-result fan-out.

---

## Changelog

See [CHANGELOG.md](./CHANGELOG.md).
