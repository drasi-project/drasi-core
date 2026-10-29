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

> `data` contains the raw update payload for an UPDATE operation (the full serialized `ResultDiff`).

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
- Uses the legacy Azure Rust Storage SDK `0.21.x` to retain account-key
  authentication, custom Azurite endpoints and Table operations. The maintained
  Blob/Queue 1.1 SDKs do not provide a drop-in replacement for this complete
  capability set.

### Legacy SDK credential logging mitigation

`RUSTSEC-2026-0275` affects the SDK's request and successful-retry debug/trace
output. Every client replaces the final request-logging transport with a
redacting transport: the original SDK HTTP client receives the signed request,
then the authorization value is replaced with `<redacted>` before retry
logging. Cancellation performs the same cleanup, and each retry is re-signed
by the SDK. Authentication, timeouts, retries and other diagnostics are retained;
no global subscriber or logging filter is changed. This is a runtime
mitigation, **not** a patched SDK or an audit exemption.

Queue replies and storage errors also pass through a quick-xml 0.41 preflight
before legacy XML deserialization. It rejects duplicate attributes and limits
fixed-schema service metadata to 256 attributes per element, mitigating
`RUSTSEC-2026-0194` without disabling duplicate checks. The legacy deserializer
uses a plain reader, not the namespace resolver affected by
`RUSTSEC-2026-0195`. Blob writes, queue messages, Table entities, templates and
custom endpoints retain their existing behavior.

Treat logs from older builds as potentially credential-bearing. Rotate keys
used while SDK debug/trace logging was enabled. The mitigation does not sanitize
configuration deliberately logged by application code or independent Azure
clients outside this plugin.
