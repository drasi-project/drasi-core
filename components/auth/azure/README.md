# Azure Identity Authentication for Drasi

Azure Identity authentication library for Drasi components, providing Azure Active Directory credential support for sources, reactions, and other plugins.

## Overview

This crate provides authentication using Azure Identity with two main approaches:

- **DefaultAzureCredential**: Automatically tries multiple credential types in order (Environment variables, Managed Identity, Azure CLI, Azure PowerShell, etc.) - **Recommended for most use cases**
- **Service Principal**: Explicit client credentials flow with tenant ID, client ID, and secret

**Note**: DefaultAzureCredential automatically handles Managed Identity (both system-assigned and user-assigned) when running in Azure, so you don't need separate managed identity configuration.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
drasi-auth-azure = { path = "path/to/components/auth/azure" }
```

## Usage Examples

### PostgreSQL with DefaultAzureCredential (Recommended)

The simplest approach - automatically handles managed identity, Azure CLI, and more:

```rust
use drasi_auth_azure::get_postgres_token_with_default_credential;

let token = get_postgres_token_with_default_credential().await?;
// Use token as password in PostgreSQL connection
```

### MySQL with DefaultAzureCredential (Recommended)

```rust
use drasi_auth_azure::get_mysql_token_with_default_credential;

let token = get_mysql_token_with_default_credential().await?;
// Use token as password in MySQL connection
```

### Azure SQL with DefaultAzureCredential (Recommended)

```rust
use drasi_auth_azure::get_sql_token_with_default_credential;

let token = get_sql_token_with_default_credential().await?;
// Use token as password in SQL connection
```

### Service Principal Authentication

For scenarios requiring explicit service principal credentials:

```rust
use drasi_auth_azure::get_postgres_token_with_service_principal;

let token = get_postgres_token_with_service_principal(
    "tenant-id",
    "client-id",
    "client-secret"
).await?;
// Use token as password in connection
```

Similar functions exist for MySQL and SQL:
- `get_mysql_token_with_service_principal()`
- `get_sql_token_with_service_principal()`

### Using the AzureIdentityAuth Enum Directly

For more control, you can use the `AzureIdentityAuth` enum:

```rust
use drasi_auth_azure::AzureIdentityAuth;

// DefaultAzureCredential
let auth = AzureIdentityAuth::default();
let token = auth.get_token(&["https://database.windows.net/.default"]).await?;

// Service Principal
let auth = AzureIdentityAuth::service_principal(
    "tenant-id",
    "client-id",
    "client-secret"
);
let token = auth.get_token(&["https://database.windows.net/.default"]).await?;
```

## Token Scopes for Azure Databases

### Azure SQL Database
```rust
let token = auth.get_sql_token().await?;
// Scope: https://database.windows.net/.default
```

### Azure Database for PostgreSQL / MySQL
```rust
let token = auth.get_postgres_token().await?;
// or
let token = auth.get_mysql_token().await?;
// Scope: https://ossrdbms-aad.database.windows.net/.default
```

### Other Azure Resources

For other resources, use `get_token()` with custom scopes:

```rust
// Azure Storage
let token = auth.get_token(&["https://storage.azure.com/.default"]).await?;

// Azure Key Vault
let token = auth.get_token(&["https://vault.azure.net/.default"]).await?;
```

## Authentication Flow

### DefaultAzureCredential

When using `DefaultAzureCredential` (via `get_*_token_with_default_credential()` functions), the following authentication methods are tried in order:

1. **Environment Variables** - Service principal credentials:
   - `AZURE_TENANT_ID`: Azure AD tenant ID
   - `AZURE_CLIENT_ID`: Application (client) ID
   - `AZURE_CLIENT_SECRET`: Client secret

2. **Managed Identity** - Automatically used when running in Azure:
   - System-assigned identity
   - User-assigned identity (if `AZURE_CLIENT_ID` is set)

3. **Azure CLI** - Uses credentials from `az login`

4. **Azure PowerShell** - Uses credentials from `Connect-AzAccount`

5. **Interactive Browser** - Fallback for interactive scenarios

### Managed Identity with User-Assigned Identity

For user-assigned managed identities, set the client ID:

```bash
export AZURE_CLIENT_ID="your-user-assigned-identity-client-id"
```

Then use the default credential functions - they'll automatically use the specified identity.

## Integration Examples

### PostgreSQL Stored Procedure Reaction

```rust
use drasi_auth_azure::get_postgres_token_with_default_credential;
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;

// Get Azure AD token
let token = get_postgres_token_with_default_credential().await?;

let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("server.postgres.database.azure.com")
    .with_database("mydb")
    // For local dev (az login): use your email
    // For managed identity: use the identity name
    .with_user("user@domain.com")
    .with_aad_token(&token)
    .with_ssl(true)
    .build()
    .await?;
```

### MySQL Stored Procedure Reaction

```rust
use drasi_auth_azure::get_mysql_token_with_default_credential;
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;

// Get Azure AD token
let token = get_mysql_token_with_default_credential().await?;

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("server.mysql.database.azure.com")
    .with_database("mydb")
    .with_user("user@domain.com")
    .with_aad_token(&token)
    .with_ssl(true)
    .with_cleartext_plugin(true)  // Required for Azure AD
    .build()
    .await?;
```

### Azure SQL Stored Procedure Reaction

```rust
use drasi_auth_azure::get_sql_token_with_default_credential;
use drasi_reaction_storedproc_mssql::MsSqlStoredProcReaction;

// Get Azure AD token
let token = get_sql_token_with_default_credential().await?;

let reaction = MsSqlStoredProcReaction::builder("my-reaction")
    .with_hostname("server.database.windows.net")
    .with_database("mydb")
    .with_user("user@domain.com")
    .with_aad_token(&token)
    .with_ssl(true)
    .build()
    .await?;
```

## Features

- **Simple API**: Easy-to-use helper functions for common scenarios
- **Type-safe**: Uses Rust enums for different credential types
- **Async-first**: All operations are async
- **Flexible**: Supports DefaultAzureCredential and Service Principal flows
- **Database-optimized**: Specialized functions for PostgreSQL, MySQL, and Azure SQL
- **Serializable**: Can be configured via JSON/YAML

## Best Practices

- **Use DefaultAzureCredential** for most scenarios - it automatically handles managed identity, Azure CLI, and more
- **For local development**: Use `az login` to authenticate with your Azure AD account
- **For production in Azure**: Enable Managed Identity on your Azure resource (App Service, AKS, VM, etc.)
- **For CI/CD**: Use Service Principal with environment variables or the service principal helper functions
- **Username format**:
  - Local development (Azure CLI): Use your email address (e.g., `user@domain.com`)
  - Managed Identity: Use the identity name (e.g., `my-app-identity`)

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
