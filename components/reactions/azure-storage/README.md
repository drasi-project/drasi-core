# Azure Storage Reaction

Azure Storage Reaction is a Drasi reaction plugin that writes query-result changes to Azure Storage targets:

- Azure Blob Storage
- Azure Queue Storage
- Azure Table Storage

It supports operation-specific Handlebars templates (`ADD`, `UPDATE`, `DELETE`) and query-specific routes with a default fallback template.

## Configuration Overview

### Core settings

| Field | Type | Required | Description |
|---|---|---|---|
| `accountName` | string | Yes | Azure storage account name |
| `accessKey` | string / config value | Yes | Azure storage account key |
| `target` | object | Yes | Destination target (blob/queue/table) |
| `blobEndpoint` | string | No | Custom blob endpoint (Azurite/local) |
| `queueEndpoint` | string | No | Custom queue endpoint (Azurite/local) |
| `tableEndpoint` | string | No | Custom table endpoint (Azurite/local) |
| `routes` | map | No | Per-query templates |
| `defaultTemplate` | object | No | Fallback templates |

### Target variants

#### Blob target

```json
{
  "type": "blob",
  "containerName": "events",
  "blobPathTemplate": "{{#if after}}{{after.id}}{{else}}{{before.id}}{{/if}}.txt",
  "contentType": "text/plain"
}
```

#### Queue target

```json
{
  "type": "queue",
  "queueName": "events"
}
```

#### Table target

```json
{
  "type": "table",
  "tableName": "EventTable",
  "partitionKeyTemplate": "{{#if after}}{{after.region}}{{else}}{{before.region}}{{/if}}",
  "rowKeyTemplate": "{{#if after}}{{after.id}}{{else}}{{before.id}}{{/if}}"
}
```

## Template Variables

| Variable | ADD | UPDATE | DELETE |
|---|---|---|---|
| `after` | ✅ | ✅ | ❌ |
| `before` | ❌ | ✅ | ✅ |
| `data` | ❌ | ✅ | ❌ |
| `query_name` | ✅ | ✅ | ✅ |
| `operation` | ✅ | ✅ | ✅ |

`{{json ...}}` helper is available to render JSON values safely.

## Builder Example

```rust,ignore
use drasi_reaction_azure_storage::{AzureStorageReaction, QueryConfig, StorageTarget, TemplateSpec};

let reaction = AzureStorageReaction::builder("azure-reaction")
    .with_query("sensor-query")
    .with_account_name("devstoreaccount1")
    .with_access_key("<key>")
    .with_target(StorageTarget::Queue {
        queue_name: "sensor-events".to_string(),
    })
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new(r#"{"op":"{{operation}}","payload":{{json after}}}"#)),
        updated: Some(TemplateSpec::new(r#"{"op":"{{operation}}","payload":{{json after}}}"#)),
        deleted: Some(TemplateSpec::new(r#"{"op":"{{operation}}","payload":{{json before}}}"#)),
    })
    .build()?;
```

## Integration Testing

Integration tests run against:

- `mcr.microsoft.com/azure-storage/azurite:latest`

Tests verify INSERT/UPDATE/DELETE end-to-end for each target type by querying the target system state.

Run integration tests:

```bash
make integration-test
```

## Limitations

- Azure Files/Shares are not supported.
- Uses the current Azure Rust SDK legacy crate set (`0.21.x`).
