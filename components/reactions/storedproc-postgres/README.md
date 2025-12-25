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
use drasi_reaction_storedproc_postgres::PostgresStoredProcReaction;
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
        .with_added_command("CALL add_user(@id, @name, @email)")
        .with_updated_command("CALL update_user(@id, @name, @email)")
        .with_deleted_command("CALL delete_user(@id)")
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
    .with_added_command("CALL add_record(@id, @name)")
    .with_updated_command("CALL update_record(@id, @name)")
    .with_deleted_command("CALL delete_record(@id)")
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
| `added_command` | Procedure for ADD operations | `Option<String>` | `None` |
| `updated_command` | Procedure for UPDATE operations | `Option<String>` | `None` |
| `deleted_command` | Procedure for DELETE operations | `Option<String>` | `None` |
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

## Parameter Mapping

Use the `@fieldName` syntax to reference fields from query results:

```rust
.with_added_command("CALL add_user(@id, @name, @email)")
```

Query result:
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
    .build()
    .await?;
```

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

## Error Handling

The reaction includes automatic retry logic with exponential backoff:
- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
