# SQS Reaction

AWS SQS reaction plugin for Drasi.  
This component sends query result diffs (`ADD`, `UPDATE`, `DELETE`) to Amazon SQS as JSON messages.

## Overview

The SQS reaction listens to subscribed query outputs and converts each `ResultDiff` into an SQS `SendMessage` call.

Key capabilities:
- Standard and FIFO queue support
- Per-query routing (`routes`) with fallback (`default_template`)
- Handlebars templates for message body
- Handlebars templates for SQS `MessageAttributes`
- Built-in system attributes: `drasi-query-id`, `drasi-operation`
- Optional local endpoint override for ElasticMQ/LocalStack testing

## Configuration

### Builder usage

```rust
use drasi_reaction_sqs::{SqsReaction, QueryConfig, TemplateSpec};

let reaction = SqsReaction::builder("sqs-reaction")
    .with_queue_url("https://sqs.us-east-1.amazonaws.com/123456789012/products")
    .with_region("us-east-1")
    .with_query("product-query")
    .with_default_template(QueryConfig {
        added: Some(
            TemplateSpec::new(
                "{\"event\":\"add\",\"id\":\"{{after.id}}\",\"data\":{{json after}}}"
            )
            .with_message_attribute("entity-id", "{{after.id}}")
        ),
        updated: Some(TemplateSpec::new(
            "{\"event\":\"update\",\"before\":{{json before}},\"after\":{{json after}}}"
        )),
        deleted: Some(TemplateSpec::new(
            "{\"event\":\"delete\",\"id\":\"{{before.id}}\"}"
        )),
    })
    .build()?;
```

### `SqsReactionConfig`

| Field | Type | Required | Default | Description |
|---|---|---:|---|---|
| `queue_url` | `String` | Yes | - | Full SQS queue URL |
| `region` | `Option<String>` | No | `None` | AWS region override |
| `endpoint_url` | `Option<String>` | No | `None` | Custom SQS endpoint (ElasticMQ/LocalStack) |
| `fifo_queue` | `bool` | No | `false` | Enables FIFO send options (`message_group_id`, dedup id) |
| `message_group_id_template` | `Option<String>` | No | `None` | Handlebars template for FIFO group id; falls back to query id |
| `routes` | `HashMap<String, QueryConfig>` | No | `{}` | Query-specific templates |
| `default_template` | `Option<QueryConfig>` | No | `None` | Fallback templates when a query route is missing |

### `QueryConfig`

| Field | Type | Description |
|---|---|---|
| `added` | `Option<TemplateSpec>` | Template used for ADD diffs |
| `updated` | `Option<TemplateSpec>` | Template used for UPDATE diffs |
| `deleted` | `Option<TemplateSpec>` | Template used for DELETE diffs |

### `TemplateSpec`

| Field | Type | Default | Description |
|---|---|---|---|
| `body` | `String` | empty | Handlebars template for SQS message body. Empty means raw JSON fallback. |
| `message_attributes` | `HashMap<String, String>` | `{}` | SQS attribute values rendered as Handlebars templates. |

## Template Variables

Templates can reference:
- `after` (ADD/UPDATE)
- `before` (UPDATE/DELETE)
- `query_id`
- `query_name`
- `operation`
- `timestamp`

Helper:
- `{{json value}}` serializes nested objects.

## Message Attributes

Every message includes:
- `drasi-query-id`
- `drasi-operation`

User-defined attributes from `TemplateSpec.message_attributes` are rendered and merged after system attributes, so users can override system keys if needed.

## Testing

Unit tests:
```bash
cargo test -p drasi-reaction-aws-sqs
```

Integration test (ElasticMQ container):
```bash
cargo test -p drasi-reaction-aws-sqs -- --ignored --nocapture
```

## Limitations

- Uses one `SendMessage` call per diff (no `SendMessageBatch` optimization yet)
- SQS message size limit applies (256KB)
- Aggregation/Noop diffs are ignored
