# Dataverse Bootstrap Provider for Drasi

A bootstrap provider for the Microsoft Dataverse source that loads initial data from Dataverse tables when a query first starts.

## Overview

The bootstrap provider retrieves all existing records from configured Dataverse entities on query startup, populating the query's initial data set. It uses the OData Web API to page through entity data.

## Quick Start

### Using Azure Identity Provider (recommended)

```rust
use drasi_identity_azure::AzureIdentityProvider;
use drasi_bootstrap_dataverse::DataverseBootstrapProvider;
use drasi_source_dataverse::DataverseSource;

let identity_provider = AzureIdentityProvider::with_default_credentials("dataverse")?;

let bootstrap = DataverseBootstrapProvider::builder()
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_identity_provider(identity_provider.clone())
    .with_entities(vec!["account".to_string()])
    .build()?;

let source = DataverseSource::builder("dv-source")
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_identity_provider(identity_provider)
    .with_entities(vec!["account".to_string()])
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

### Using client credentials (OAuth2)

```rust
use drasi_bootstrap_dataverse::DataverseBootstrapProvider;
use drasi_source_dataverse::DataverseSource;

let bootstrap = DataverseBootstrapProvider::builder()
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_tenant_id("tenant-id")
    .with_client_id("client-id")
    .with_client_secret("client-secret")
    .with_entities(vec!["account".to_string()])
    .build()?;

let source = DataverseSource::builder("dv-source")
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_tenant_id("tenant-id")
    .with_client_id("client-id")
    .with_client_secret("client-secret")
    .with_entities(vec!["account".to_string()])
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

## Authentication

The bootstrap provider supports the same authentication methods as the Dataverse source.
The Dataverse token scope is derived automatically from `environment_url`.

### Azure Identity Provider (`drasi-identity-azure`)

Pass an `AzureIdentityProvider` via `.with_identity_provider()`. When using an identity provider,
`tenant_id`, `client_id`, and `client_secret` are not required.

| Method | Constructor | Use Case |
|--------|-----------|----------|
| Developer tools | `AzureIdentityProvider::with_default_credentials("dataverse")` | Local development (`az login`) |
| System-assigned managed identity | `AzureIdentityProvider::new("dataverse")` | Azure Container Apps, VMs, App Service |
| User-assigned managed identity | `AzureIdentityProvider::with_managed_identity("dataverse", "<client-id>")` | Shared identity across Azure resources |
| Workload identity | `AzureIdentityProvider::with_workload_identity("dataverse")` | AKS pods with federated identity |

### Client Credentials (built-in)

```rust
.with_tenant_id("tenant-id")
.with_client_id("client-id")
.with_client_secret("client-secret")
```

### Azure CLI (built-in)

```rust
.with_azure_cli_auth()
```

## Configuration

| Field                  | Type                        | Default   | Description                                   |
|------------------------|-----------------------------|-----------|-----------------------------------------------|
| `environment_url`      | `String`                    | required  | Dataverse environment URL                     |
| `tenant_id`            | `String`                    | optional* | Azure AD tenant ID (client credentials only)  |
| `client_id`            | `String`                    | optional* | Azure AD application ID (client credentials only) |
| `client_secret`        | `String`                    | optional* | Azure AD client secret (client credentials only) |
| `entities`             | `Vec<String>`               | required  | Entity logical names to bootstrap             |
| `entity_set_overrides` | `HashMap<String, String>`   | `{}`      | Override entity set name for non-standard pluralization |
| `entity_columns`       | `HashMap<String, Vec<String>>` | `{}`   | Per-entity column selection (`$select`)        |
| `api_version`          | `String`                    | `"v9.2"`  | Dataverse Web API version                     |
| `page_size`            | `usize`                     | `5000`    | Records per page during bootstrap             |

\* Required when using built-in client credentials authentication. Not required when using an identity provider or Azure CLI auth.

## Usage with DataverseSource

The bootstrap provider is typically paired with the Dataverse source. When using an identity provider,
share the same instance between both so they use the same credentials:

```rust
use drasi_identity_azure::AzureIdentityProvider;
use drasi_bootstrap_dataverse::DataverseBootstrapProvider;
use drasi_source_dataverse::DataverseSource;

let identity = AzureIdentityProvider::new("dataverse")?;

let bootstrap = DataverseBootstrapProvider::builder()
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_identity_provider(identity.clone())
    .with_entities(vec!["account".to_string()])
    .build()?;

let source = DataverseSource::builder("dv-source")
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_identity_provider(identity)
    .with_entities(vec!["account".to_string()])
    .with_bootstrap_provider(bootstrap)
    .build()?;
```

## Data Mapping

### Element IDs
Bootstrap records are identified as `{entity_name}:{guid}`. The ID field is extracted from `{entity_name}id` (e.g., `accountid` for accounts) or falls back to `id`.

### Labels
Each bootstrapped element has a single label matching its entity logical name.

### Change Type
All bootstrap records are emitted as `SourceChange::Insert`, since the bootstrap represents the initial state of the data.

### Filtering
- OData annotation fields (keys starting with `@`) are automatically filtered out
- Only entities matching the requested labels are bootstrapped (label filtering)

## Prerequisites

1. **Microsoft Dataverse environment**
2. **Authentication** — one of:
   - **Identity provider** (recommended): An `AzureIdentityProvider` from `drasi-identity-azure`. For managed identity or workload identity, the identity must be registered as an [application user](https://learn.microsoft.com/en-us/power-platform/admin/manage-application-users) in the Power Platform admin center with an appropriate security role.
   - **Client credentials**: An Azure AD app registration with `Dynamics CRM` API permission and a client secret.
   - **Azure CLI**: `az login` for local development.
3. Records must exist in the target entities for bootstrap to return data
