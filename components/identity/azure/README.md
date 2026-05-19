# Azure Identity Provider

`drasi-identity-azure` provides Azure Entra ID (formerly Azure AD) authentication for Drasi sources and reactions. It supports managed identity, workload identity, and developer tools authentication.

## Installation

```toml
[dependencies]
drasi-identity-azure = { path = "path/to/drasi-core/components/identity/azure" }
```

## Authentication Methods

### System-Assigned Managed Identity

For Azure VMs, App Service, Azure Functions, or Azure Container Apps with a system-assigned managed identity enabled:

```rust
use drasi_identity_azure::AzureIdentityProvider;

let provider = AzureIdentityProvider::new("myapp-identity")?;
```

The `identity_name` parameter is used as the PostgreSQL username and must match the database role name you created (see [Database Setup](#database-setup)).

### User-Assigned Managed Identity

When using a user-assigned managed identity, pass the client ID:

```rust
let provider = AzureIdentityProvider::with_managed_identity(
    "myapp-identity",           // PostgreSQL role name
    "12345678-abcd-efgh-ijkl",  // managed identity client ID
)?;
```

### Developer Tools (Local Development)

Uses the credential chain from `az login`, Visual Studio, or other developer tools:

```rust
let provider = AzureIdentityProvider::with_default_credentials("user@example.com")?;
```

The `identity_name` must be your Azure AD principal name (email) and must match a PostgreSQL role with an AAD security label.

### Workload Identity (AKS)

For Azure Kubernetes Service with workload identity configured:

```rust
let provider = AzureIdentityProvider::with_workload_identity("myapp-identity")?;
```

Requires the following environment variables (automatically set by AKS):
- `AZURE_TENANT_ID`
- `AZURE_CLIENT_ID`
- `AZURE_FEDERATED_TOKEN_FILE`

### Custom Scope

All methods return a builder-style `AzureIdentityProvider` that defaults to the `ossrdbms-aad` scope (for PostgreSQL/MySQL). Override it for other resources:

```rust
let provider = AzureIdentityProvider::new("myapp")?
    .with_scope("https://my-custom-resource/.default");
```

## Complete Examples

### Local Development with Azure PostgreSQL

```rust
use drasi_lib::{DrasiLib, Query};
use drasi_identity_azure::AzureIdentityProvider;
use drasi_source_mock::{MockSource, MockSourceConfig, DataType};
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
use std::sync::Arc;

// 1. Authenticate with `az login` credentials
let provider = AzureIdentityProvider::with_default_credentials("user@example.com")?;

// 2. Create the reaction — identity provider passed at framework level
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.postgres.database.azure.com")
    .with_port(5432)
    .with_database("mydb")
    .with_ssl(true)
    .with_user("user@example.com")  // pass validation; overridden by identity provider at runtime
    .with_query("my-query")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL handle_added(@after.id, @after.value)")),
        updated: Some(TemplateSpec::new("CALL handle_updated(@after.id, @after.value)")),
        deleted: Some(TemplateSpec::new("CALL handle_deleted(@before.id)")),
    })
    .build()
    .await?;

// 3. Wire up with DrasiLib
let app = DrasiLib::builder()
    .with_id("my-app")
    .with_identity_provider(Arc::new(provider))
    .with_source(my_source)
    .with_reaction(reaction)
    .with_query(
        Query::cypher("my-query")
            .query("MATCH (n:MyNode) RETURN n.id AS id, n.value AS value")
            .from_source("my-source")
            .build()
    )
    .build()
    .await?;

app.start().await?;
```

### Azure Container Apps with Managed Identity

```rust
use drasi_identity_azure::AzureIdentityProvider;
use std::sync::Arc;

// Read identity name from environment
let identity_name = std::env::var("AZURE_IDENTITY_NAME")
    .unwrap_or_else(|_| "my-aca-app".to_string());

let provider = AzureIdentityProvider::new(&identity_name)?;

let app = DrasiLib::builder()
    .with_id("my-app")
    .with_identity_provider(Arc::new(provider))
    // ... sources, reactions, queries
    .build()
    .await?;
```

### Switching Auth Mode via Environment Variable

A common pattern for apps that run both locally and in the cloud:

```rust
let auth_mode = std::env::var("AUTH_MODE").unwrap_or_else(|_| "managed_identity".to_string());

let provider: Arc<dyn drasi_lib::identity::IdentityProvider> = match auth_mode.as_str() {
    "managed_identity" => {
        let name = std::env::var("AZURE_IDENTITY_NAME").expect("AZURE_IDENTITY_NAME required");
        Arc::new(AzureIdentityProvider::new(&name)?)
    }
    "developer" => {
        let name = std::env::var("AZURE_IDENTITY_NAME").expect("AZURE_IDENTITY_NAME required");
        Arc::new(AzureIdentityProvider::with_default_credentials(&name)?)
    }
    "password" => {
        let user = std::env::var("PG_USER").expect("PG_USER required");
        let pass = std::env::var("PG_PASSWORD").expect("PG_PASSWORD required");
        Arc::new(drasi_lib::identity::PasswordIdentityProvider::new(user, pass))
    }
    _ => panic!("Unknown AUTH_MODE"),
};
```

## Database Setup

### Prerequisites

1. **Enable Entra authentication** on your Azure PostgreSQL Flexible Server
2. **Add yourself as an Entra admin** on the server

### Create a Role for Managed Identity

Connect to PostgreSQL as an Entra admin (not a password-based admin) and run:

```sql
-- Install the pgaadauth extension (if not already present)
CREATE EXTENSION IF NOT EXISTS pgaadauth;

-- Create the role
CREATE ROLE "my-identity-name" LOGIN;

-- Attach the AAD security label (use the managed identity's Object/Principal ID)
SECURITY LABEL FOR "pgaadauth" ON ROLE "my-identity-name"
    IS 'aadauth,oid=<principal-id>,type=service';

-- Grant permissions
GRANT CONNECT ON DATABASE mydb TO "my-identity-name";
GRANT USAGE ON SCHEMA public TO "my-identity-name";
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO "my-identity-name";
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO "my-identity-name";
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO "my-identity-name";
GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA public TO "my-identity-name";
```

**Important**: The `SECURITY LABEL` command must be run by an Entra-mapped user. If you get an error about security labels only being applied by Entra principals, connect using an access token:

```bash
export PGPASSWORD=$(az account get-access-token --resource-type oss-rdbms --query accessToken -o tsv)
psql "host=myserver.postgres.database.azure.com port=5432 dbname=mydb user=me@example.com sslmode=require"
```

### Create a Role for Developer Access

For local development with `az login`:

```sql
CREATE ROLE "user@example.com" LOGIN;
SECURITY LABEL FOR "pgaadauth" ON ROLE "user@example.com"
    IS 'aadauth,oid=<your-user-object-id>,type=user';

GRANT CONNECT ON DATABASE mydb TO "user@example.com";
GRANT USAGE ON SCHEMA public TO "user@example.com";
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO "user@example.com";
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO "user@example.com";
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO "user@example.com";
GRANT EXECUTE ON ALL PROCEDURES IN SCHEMA public TO "user@example.com";
```

Get your object ID with: `az ad signed-in-user show --query id -o tsv`

## Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| `password authentication failed for user "X"` | Missing AAD security label on the PostgreSQL role | Run `SECURITY LABEL FOR "pgaadauth" ...` as an Entra admin |
| `extension "pgaadauth" is not allow-listed` | pgaadauth not available to your admin role | Connect as an Entra admin user, not a password-based admin |
| `role "X" does not exist` | PostgreSQL role not created | Run `CREATE ROLE "X" LOGIN;` |
| Token acquisition fails in ACA | Managed identity not enabled | Enable system-assigned identity on the container app |
| `AZURE_IDENTITY_NAME` mismatch | Identity name doesn't match PostgreSQL role | Ensure the name passed to `AzureIdentityProvider` matches the role name exactly |
