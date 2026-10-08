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

## Lifecycle and delivery boundary

Startup and stop are serialized. Running is published only after SDK client
configuration and ownership-before-spawn registration of the processor; it does
not prove remote credentials or queue access. Duplicate start, cancelled startup,
and incomplete previous cleanup reject replacement work.

Stop closes ordinary processing admission and gracefully joins the processor.
An in-flight `SendMessage` (including its SDK retries) remains owned through
cancelled cleanup or the two-second processor join deadline. A timeout reports
incomplete cleanup without aborting the request; retry stop after it completes.
Restart remains blocked until processor and base cleanup both finish.

This does not change the existing **Accepted**, best-effort legacy boundary:
render/send errors are logged and later results may continue. Stop does not
promise to drain the whole queued backlog. There is no durable consumer
checkpoint or atomic effect/receipt transaction. FIFO deduplication IDs are
generated per send invocation, not stable across graph replay.

## Configuration

### Builder usage

```rust
use drasi_reaction_aws_sqs::{SqsReaction, QueryConfig, TemplateSpec};

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
| `queueUrl` | `String` | Yes | - | Full SQS queue URL |
| `region` | `Option<String>` | No | `None` | AWS region override |
| `endpointUrl` | `Option<String>` | No | `None` | Custom SQS endpoint (ElasticMQ/LocalStack) |
| `accessKeyId` | `Option<String>` | No | `None` | Explicit AWS access key ID (primarily for local testing) |
| `secretAccessKey` | `Option<String>` | No | `None` | Explicit AWS secret access key (primarily for local testing) |
| `fifoQueue` | `bool` | No | `false` | Enables FIFO send options (`message_group_id`, dedup id) |
| `messageGroupIdTemplate` | `Option<String>` | No | `None` | Handlebars template for FIFO group id; falls back to query id |
| `routes` | `HashMap<String, QueryConfig>` | No | `{}` | Query-specific templates |
| `defaultTemplate` | `Option<QueryConfig>` | No | `None` | Fallback templates when a query route is missing |

**Note:** In production, prefer using IAM roles or environment-based AWS credentials instead of embedding `access_key_id` and `secret_access_key` directly in configuration.

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
| `messageAttributes` | `HashMap<String, String>` | `{}` | SQS attribute values rendered as Handlebars templates. |

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

User-defined attributes from `TemplateSpec.message_attributes` are rendered first;
system attributes are applied last and cannot be overridden.

## Testing

Unit and local protocol lifecycle tests (no cloud account required):
```bash
cargo test -p drasi-reaction-aws-sqs
```

Integration tests (ElasticMQ containers):
```bash
cargo test -p drasi-reaction-aws-sqs -- --ignored --nocapture
```

The runtime parity runner includes both container tests. Current-thread lifecycle
cases hold real SDK HTTP replies through stop cancellation and timeout, verify
Standard/FIFO payloads across three restarts, require owned registration before
Running, and retain typed worker-panic/remaining-cleanup failures.

## Limitations

- Uses one `SendMessage` call per diff (no `SendMessageBatch` optimization yet)
- SQS message size limit applies (256KB)
- Aggregation diffs are sent as `UPDATE` messages; Noop diffs are ignored
