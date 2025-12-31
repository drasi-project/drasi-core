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
<<<<<<< HEAD
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;
=======
use drasi_reaction_storedproc_postgres::{PostgresStoredProcReaction, QueryConfig, TemplateSpec};
>>>>>>> feature-lib
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
<<<<<<< HEAD
        .with_added_command("CALL add_user(@id, @name, @email)")
        .with_updated_command("CALL update_user(@id, @name, @email)")
        .with_deleted_command("CALL delete_user(@id)")
=======
        .with_default_template(QueryConfig {
            added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
            updated: Some(TemplateSpec::new("CALL update_user(@after.id, @after.name, @after.email)")),
            deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
        })
>>>>>>> feature-lib
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
<<<<<<< HEAD
    .with_added_command("CALL add_record(@id, @name)")
    .with_updated_command("CALL update_record(@id, @name)")
    .with_deleted_command("CALL delete_record(@id)")
=======
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL add_record(@after.id, @after.name)")),
        updated: Some(TemplateSpec::new("CALL update_record(@after.id, @after.name)")),
        deleted: Some(TemplateSpec::new("CALL delete_record(@before.id)")),
    })
>>>>>>> feature-lib
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
<<<<<<< HEAD
| `added_command` | Procedure for ADD operations | `Option<String>` | `None` |
| `updated_command` | Procedure for UPDATE operations | `Option<String>` | `None` |
| `deleted_command` | Procedure for DELETE operations | `Option<String>` | `None` |
=======
| `default_template` | Default templates for all queries | `Option<QueryConfig>` | `None` |
| `routes` | Query-specific template overrides | `HashMap<String, QueryConfig>` | Empty |
>>>>>>> feature-lib
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

## Parameter Mapping

<<<<<<< HEAD
Use the `@fieldName` syntax to reference fields from query results:

```rust
.with_added_command("CALL add_user(@id, @name, @email)")
```

Query result:
=======
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
>>>>>>> feature-lib
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

<<<<<<< HEAD
```rust
.with_added_command("CALL add_address(@user.id, @address.city)")
```

## Azure Identity Authentication

For Azure Database for PostgreSQL, you can use Azure Identity (Managed Identity, Service Principal, etc.) instead of password authentication.

### Prerequisites

Add the optional `azure` feature and the auth crate:

```toml
[dependencies]
drasi-reaction-storedproc-postgres = { path = "...", features = ["azure"] }
drasi-auth-azure = { path = "..." }
```

### Using Managed Identity

```rust
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;
use drasi_auth_azure::AzureIdentityAuth;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Get an access token using Azure Managed Identity
    let azure_auth = AzureIdentityAuth::managed_identity();
    let token = azure_auth.get_postgres_token().await?;

    // Use the token as the password
    let reaction = PostgresStoredProcReaction::builder("user-sync")
        .with_hostname("myserver.postgres.database.azure.com")
        .with_port(5432)
        .with_database("mydb")
        .with_user("myuser@myserver")  // Format: identity-name@server-name
        .with_password(token)  // Pass the token as password
        .with_ssl(true)  // SSL required for Azure
        .with_query("user-changes")
        .with_added_command("CALL add_user(@id, @name, @email)")
        .build()
        .await?;

    // ... rest of setup
    Ok(())
}
```

### Using Service Principal

```rust
use drasi_auth_azure::AzureIdentityAuth;

// Get token using Service Principal
let azure_auth = AzureIdentityAuth::service_principal(
    "tenant-id",
    "client-id",
    "client-secret"
);
let token = azure_auth.get_postgres_token().await?;

// Use token as password in reaction builder
let reaction = PostgresStoredProcReaction::builder("user-sync")
    .with_password(token)
    // ... rest of configuration
=======
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
>>>>>>> feature-lib
    .build()
    .await?;
```

<<<<<<< HEAD
### Convenience Helper Functions

The `drasi-auth-azure` crate provides convenient helper functions:

```rust
use drasi_auth_azure::get_postgres_token_with_managed_identity;

// Quick way to get a token with managed identity
let token = get_postgres_token_with_managed_identity(None).await?;

// Or with a specific client ID for user-assigned managed identity
let token = get_postgres_token_with_managed_identity(
    Some("my-client-id".to_string())
).await?;
```

### Important Notes

- When using Azure Identity, SSL must be enabled (`.with_ssl(true)`)
- The username format should be: `identity-name@server-name`
- For Managed Identity: the identity name is typically the VM/App Service name
- Tokens expire and need to be refreshed periodically (consider implementing token refresh logic for long-running applications)
=======
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
>>>>>>> feature-lib

## Error Handling

The reaction includes automatic retry logic with exponential backoff:
- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
