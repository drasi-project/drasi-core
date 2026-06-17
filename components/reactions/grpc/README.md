# Drasi gRPC Reaction

`drasi-reaction-grpc` is the gRPC reaction for the [Drasi](https://drasi.io) embedded
runtime ([`drasi-lib`](../../../lib)). It streams the result changes produced by Drasi
continuous queries to a downstream gRPC service via the
`drasi.v1.ReactionService.ProcessResults` RPC defined in
[`proto/drasi/v1/reaction.proto`](proto/drasi/v1/reaction.proto). The proto file is
intentionally narrow — it carries only the reaction's wire contract; no source,
bootstrap, streaming, or subscription types.

Changes can be delivered two ways:

- **Fixed batching** (default) — items accumulate until either `batchSize` items have
  been collected or `batchFlushTimeoutMs` elapses (whichever comes first).
- **Adaptive batching** (optional) — batch size scales automatically between
  `adaptiveMinBatchSize` and `adaptiveMaxBatchSize` based on observed throughput,
  trading a small amount of latency for higher throughput on bursty workloads.
  Enabled by configuring `batching.mode = "adaptive"`.

Both modes support [Handlebars](https://handlebarsjs.com/) output templates that
reshape the row content emitted in the `before` / `after` fields of every result
item. When no template is configured (or a template fails to render) the raw row
state is emitted on the wire so receivers can always recover the change.

The reaction is built in Rust with `GrpcReaction::builder()` and added to a running
`DrasiLib` via `add_reaction(...)`. See [Examples](#examples).

---

## Table of contents

1. [Quick start](#quick-start)
2. [Configuration reference](#configuration-reference)
3. [Output templates and per-query routing](#output-templates-and-per-query-routing)
4. [Templating](#templating)
5. [Output schema](#output-schema)
6. [Adaptive batching](#adaptive-batching)
7. [Render-error fallback](#render-error-fallback)
8. [Authentication](#authentication)
9. [Examples](#examples)
10. [Plugin metadata](#plugin-metadata)
11. [Testing](#testing)
12. [Changelog](#changelog)

---

## Quick start

Here is a complete example of how to use the gRPC Reaction. It uses a `MockSource` to
generate simulated sensor data, a Cypher continuous query that surfaces hot sensors,
and the gRPC reaction that streams each result change to a downstream collector
listening on `grpc://localhost:50052`.

Add the crates to your `Cargo.toml`:

```toml
[dependencies]
drasi-lib            = { path = "../drasi-core/lib" }
drasi-source-mock    = { path = "../drasi-core/components/sources/mock" }
drasi-reaction-grpc  = { path = "../drasi-core/components/reactions/grpc" }

anyhow      = "1"
tokio       = { version = "1", features = ["macros", "rt-multi-thread", "signal"] }
env_logger  = "0.11"
```

Rust code for the example:

```rust,ignore
use std::sync::Arc;

use anyhow::Result;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_grpc::GrpcReaction;
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

    // 3. Reaction: stream each result change as a `ProcessResults` RPC to a
    //    downstream gRPC collector on grpc://localhost:50052.
    let reaction = GrpcReaction::builder("hot-sensors-grpc")
        .with_queries(vec!["hot-sensors".to_string()])
        .with_endpoint("grpc://localhost:50052")
        .with_timeout_ms(5_000)
        .build()?;

    // 4. Wire everything into a DrasiLib instance and start.
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("grpc-reaction-quickstart")
            .with_source(source)
            .with_query(hot_sensors)
            .with_reaction(reaction)
            .build()
            .await?,
    );
    core.start().await?;

    println!("gRPC reaction running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    core.stop().await?;
    Ok(())
}
```

With no `output_templates` configured, each result change is delivered with raw
`before` / `after` row state; see [Output schema](#output-schema). To switch the
same reaction to adaptive batched delivery, replace the fixed batching with an
adaptive config:

```rust,ignore
use drasi_lib::reactions::common::AdaptiveBatchConfig;
use drasi_reaction_grpc::GrpcReaction;

let reaction = GrpcReaction::builder("hot-sensors-grpc")
    .with_queries(vec!["hot-sensors".to_string()])
    .with_endpoint("grpc://localhost:50052")
    .with_adaptive_batching(AdaptiveBatchConfig {
        adaptive_min_batch_size: 50,
        adaptive_max_batch_size: 2_000,
        adaptive_window_size: 100,        // 10 s throughput window
        adaptive_batch_timeout_ms: 500,
    })
    .build()?;
```

---

## Configuration reference

`GrpcReactionConfig` is the struct populated by `GrpcReactionBuilder`. All keys
serialize and deserialize in `camelCase` when supplied via descriptor configuration
(JSON / YAML); defaults are applied by the builder.

| Field | Type | Default | Builder setter | Description |
|-------|------|---------|----------------|-------------|
| `endpoint` | `String` | `grpc://localhost:50052` | `with_endpoint` | gRPC server endpoint to connect to. |
| `timeoutMs` | `u64` | `5000` | `with_timeout_ms` | Per-RPC timeout in milliseconds. |
| `maxRetries` | `u32` | `3` | `with_max_retries` | Maximum retry attempts per batch on transient send errors. Batches that exhaust retries are logged and dropped; the client reconnects before the next batch. |
| `connectionRetryAttempts` | `u32` | `5` | `with_connection_retry_attempts` | Maximum attempts when (re)establishing the channel after a connection-level error. |
| `initialConnectionTimeoutMs` | `u64` | `10000` | `with_initial_connection_timeout_ms` | Timeout for each connection attempt. |
| `metadata` | `Map<String,String>` | `{}` | `with_metadata(k, v)` / `with_all_metadata(map)` | Sent on every call **both** as `ProcessResultsRequest.metadata` (a body field on the proto) **and** as actual gRPC request headers (`tonic::Request::metadata_mut()`). ASCII-only; entries with invalid header names/values are skipped from the headers with a warning and still ride in the body field. Via descriptor configuration each value is a `ConfigValue<String>` and may reference an environment variable or secret — see [Authentication](#authentication). |
| `batching` | `BatchingConfig` | `Fixed { batchSize: 100, batchFlushTimeoutMs: 1000 }` | `with_fixed_batching`, `with_adaptive_batching`, or the per-field adaptive setters | Batching strategy — see [Adaptive batching](#adaptive-batching). |
| `outputTemplates` | `Option<OutputTemplates>` | `None` | `with_output_templates` | Optional Handlebars output templates — see [Output templates and per-query routing](#output-templates-and-per-query-routing). When present but containing neither a `defaultTemplate` nor any `routes` entry, a warning is logged at startup (the configuration is a no-op). |

Builder-only settings (not stored on `GrpcReactionConfig`):

| Setter | Default | Description |
|--------|---------|-------------|
| `with_queries(Vec<String>)` / `with_query(...)` | _empty_ | Query IDs this reaction subscribes to. |
| `with_priority_queue_capacity(usize)` | runtime default | Capacity of the inbound queue that buffers query results before processing. Tune for high-throughput sources. |
| `with_auto_start(bool)` | `true` | Whether the reaction starts automatically when `DrasiLib` is running. |

### `BatchingConfig`

Discriminated by `mode`. Fixed (default) and adaptive are mutually exclusive.

| `mode = "fixed"` field | Type | Default | Builder setter | Description |
|-----------|------|---------|----------------|-------------|
| `batchSize` | `usize` | `100` | `with_fixed_batching(size, ms)` | Maximum items in a single `ProcessResults` RPC; the runner flushes when this is reached. |
| `batchFlushTimeoutMs` | `u64` | `1000` | `with_fixed_batching(size, ms)` | Maximum time to hold a partial batch before flushing. |

| `mode = "adaptive"` field | Type | Default | Builder setter | Description |
|-----------|------|---------|----------------|-------------|
| `adaptiveMinBatchSize` | `usize` | `1` | `with_min_batch_size(n)` | Lower bound on batch size, used during idle / low traffic. |
| `adaptiveMaxBatchSize` | `usize` | `100` | `with_max_batch_size(n)` | Upper bound on batch size, used during bursts. |
| `adaptiveWindowSize` | `usize` | `10` | `with_window_size(n)` | Throughput sample window in 100 ms units (`10` = 1 s, `100` = 10 s). |
| `adaptiveBatchTimeoutMs` | `u64` | `1000` | `with_batch_timeout_ms(ms)` | Maximum time the adaptive batcher waits before flushing a partial batch. |

Convenience: `with_adaptive_batching(AdaptiveBatchConfig)` accepts a full struct;
`with_adaptive_defaults()` enables adaptive mode with all defaults. Each per-field
setter (`with_min_batch_size`, `with_max_batch_size`, `with_window_size`,
`with_batch_timeout_ms`) **implicitly enables adaptive mode** if the builder is
currently fixed, populating the remaining adaptive fields from
`AdaptiveBatchConfig::default()`.

### `OutputTemplates`

| Field | Type | Description |
|-------|------|-------------|
| `defaultTemplate` | `Option<QueryConfig>` | Applied to any query without a matching `routes` entry. |
| `routes` | `HashMap<String, QueryConfig>` | Per-query overrides keyed by **exact query id** (no suffix-fallback). |

### `QueryConfig`

| Field | Type | Description |
|-------|------|-------------|
| `added` | `Option<TemplateSpec>` | Template used when a result row is added. |
| `updated` | `Option<TemplateSpec>` | Template used when a result row is updated. |
| `deleted` | `Option<TemplateSpec>` | Template used when a result row is deleted. |

`TemplateSpec` is a Handlebars body `template` string (the gRPC reaction uses
`TemplateSpec<()>` — there is no per-template HTTP-style extension data because gRPC
sends every batch on the same `ProcessResults` RPC).

---

## Output templates and per-query routing

`outputTemplates` lets each query customise the **content** of `before` / `after` on each emitted `QueryResultItem`. For each result, the reaction selects a template in this order:

1. `outputTemplates.routes[<query_id>]` — a per-query override matched by the
   exact wire query id.
2. `outputTemplates.routes[<last dotted segment>]` — so a route keyed `my_query`
   matches a wire id `source.my_query`.
3. `outputTemplates.defaultTemplate` — a shared fallback, if present.
4. **No template** — the item is emitted with the **raw row state** in whichever
   of `before` / `after` is present for the op.

Within the selected `QueryConfig`, the `added` / `updated` / `deleted` spec matching the result's operation is used. Each present row state is rendered through that spec **independently**:

- `ADD` runs the `added` template once on the new row → output goes into `after`.
- `DELETE` runs the `deleted` template once on the old row → output goes into `before`.
- `UPDATE` runs the `updated` template **twice** — once on the before-row → `before`, once on the after-row → `after`.

Operations without a matching spec emit the field's raw row state. Aggregation surfaces as UPDATE (matches RabbitMQ and Azure Storage); Noop is dropped at the runner.

```rust,ignore
use std::collections::HashMap;
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_reaction_grpc::OutputTemplates;

let orders = QueryConfig {
    added: Some(TemplateSpec::new(
        r#"{"event":"created","order":{{json row}}}"#,
    )),
    updated: Some(TemplateSpec::new(
        r#"{"event":"updated","order":{{json row}}}"#,
    )),
    deleted: Some(TemplateSpec::new(
        r#"{"event":"cancelled","orderId":"{{row.id}}"}"#,
    )),
};

let mut routes = HashMap::new();
routes.insert("orders-by-region".to_string(), orders);

let templates = OutputTemplates {
    default_template: None,
    routes,
};
```

---

## Templating

Templates use [Handlebars](https://handlebarsjs.com/) syntax. Each render produces a single JSON object that replaces the raw row state in one proto field. Alongside the row being rendered, every render receives the standard cross-reaction template keys so templates are portable with the other Drasi reactions:

| Variable | Meaning |
|----------|---------|
| `query_id` / `query_name` | The originating query id (both keys hold the same value). |
| `operation` | `"ADD"`, `"UPDATE"`, or `"DELETE"` (Aggregation surfaces here as `"UPDATE"`). |
| `timestamp` | The emission timestamp (RFC3339). |
| `metadata` | The result metadata map (may be empty). |
| `before` | The before-row (present for UPDATE and DELETE). |
| `after` | The after-row (present for ADD and UPDATE). |
| `data` | The raw `data` payload of an UPDATE diff. |
| `row` | The single row being rendered (the before-row for `Side::Before`, the after-row for `Side::After`). A reaction-specific convenience. |
| `side` | `"before"` or `"after"`. A reaction-specific convenience for an `updated` template that differentiates per side. For ADD it is `"after"`; for DELETE it is `"before"`. |

A `json` helper is registered for embedding a nested object or array as JSON:

```handlebars
{
  "query": "{{query_id}}",
  "operation": "{{operation}}",
  "side": "{{side}}",
  "row": {{json row}}
}
```

The template **must render to valid JSON**. If it does not (or fails to render at all), the reaction logs a warning and falls back to the **raw row state** for that field — see [Render-error fallback](#render-error-fallback). If the JSON rendered is not a top-level object, the framework wraps it as `{"value": ...}` before encoding into the proto `Struct`.

---

## Output schema

Every batch is sent as one unary `ProcessResults` RPC on the
`drasi.v1.ReactionService`. The request and response shapes are:

```protobuf
service ReactionService {
    rpc ProcessResults(ProcessResultsRequest) returns (ProcessResultsResponse);
}

message ProcessResultsRequest {
    QueryResult results = 1;
    map<string, string> metadata = 2;       // also sent as actual gRPC headers
}

message QueryResult {
    string query_id = 1;
    repeated QueryResultItem results = 2;
    google.protobuf.Timestamp timestamp = 3;
}

enum QueryResultItemType {
    // Required by proto3 (zero value must exist); never emitted by the
    // reaction. Aggregation surfaces as UPDATE; Noop never reaches the wire.
    QUERY_RESULT_ITEM_TYPE_UNSPECIFIED = 0;
    QUERY_RESULT_ITEM_TYPE_ADD         = 1;
    QUERY_RESULT_ITEM_TYPE_UPDATE      = 2;
    QUERY_RESULT_ITEM_TYPE_DELETE      = 3;
}

message QueryResultItem {
    QueryResultItemType item_type = 1;
    uint64 row_signature = 2;
    google.protobuf.Struct before = 3;   // populated for UPDATE and DELETE
    google.protobuf.Struct after  = 4;   // populated for ADD and UPDATE
    uint64 sequence = 5;                  // per-query emission sequence number
}

message ProcessResultsResponse {
    bool success = 1;
    string message = 2;
    string error = 3;
    uint32 items_processed = 4;
}
```

How the per-variant fields are populated:

| `item_type` | `before`            | `after`             | Behavior with a template configured                      |
|-------------|---------------------|---------------------|----------------------------------------------------------|
| `ADD`       | _absent_            | new row             | `after` carries the rendered `added` template output     |
| `DELETE`    | previous row        | _absent_            | `before` carries the rendered `deleted` template output  |
| `UPDATE`    | previous row        | new row             | both fields carry **independently** rendered output of the `updated` template (run twice, once per side) |

Notes:

- **Aggregation** results surface as `item_type == UPDATE` on the wire (matches the RabbitMQ and Azure Storage reactions). When the diff carries a `before` value, both sides are templated; otherwise only `after` is templated.
- **Noop** results are dropped at the runner level — they never appear in `QueryResult.results`.
- When **no template** is configured for the op, the field carries the raw row state.
- On **render failure** (template syntax error, non-JSON output, missing field reference, etc.), the reaction logs a warning and falls back to the raw row state for that field. Events are never dropped.
- `row_signature` is the row's identity across emissions and is set on every item. Receivers can use it to correlate or de-duplicate without parsing the row.
- `sequence` is the monotonic per-query emission number of the originating `QueryResult`, set on every item so receivers can order or de-duplicate emissions. Because one `ProcessResultsRequest` may coalesce items from several emissions of the same query, the sequence is carried per item rather than on the envelope.
- `before` / `after` are structurally `google.protobuf.Struct` (JSON-equivalent). If a template renders a non-object top-level value, it is wrapped as `{"value": ...}` before encoding.

One `ProcessResultsRequest` carries one `QueryResult` (a single `query_id` + N items + timestamp). When items for multiple query IDs are batched together by the adaptive runner, the batcher splits them and sends one RPC per query — the proto envelope itself is single-query.

---

## Adaptive batching

Switching `batching.mode` to `"adaptive"` (or calling any per-field setter on the
builder) puts the reaction behind an internal batcher that scales batch size based
on observed throughput between `adaptiveMinBatchSize` and `adaptiveMaxBatchSize`,
flushing a partial batch after `adaptiveBatchTimeoutMs`.

Internally the gRPC adaptive runner has a two-stage pipeline:

1. **Per-query buffering in the runner** — incoming results accumulate per
   `query_id`. The runner releases the buffered batch to the downstream
   `AdaptiveBatcher` when (a) a different `query_id` arrives or (b) the buffer
   reaches 100 items.
2. **Throughput-aware batching by the `AdaptiveBatcher`** — re-coalesces batches by
   `query_id` and emits them either when its size threshold is reached or when
   `adaptiveBatchTimeoutMs` elapses.

This shape favours single-query throughput (one RPC per (query, time-window)). For
low-latency single-query workloads, prefer fixed batching with `batchSize: 1` and a
small `batchFlushTimeoutMs`.

On shutdown the runner drains the in-flight batch through the batcher, then bounds
the batcher join to **5 seconds** via `tokio::time::timeout` — so a stuck downstream
cannot block `stop()` indefinitely. A warning is logged if the drain does not
complete in time and the in-flight batch is abandoned.

---

## Render-error fallback

If an output template fails to render — for example because it references a field
missing from the row, or because the rendered string is not valid JSON — the
reaction:

1. Logs a warning with the query id, operation, side, and the render error.
2. **Falls back to the raw row state** for that field.
3. Still emits the item with the correct `item_type` / `row_signature` and any
   successfully-rendered sibling field (e.g., for an `UPDATE` where only the
   before-side render failed, `before` carries the raw row while `after` carries
   the templated output).

Render failures never drop events. Receivers always read `before` / `after` for the
op — the contract is "if a template is configured and rendered, this is the
template output; otherwise this is the raw row state".

---

## Authentication

Use `with_metadata(...)` to attach per-call entries that ride on every RPC. Each
entry is sent **both** in `ProcessResultsRequest.metadata` (the body field on the
proto) **and** as a real gRPC request header on the underlying HTTP/2 call, so
either reception style (body field or `Context::metadata()`) works for receivers:

```rust,ignore
let reaction = GrpcReaction::builder("my-grpc-reaction")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_endpoint("grpc://collector:50052")
    .with_metadata("authorization", std::env::var("MY_BEARER_TOKEN")?)
    .with_metadata("x-tenant", "tenant-42")
    .build()?;
```

When configured via the dynamic-plugin descriptor, each metadata value is a
`ConfigValue<String>` and may be a static string, an environment-variable
reference (`${VAR}` or `{ kind: EnvironmentVariable, name: VAR }`), or a secret
reference (`{ kind: Secret, name: my-token }`) resolved at construction time
(secret references require a `SecretResolver` registered in the host process).
Entries with invalid HTTP/2 header names or values are skipped from the headers
with a warning, but still ride in the body field.

---

## Examples

### 1. Fixed batching, no templates

Each batch sent as one `ProcessResults` RPC; up to 100 items per batch or every 1 s
(the defaults):

```rust,ignore
let reaction = GrpcReaction::builder("orders-grpc")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_endpoint("grpc://collector:50052")
    .build()?;
core.add_reaction(reaction).await?;
```

### 2. Fixed batching with a per-query template

Reshape each ADD into a domain event placed in `after`:

```rust,ignore
use std::collections::HashMap;
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_reaction_grpc::{GrpcReaction, OutputTemplates};

let mut routes = HashMap::new();
routes.insert(
    "orders-by-region".to_string(),
    QueryConfig {
        added: Some(TemplateSpec::new(
            r#"{"event":"OrderCreated","id":"{{row.id}}","total":{{row.total}}}"#,
        )),
        updated: None,
        deleted: Some(TemplateSpec::new(
            r#"{"event":"OrderCancelled","id":"{{row.id}}"}"#,
        )),
    },
);

let templates = OutputTemplates {
    default_template: None,
    routes,
};

let reaction = GrpcReaction::builder("orders-events")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_endpoint("grpc://collector:50052")
    .with_fixed_batching(100, 250)
    .with_output_templates(templates)
    .build()?;
```

### 3. Adaptive batching (high throughput)

Burst-friendly tuning via the per-field setters, which implicitly enable adaptive
mode:

```rust,ignore
let reaction = GrpcReaction::builder("metrics-grpc")
    .with_queries(vec!["server-metrics".to_string()])
    .with_endpoint("grpc://collector:50052")
    .with_min_batch_size(50)
    .with_max_batch_size(2_000)
    .with_window_size(100)        // 10 s throughput window
    .with_batch_timeout_ms(500)
    .build()?;
```

### 4. Adaptive batching with a default template

```rust,ignore
use drasi_lib::reactions::common::{AdaptiveBatchConfig, QueryConfig, TemplateSpec};
use drasi_reaction_grpc::{GrpcReaction, OutputTemplates};

let templates = OutputTemplates {
    default_template: Some(QueryConfig {
        added: Some(TemplateSpec::new(r#"{"op":"add","row":{{json row}}}"#)),
        updated: Some(TemplateSpec::new(r#"{"op":"upd","row":{{json row}}}"#)),
        deleted: Some(TemplateSpec::new(r#"{"op":"del","row":{{json row}}}"#)),
    }),
    routes: std::collections::HashMap::new(),
};

let reaction = GrpcReaction::builder("orders-batched")
    .with_queries(vec!["orders-by-region".to_string(), "shipments".to_string()])
    .with_endpoint("grpc://collector:50052")
    .with_adaptive_batching(AdaptiveBatchConfig {
        adaptive_min_batch_size: 50,
        adaptive_max_batch_size: 2_000,
        adaptive_window_size: 100,
        adaptive_batch_timeout_ms: 500,
    })
    .with_output_templates(templates)
    .build()?;
```

### 5. Authenticated reaction with per-tenant routing header

```rust,ignore
let reaction = GrpcReaction::builder("orders-authed")
    .with_queries(vec!["orders-by-region".to_string()])
    .with_endpoint("grpc://collector:50052")
    .with_metadata("authorization", std::env::var("MY_BEARER_TOKEN")?)
    .with_metadata("x-tenant", "tenant-42")
    .with_fixed_batching(100, 1_000)
    .build()?;
```

---

## Plugin metadata

The reaction also ships as a Drasi dynamic plugin. The plugin descriptor publishes
named OpenAPI sub-schemas for `GrpcReactionConfig`, `BatchingConfig`,
`OutputTemplates`, `QueryConfig`, and `TemplateSpec`, plus `SchemaUiAnnotator`
groupings (`Connection`, `Reliability`, `Auth`, `Batching`, `Templates`) so
management UIs can render the configuration form.

| Property | Value |
|----------|-------|
| Plugin kind | `grpc` |
| Config version | `3.0.0` |
| Crate version | `0.4.0` |
| Dynamic plugin feature | `dynamic-plugin` |

Build as a dynamic plugin:

```bash
cargo build -p drasi-reaction-grpc --release --features dynamic-plugin
```

The produced `cdylib` exports the standard Drasi plugin entry point.

---

## Testing

```bash
cargo test -p drasi-reaction-grpc
```

Unit tests cover config defaults, builder semantics (including per-field adaptive
setters), template validation at construction, route resolution (exact,
dotted-suffix, default), template render success/failure, and `build_proto_item`
field population for every variant.

Integration tests in `tests/integration_tests.rs` use an in-process tonic mock
`ReactionService` server bound to an ephemeral loopback port to exercise:

- Fixed batching end-to-end (round-trip; verify `item_type`, `row_signature`,
  `before`/`after`, and `sequence`).
- Fixed batching with a template (verify the rendered output lands in `before` /
  `after`).
- Render-error fallback: a non-JSON template output triggers a warning and the
  field carries the raw row state — events are never dropped.
- `UPDATE` and `DELETE` template context (before / after population) and the
  portable standard keys (`after`, `operation`).
- Per-query emission `sequence` propagated on the wire.
- Per-call metadata propagated as actual gRPC request headers, including
  descriptor-resolved `ConfigValue` (environment-variable) metadata.
- Adaptive batching steady-state delivery with `row_signature` round-trip.
- End-to-end through `DrasiLib` (mock source → query → reaction).
- Bounded shutdown drain (the 5 s `tokio::time::timeout` wrap on the batcher join).

---

## Changelog

See [CHANGELOG.md](./CHANGELOG.md).
