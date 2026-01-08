# PostgreSQL Stored Procedure Reaction

A Drasi reaction plugin that invokes PostgreSQL stored procedures when continuous query results change.

## Overview

The PostgreSQL Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Map query result fields to stored procedure parameters using `@fieldName` syntax
- Handle multiple queries with a single reaction
- Automatically retry failed procedure calls with exponential backoff
- Configure connection parameters and timeouts

## Installation

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
drasi-reaction-storedproc-postgres = { path = "path/to/drasi-core/components/reactions/storedproc-postgres" }

# For Azure AD authentication support:
drasi-auth-azure = { path = "path/to/drasi-core/components/auth/azure" }
```

## Quick Start

### 1. Create Stored Procedures in PostgreSQL

```sql
CREATE OR REPLACE PROCEDURE add_user(
    p_id INTEGER,
    p_name TEXT,
    p_email TEXT
)
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO users_sync (id, name, email)
    VALUES (p_id, p_name, p_email);
END;
$$;
```

### 2. Create the Reaction

```rust
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let reaction = PostgresStoredProcReaction::builder("user-sync")
        .with_connection(
            "localhost",
            5432,
            "mydb",
            "postgres",
            "password"
        )
        .with_query("user-changes")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
            updated: Some(TemplateSpec::new("CALL update_user(@after.id, @after.name, @after.email)")),
            deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
        })
        .build()
        .await?;

    let drasi = DrasiLib::builder()
        .with_id("my-app")
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;
    tokio::signal::ctrl_c().await?;

    Ok(())
}
```

## Configuration

### Builder API

```rust
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_user("postgres")
    .with_password("secret")
    .with_ssl(true)  // Enable SSL/TLS
    .with_query("query1")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL add_record(@after.id, @after.name)")),
        updated: Some(TemplateSpec::new("CALL update_record(@after.id, @after.name)")),
        deleted: Some(TemplateSpec::new("CALL delete_record(@before.id)")),
    })
    .with_command_timeout_ms(30000)
    .with_retry_attempts(3)
    .build()
    .await?;
```

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `hostname` | Database hostname | `String` | `"localhost"` |
| `port` | Database port | `u16` | `5432` |
| `user` | Database user | `String` | Required |
| `password` | Database password | `String` | Required |
| `database` | Database name | `String` | Required |
| `ssl` | Enable SSL/TLS | `bool` | `false` |
| `default_template` | Default templates for all queries | `Option<QueryConfig>` | `None` |
| `routes` | Query-specific template overrides | `HashMap<String, QueryConfig>` | Empty |
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

## Authentication

The PostgreSQL Stored Procedure reaction supports two authentication methods:

### Password Authentication (Traditional)

Use username and password credentials:

```rust
let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_user("postgres")
    .with_password("secret")
    .build()
    .await?;
```

### Azure AD Authentication with Default Credentials

For Azure Database for PostgreSQL, you can use Azure AD authentication with `DefaultAzureCredential`. This method automatically tries multiple authentication mechanisms in order:

1. **Managed Identity** - for production in Azure
2. **Azure CLI** (`az login`) - for local development
3. **Azure PowerShell**
4. **Interactive browser**

#### Azure Portal Setup

1. **Configure Azure AD Admin:**
   - Navigate to: Azure Portal → PostgreSQL Server → Settings → Authentication
   - Set authentication method to "Azure AD authentication only" or "PostgreSQL and Azure AD authentication"
   - Add your Azure AD account as admin

2. **Configure Firewall Rules:**
   - Navigate to: Azure Portal → PostgreSQL Server → Settings → Networking
   - Add your client IP address to the firewall rules
   - For local development, add your current IP address
   - For production, add your Azure resource's IP range or enable "Allow Azure services and resources to access this server"

3. **No additional SQL setup needed** - Azure AD admins automatically have full access

#### Code Configuration

```rust
use drasi_reaction_storedproc_postgres::azure_auth::get_postgres_aad_token;
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Get Azure AD token using the component helper
    let token = get_postgres_aad_token().await?;

    let reaction = PostgresStoredProcReaction::builder("my-reaction")
        .with_hostname("server.postgres.database.azure.com")
        .with_port(5432)
        .with_database("drasi_test")
        // Username format depends on authentication method:
        // - Local dev (az login): use your email (e.g., "user@<domain>.com")
        // - Managed Identity: use the identity name (e.g., "my-app-identity")
        .with_user("user@<domain>.com")
        .with_aad_token(&token)             // Use token instead of password
        .with_ssl(true)                     // Required for Azure PostgreSQL
        .with_query("my-query")
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_record(@after.id)")),
            updated: None,
            deleted: None,
        })
        .build()
        .await?;

    // ... rest of your application
    Ok(())
}
```

**Key Requirements:**
- Use `.with_aad_token(&token)` instead of `.with_password()`
- Username format depends on your authentication method (see below)
- SSL must be enabled with `.with_ssl(true)`
- The helper automatically uses the correct OAuth scope: `https://ossrdbms-aad.database.windows.net/.default`

#### Alternative: Service Principal Authentication

For CI/CD scenarios or when you need explicit service principal credentials:

```rust
use drasi_reaction_storedproc_postgres::azure_auth::get_postgres_aad_token_with_service_principal;

let token = get_postgres_aad_token_with_service_principal(
    "tenant-id",
    "client-id",
    "client-secret"
).await?;

// Use the token with the builder as shown above
```

#### Local Development with Azure CLI

The easiest way for local development is using Azure CLI:

```bash
# Login to Azure CLI with your Azure AD account
az login

# Verify you're logged in
az account show
```

**Important:** When using `az login` for local development:
- Use your **full email address** as the username (e.g., `user@<domain>.com`)
- `DefaultAzureCredential` will automatically use your Azure CLI credentials
- Ensure your Azure AD account is added as an admin in the PostgreSQL server

#### Production Deployment with Managed Identity

When running in Azure (App Service, AKS, VM, Container Apps, etc.):

1. Enable Managed Identity on your Azure resource
2. Grant the Managed Identity access to your PostgreSQL server as an Azure AD admin
3. Use the **Managed Identity name** as the username (e.g., `my-app-identity`)
4. `DefaultAzureCredential` automatically uses Managed Identity - no code changes needed
5. No secrets to manage or rotate

**Important:** When using Managed Identity:
- Use the **identity name** as the username, not an email address
- System-assigned identities typically use the resource name
- User-assigned identities use the identity resource name
- Example: `.with_user("my-app-identity")`

#### Troubleshooting

**"Access denied" Error**
- Verify you're added as Azure AD admin in Azure Portal
- Wait 1-2 minutes for changes to propagate
- Ensure using correct username format (full email)

**"Failed to get token"**
```bash
# Login to Azure CLI
az login

# Verify subscription
az account show

# If needed, set the correct subscription
az account set --subscription "subscription-name"
```

**SSL Connection Errors**
- Ensure `.with_ssl(true)` is set
- Azure PostgreSQL requires SSL/TLS connections

## Parameter Mapping

Templates use the `@` syntax to reference fields from query results. The reaction provides different data contexts based on the operation type:

- **ADD operations**: Use `@after.field` to access the new data
- **UPDATE operations**: Use `@after.field` for new data, `@before.field` for old data
- **DELETE operations**: Use `@before.field` to access the deleted data

### Example

```rust
.with_default_template(QueryConfig {
    added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
    updated: Some(TemplateSpec::new("CALL update_user(@after.id, @after.name, @after.email)")),
    deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
})
```

Query result for ADD operation:
```json
{
  "id": 1,
  "name": "Alice",
  "email": "alice@example.com"
}
```

Executes:
```sql
CALL add_user(1, 'Alice', 'alice@example.com')
```

### Nested Field Access

Access nested fields using dot notation:

```rust
TemplateSpec::new("CALL add_address(@after.user.id, @after.address.city)")
```

### Per-Query Templates

You can configure different templates for specific queries using the `routes` field or the builder's `with_route` method:

```rust
use std::collections::HashMap;

let mut routes = HashMap::new();
routes.insert("user-query".to_string(), QueryConfig {
    added: Some(TemplateSpec::new("CALL user_added(@after.id, @after.name)")),
    updated: None,
    deleted: None,
});

let reaction = PostgresStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_database("mydb")
    .with_user("postgres")
    .with_password("secret")
    .with_query("user-query")
    .with_route("user-query", QueryConfig {
        added: Some(TemplateSpec::new("CALL user_added(@after.id, @after.name)")),
        ..Default::default()  // updated and deleted will use default template
    })
    .build()
    .await?;
```

## Advanced Example: Partial Route Overrides

This example shows how to override only specific operations for a query while falling back to defaults for others:

```rust
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};

let reaction = PostgresStoredProcReaction::builder("multi-query-sensor-sync")
    .with_hostname("localhost")
    .with_port(5432)
    .with_database("drasi_test")
    .with_user("postgres")
    .with_password("mysecret")
    // Subscribe to multiple queries
    .with_query("high-temp")
    .with_query("low-temp")
    .with_query("critical-temp")
    // Default template - applies to "high-temp" and "low-temp"
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL log_sensor_added(@after.id, @after.temperature, @after.timestamp)"
        )),
        updated: Some(TemplateSpec::new(
            "CALL log_sensor_updated(@after.id, @after.temperature)"
        )),
        deleted: Some(TemplateSpec::new(
            "CALL log_sensor_deleted(@before.id)"
        )),
    })
    // Custom route for critical temperature readings
    // Only handles ADD operations, falls back to default for UPDATE/DELETE
    .with_route("critical-temp", QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL log_critical_alert(@after.id, @after.temperature, @after.timestamp)"
        )),
        ..Default::default()  // updated and deleted will use default template
    })
    .with_command_timeout_ms(5000)
    .with_retry_attempts(3)
    .build()
    .await?;
```

**How it works:**

- **"high-temp" and "low-temp" queries** → Use default template for all operations
- **"critical-temp" query**:
  - ADD: `CALL log_critical_alert(...)` *(custom route)*
  - UPDATE: `CALL log_sensor_updated(...)` *(falls back to default)*
  - DELETE: `CALL log_sensor_deleted(...)` *(falls back to default)*

## Advanced Example: Multiple Queries with Default and Custom Routes

This example shows a reaction handling multiple queries with different stored procedure requirements:

```rust
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};

// Create a reaction that:
// 1. Subscribes to 3 different queries: "user-changes", "product-changes", "order-changes"
// 2. Has a default template for most operations
// 3. Overrides only the "product-changes" query with custom procedures

let reaction = PostgresStoredProcReaction::builder("multi-query-sync")
    .with_hostname("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_user("postgres")
    .with_password("secret")
    // Subscribe to multiple queries
    .with_query("user-changes")
    .with_query("product-changes")
    .with_query("order-changes")
    // Default template applies to "user-changes" and "order-changes"
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL log_entity_added(@after.id, @after.type)")),
        updated: Some(TemplateSpec::new("CALL log_entity_updated(@after.id, @after.type)")),
        deleted: Some(TemplateSpec::new("CALL log_entity_deleted(@before.id, @before.type)")),
    })
    // Override "product-changes" with specific procedures
    .with_route("product-changes", QueryConfig {
        added: Some(TemplateSpec::new(
            "CALL sync_product_added(@after.product_id, @after.name, @after.price, @after.inventory)"
        )),
        updated: Some(TemplateSpec::new(
            "CALL sync_product_updated(@after.product_id, @after.price, @after.inventory)"
        )),
        ..Default::default()  // deleted will fall back to default template
    })
    .with_command_timeout_ms(5000)
    .with_retry_attempts(3)
    .build()
    .await?;
```

**How it works:**

1. **"user-changes" query** → Uses default template
   - Add: `CALL log_entity_added(@after.id, @after.type)`
   - Update: `CALL log_entity_updated(@after.id, @after.type)`
   - Delete: `CALL log_entity_deleted(@before.id, @before.type)`

2. **"product-changes" query** → Uses custom route (with fallback to default for delete)
   - Add: `CALL sync_product_added(@after.product_id, @after.name, @after.price, @after.inventory)`
   - Update: `CALL sync_product_updated(@after.product_id, @after.price, @after.inventory)`
   - Delete: `CALL log_entity_deleted(@before.id, @before.type)` *(falls back to default)*

3. **"order-changes" query** → Uses default template
   - Add: `CALL log_entity_added(@after.id, @after.type)`
   - Update: `CALL log_entity_updated(@after.id, @after.type)`
   - Delete: `CALL log_entity_deleted(@before.id, @before.type)`

**Note:** If a route specifies `None` for an operation (like `deleted: None` for product-changes), the reaction will check the default template. If the default template also has `None` for that operation, no procedure will be called.

## Error Handling

The reaction includes automatic retry logic with exponential backoff:
- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
