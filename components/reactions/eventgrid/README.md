# Drasi Azure Event Grid Reaction

Publishes Drasi continuous-query result changes to an **Azure Event Grid custom
topic** as events. This is a native `drasi-lib` port of the Drasi Platform
[Event Grid reaction](https://github.com/drasi-project/drasi-platform/tree/main/reactions/azure/eventgrid-reaction).

The reaction is a **publish-only, protocol target**: it POSTs event batches to
the topic's HTTP endpoint. Publishing uses plain HTTP (`reqwest`) rather than
the Azure Event Grid SDK, keeping the dependency surface small and the reaction
fully testable against a local mock server.

## Features

- **Two wire schemas**
  - `CloudEvents` — CloudEvents 1.0 (default). Template `metadata` becomes
    CloudEvent extension attributes.
  - `EventGrid` — native Event Grid schema.
- **Three output formats**
  - `packed` — one event carrying the raw packed `QueryResult` (default).
  - `unpacked` — one event per result-set change (add/update/delete).
  - `template` — Handlebars-templated event `data` per query per operation.
- **Two authentication modes**
  - **Access key** — the topic key sent via the `aeg-sas-key` header.
  - **Microsoft Entra Workload Identity** — an AAD bearer token scoped to
    `https://eventgrid.azure.net/.default`, obtained from the reaction's identity
    provider.
- **Deterministic event ids** (UUIDv5 from reaction id + query id + sequence +
  op + index) so retries and outbox replays carry the same id, enabling
  downstream deduplication.

## Configuration

| Property | Type | Required | Default | Description |
|----------|------|----------|---------|-------------|
| `endpoint` | string | yes | — | Full Event Grid custom-topic events URL, e.g. `https://<topic>.<region>.eventgrid.azure.net/api/events`. Passed as-is (matching the platform). |
| `accessKey` | string | no¹ | — | Topic access key, sent as the `aeg-sas-key` header. |
| `schema` | `CloudEvents` \| `EventGrid` | no | `CloudEvents` | Wire schema. |
| `format` | `packed` \| `unpacked` \| `template` | no | `packed` | Output format. |
| `timeoutMs` | integer | no | `10000` | Per-request timeout in milliseconds. |
| `outputTemplates` | object | no | — | Per-query and default templates (template format). |

¹ `accessKey` is required unless an identity provider is configured for AAD
authentication.

### Per-query templates (`template` format)

```yaml
outputTemplates:
  routes:
    product-inventory:
      added:
        template: |
          { "eventType": "ProductAdded", "productId": {{after.product_id}} }
        metadata:
          category: inventory
          action: create
```

Template variables: `after`, `before`, `data`, `query_name` / `query_id`,
`operation` (`ADD`/`UPDATE`/`DELETE`), `timestamp`. The `metadata` map is applied
as CloudEvent extension attributes (CloudEvents schema only; dropped with a
warning for the native EventGrid schema).

## Usage

### Access key, CloudEvents, unpacked

```rust
use drasi_reaction_eventgrid::{EventGridReaction, EventGridSchema, OutputFormat};

let reaction = EventGridReaction::builder("orders-eventgrid")
    .with_query("orders")
    .with_endpoint("https://my-topic.eastus-1.eventgrid.azure.net/api/events")
    .with_access_key("<topic-access-key>")
    .with_schema(EventGridSchema::CloudEvents)
    .with_format(OutputFormat::Unpacked)
    .build()?;
```

### Template format with metadata

```rust
use drasi_reaction_eventgrid::{
    EventGridQueryConfig, EventGridReaction, EventGridTemplateExt, EventGridTemplateSpec,
    OutputFormat,
};
use std::collections::HashMap;

let template = EventGridQueryConfig {
    added: Some(EventGridTemplateSpec::with_extension(
        r#"{ "eventType": "ProductAdded", "productId": {{after.product_id}} }"#,
        EventGridTemplateExt {
            metadata: HashMap::from([("category".to_string(), "inventory".to_string())]),
        },
    )),
    ..Default::default()
};

let reaction = EventGridReaction::builder("inv-eventgrid")
    .with_endpoint("https://my-topic.eastus-1.eventgrid.azure.net/api/events")
    .with_access_key("<topic-access-key>")
    .with_format(OutputFormat::Template)
    .with_query("product-inventory")
    .with_query_template("product-inventory", template)
    .build()?;
```

### AAD (Microsoft Entra Workload Identity)

Omit `accessKey` and attach an Azure identity provider to DrasiLib. The reaction
requests a token scoped to `https://eventgrid.azure.net/.default`:

```rust
use std::sync::Arc;
use drasi_identity_azure::AzureIdentityProvider;

let identity = AzureIdentityProvider::with_workload_identity("eventgrid")?
    .with_scope("https://eventgrid.azure.net/.default");

let drasi = DrasiLib::builder()
    .with_identity_provider(Arc::new(identity))
    // ... sources, queries ...
    .with_reaction(
        EventGridReaction::builder("orders-eventgrid")
            .with_endpoint("https://my-topic.eastus-1.eventgrid.azure.net/api/events")
            .with_query("orders")
            .build()?,
    )
    .build()
    .await?;
```

## Output examples

**CloudEvents, unpacked, INSERT** — POSTed as
`Content-Type: application/cloudevents-batch+json`:

```json
[
  {
    "id": "927a85e5-a832-41ef-ba10-44bf36b10cd2",
    "source": "orders",
    "type": "Drasi.ChangeEvent",
    "specversion": "1.0",
    "time": "2026-07-10T18:26:06+00:00",
    "subject": "orders",
    "data": { "op": "i", "ts_ms": 1700000000000,
              "payload": { "source": { "queryId": "orders", "ts_ms": 1700000000000 },
                           "after": { "id": 1, "name": "widget" } } }
  }
]
```

**Native EventGrid, unpacked, INSERT** — POSTed as `Content-Type: application/json`:

```json
[
  {
    "id": "927a85e5-a832-41ef-ba10-44bf36b10cd2",
    "subject": "orders",
    "eventType": "Drasi.ChangeEvent",
    "eventTime": "2026-07-10T18:26:06+00:00",
    "data": { "op": "i", "...": "..." },
    "dataVersion": "1"
  }
]
```

Unpacked `op` codes: `i` (insert), `u` (update), `d` (delete). Aggregation diffs
map to `u`.

## Delivery guarantees

- **At-least-once.** Event Grid does not deduplicate custom events. On
  crash/replay the same change may be re-published. Subscribers should dedupe on
  the event `id`, which is **deterministic** for a given
  `(reaction, query, sequence, op, index)` tuple.
- **Retries.** Transient failures (network errors, 5xx, 429, 408) are retried up
  to 3 times with exponential backoff. Non-transient failures (e.g. 401) are
  logged and the message is dropped; the processing loop keeps running.
- **Recovery policy.** The reaction is stateless (`is_durable() == false`,
  `needs_snapshot_on_fresh_start() == false`) and defaults to
  `ReactionRecoveryPolicy::AutoSkipGap`. Override to `Strict` if you require
  strict at-least-once from the last checkpoint.

## Limitations

- The native **EventGrid schema drops template `metadata`** (it has no
  extension-attribute concept). Use the CloudEvents schema to carry metadata.
- Event Grid enforces per-request size limits (~1 MB total). Very large `packed`
  payloads may be rejected; chunking is not implemented in this version.

## Testing

Unit tests:

```bash
make -C components/reactions/eventgrid test
```

Integration tests (protocol-target; a local `wiremock` server stands in for the
topic — no Azure account or Docker required):

```bash
make -C components/reactions/eventgrid integration-test
```

The integration tests drive an `ApplicationSource` through a real `DrasiLib`
instance and assert on the captured request bodies for INSERT, UPDATE, DELETE
across both schemas, template-metadata extension attributes, and publish-failure
handling.

### Manual smoke test against a real topic

With an Azure subscription: create an Event Grid custom topic, copy its
**Endpoint** and an **access key**, then build the reaction with those values and
a real source/query. Confirm events arrive at an Event Grid subscription (e.g. an
Event Hub, Storage Queue, or webhook).
