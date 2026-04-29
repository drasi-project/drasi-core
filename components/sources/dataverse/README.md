# Dataverse Source for Drasi

A Microsoft Dataverse source plugin for the Drasi platform that monitors Dataverse tables for real-time changes using OData change tracking.

## Overview

This source uses the [OData change tracking](https://learn.microsoft.com/en-us/power-apps/developer/data-platform/use-change-tracking-synchronize-data-external-systems) Web API — the REST equivalent of `RetrieveEntityChangesRequest` — to detect inserts, updates, and deletes in Microsoft Dataverse tables.

### Architecture Alignment with Platform Source

This Rust/Web API implementation mirrors the [platform's C# Dataverse source](https://github.com/drasi-project/drasi-platform/tree/main/sources/dataverse):

| Platform (C#)                     | Drasi-Core (Rust)                       |
|-----------------------------------|-----------------------------------------|
| `RetrieveEntityChangesRequest`    | OData `Prefer: odata.track-changes`     |
| `DataVersion` / `DataToken`       | Delta token in `@odata.deltaLink`       |
| `NewOrUpdatedItem`                | Record without `$deletedEntity` context |
| `RemovedOrDeletedItem`            | Record with `$deletedEntity` in context |
| `SyncWorker` (per-entity)         | Per-entity `tokio::spawn` task          |
| `{entity}-deltatoken` state key   | Same state key format                   |
| `ServiceClient`                   | `reqwest` HTTP client                   |
| Adaptive backoff (500ms → 30s)    | Same adaptive backoff pattern           |

## Quick Start

### Using Azure Identity Provider (recommended)

```rust
use drasi_identity_azure::AzureIdentityProvider;
use drasi_source_dataverse::DataverseSource;

// For local development (uses `az login` credentials)
let identity_provider = AzureIdentityProvider::with_default_credentials("dataverse")?;

// For Azure-hosted apps (uses managed identity)
// let identity_provider = AzureIdentityProvider::new("dataverse")?;

let source = DataverseSource::builder("dv-source")
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_entities(vec!["account".to_string(), "contact".to_string()])
    .with_identity_provider(identity_provider)
    .build()?;
```

### Using client credentials (OAuth2)

```rust
use drasi_source_dataverse::DataverseSource;

let source = DataverseSource::builder("dv-source")
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_tenant_id("00000000-0000-0000-0000-000000000001")
    .with_client_id("00000000-0000-0000-0000-000000000002")
    .with_client_secret("my-client-secret")
    .with_entities(vec!["account".to_string(), "contact".to_string()])
    .build()?;
```

## Authentication

The source supports multiple authentication methods. The Dataverse token scope is derived
automatically from the `environment_url` — you do not need to configure it.

### Azure Identity Provider (`drasi-identity-azure`)

This is the recommended approach. Pass an `AzureIdentityProvider` via `.with_identity_provider()`.
The source automatically passes the correct Dataverse scope (e.g., `https://myorg.crm.dynamics.com/.default`)
to the identity provider via `CredentialContext`.

| Method | Constructor | Use Case |
|--------|-----------|----------|
| Developer tools | `AzureIdentityProvider::with_default_credentials("dataverse")` | Local development (`az login`, `azd`, PowerShell) |
| System-assigned managed identity | `AzureIdentityProvider::new("dataverse")` | Azure Container Apps, VMs, App Service |
| User-assigned managed identity | `AzureIdentityProvider::with_managed_identity("dataverse", "<client-id>")` | Shared identity across Azure resources |
| Workload identity | `AzureIdentityProvider::with_workload_identity("dataverse")` | AKS pods with federated identity |

When using an identity provider, `tenant_id`, `client_id`, and `client_secret` are not required.

### Client Credentials (built-in)

For service-to-service scenarios with an Azure AD app registration and client secret:

```rust
.with_tenant_id("tenant-id")
.with_client_id("client-id")
.with_client_secret("client-secret")
```

Requires an Azure AD app registration with `Dynamics CRM` API permission.

### Azure CLI (built-in)

For quick local development, shells out to `az account get-access-token`:

```rust
.with_azure_cli_auth()
```

When using Azure CLI auth, `tenant_id`, `client_id`, and `client_secret` are not required.
Requires `az login` to have been run beforehand.

## Configuration

| Field                  | Type                        | Default   | Description                                   |
|------------------------|-----------------------------|-----------|-----------------------------------------------|
| `environment_url`      | `String`                    | required  | Dataverse environment URL                     |
| `tenant_id`            | `String`                    | optional* | Azure AD tenant ID (client credentials only)  |
| `client_id`            | `String`                    | optional* | Azure AD application ID (client credentials only) |
| `client_secret`        | `String`                    | optional* | Azure AD client secret (client credentials only) |
| `entities`             | `Vec<String>`               | required  | Entity logical names to monitor               |
| `entity_set_overrides` | `HashMap<String, String>`   | `{}`      | Override entity set name for non-standard pluralization |
| `entity_columns`       | `HashMap<String, Vec<String>>` | `{}`   | Per-entity column selection (`$select`)        |
| `min_interval_ms`      | `u64`                       | `500`     | Minimum adaptive polling interval (ms)        |
| `max_interval_seconds` | `u64`                       | `30`      | Maximum adaptive polling interval (s)         |
| `api_version`          | `String`                    | `"v9.2"`  | Dataverse Web API version                     |

\* Required when using built-in client credentials authentication. Not required when using an identity provider or Azure CLI auth.

### Entity Naming

The source accepts entity **logical names** (singular, lowercase) — matching the platform C# source. Entity set names (plural) are derived automatically by appending `s`. For entities with non-standard pluralization, use `entity_set_overrides`:

```rust
let source = DataverseSource::builder("dv-source")
    // ... auth config ...
    .with_entity("activityparty")
    .with_entity_set_override("activityparty", "activityparties")
    .build()?;
```

### Column Selection

Limit the columns returned per entity to reduce bandwidth:

```rust
let source = DataverseSource::builder("dv-source")
    // ... auth config ...
    .with_entity("account")
    .with_entity_columns("account", vec!["name".to_string(), "revenue".to_string()])
    .build()?;
```

## Prerequisites

1. **Microsoft Dataverse environment** with a URL like `https://yourorg.crm.dynamics.com`
2. **Authentication** — one of:
   - **Identity provider** (recommended): An `AzureIdentityProvider` from `drasi-identity-azure`. For managed identity or workload identity, the identity must be registered as an [application user](https://learn.microsoft.com/en-us/power-platform/admin/manage-application-users) in the Power Platform admin center with an appropriate security role.
   - **Client credentials**: An Azure AD app registration with `Dynamics CRM` API permission and a client secret.
   - **Azure CLI**: `az login` for local development.
3. **Change tracking enabled** on target entities:
   - Dataverse Admin Center → Advanced Settings → Customization → Entities → Enable Change Tracking

## Data Mapping

### Element IDs

Elements are identified as `{entity_name}:{guid}`, e.g., `account:abc-123-def`.

### Labels

Each element has a single label matching its entity logical name (e.g., `account`, `contact`).

### Change Types

| Dataverse Change          | Drasi SourceChange    | Description                              |
|---------------------------|-----------------------|------------------------------------------|
| `NewOrUpdatedItem`        | `SourceChange::Update`| New or modified record (all properties)  |
| `RemovedOrDeletedItem`    | `SourceChange::Delete`| Deleted record (metadata only)           |

Note: Dataverse's delta API does not distinguish between new and updated records. All non-deleted changes are emitted as `Update`.

### Value Conversion

| Dataverse Value Type               | ElementValue          |
|------------------------------------|-----------------------|
| `null`                             | `Null`                |
| `true`/`false`                     | `Bool`                |
| Integer numbers                    | `Integer`             |
| Floating-point numbers             | `Float`               |
| Strings                            | `String`              |
| `{"Value": X}`                     | Unwrapped value of X  |
| `[{"Value": 1}, {"Value": 2}]`    | `List([1, 2])`        |
| Objects                            | `Object`              |

## Adaptive Backoff

The source implements the same two-phase adaptive backoff as the platform's `SyncWorker`:

| Phase                      | Multiplier | Interval Range     |
|----------------------------|------------|--------------------|
| Under 5s threshold         | 1.2x       | 500ms → 5s         |
| Over 5s threshold          | 1.5x       | 5s → 30s (max)     |
| Changes detected           | Reset      | Back to 500ms      |
| Error                      | Fixed      | 5s retry wait      |

## State Management

Delta tokens are persisted to the StateStore with keys in the format `{entity_name}-deltatoken`, matching the platform implementation. This enables:

- **Crash recovery**: Resumes from the last checkpoint
- **Restart**: No re-processing of already-seen changes
- **Cold start**: If no token exists, performs initial change tracking to get the current delta position

## Limitations

- Change tracking must be explicitly enabled per entity in Dataverse admin
- No distinction between insert and update in delta responses
- Polling-based (not push/webhook) — minimum latency is `min_interval_ms`

## Troubleshooting

### "No delta link returned from initial change tracking request"
Change tracking is not enabled on the entity. Enable it in Dataverse admin settings.

### "Failed to get Azure AD token"
- If using managed identity: ensure the identity is enabled on the host (ACA, VM, etc.)
- If using client credentials: verify tenant/client IDs and secret, check secret expiration
- If using Azure CLI: run `az login` first

### "The user is not a member of the organization" (403)
The identity is not registered as an application user in Dataverse. Register it in the Power Platform admin center under **Settings → Users + permissions → Application users**.

### "HTTP status client error (403 Forbidden)"
The application user does not have sufficient permissions. Assign a security role with read access to the target entities.

### No changes detected after modifications
- Verify the entity logical name is correct (singular, lowercase)
- Enable debug logging: `RUST_LOG=debug`
- Check that changes are being made to monitored columns (if using `entity_columns`)
