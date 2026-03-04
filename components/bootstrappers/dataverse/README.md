# Dataverse Bootstrap Provider for Drasi

A bootstrap provider for the Microsoft Dataverse source that loads initial data from Dataverse tables when a query first starts.

## Overview

The bootstrap provider retrieves all existing records from configured Dataverse entities on query startup, populating the query's initial data set. It uses the OData Web API to page through entity data.

## Quick Start

```rust
use drasi_bootstrap_dataverse::DataverseBootstrapProvider;

let bootstrap = DataverseBootstrapProvider::builder()
    .with_environment_url("https://myorg.crm.dynamics.com")
    .with_tenant_id("00000000-0000-0000-0000-000000000001")
    .with_client_id("00000000-0000-0000-0000-000000000002")
    .with_client_secret("my-client-secret")
    .with_entities(vec!["account".to_string(), "contact".to_string()])
    .build()?;
```

## Configuration

| Field                  | Type                        | Default   | Description                                   |
|------------------------|-----------------------------|-----------|-----------------------------------------------|
| `environment_url`      | `String`                    | required  | Dataverse environment URL                     |
| `tenant_id`            | `String`                    | required  | Azure AD tenant ID                            |
| `client_id`            | `String`                    | required  | Azure AD application (client) ID              |
| `client_secret`        | `String`                    | required  | Azure AD client secret                        |
| `entities`             | `Vec<String>`               | required  | Entity logical names to bootstrap             |
| `entity_set_overrides` | `HashMap<String, String>`   | `{}`      | Override entity set name for non-standard pluralization |
| `entity_columns`       | `HashMap<String, Vec<String>>` | `{}`   | Per-entity column selection (`$select`)        |
| `api_version`          | `String`                    | `"v9.2"`  | Dataverse Web API version                     |
| `page_size`            | `usize`                     | `5000`    | Records per page during bootstrap             |

## Usage with DataverseSource

Typically used together with the Dataverse source:

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

Same as the Dataverse source:
1. Microsoft Dataverse environment
2. Azure AD app registration with Dynamics CRM permissions
3. Records must exist in the target entities for bootstrap to return data
