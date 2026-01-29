# MS SQL Server Stored Procedure Reaction

A Drasi reaction plugin that invokes MS SQL Server stored procedures when continuous query results change.

## Overview

The MS SQL Server Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Map query result fields to stored procedure parameters using `@fieldName` syntax
- Handle multiple queries with a single reaction
- Automatically retry failed procedure calls with exponential backoff
- Configure connection parameters and timeouts

## Installation

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
drasi-reaction-storedproc-mssql = { path = "path/to/drasi-core/components/reactions/storedproc-mssql" }
```

## Quick Start

### 1. Create Stored Procedures in MS SQL Server

```sql
CREATE PROCEDURE add_user
    @p_id INT,
    @p_name NVARCHAR(255),
    @p_email NVARCHAR(255)
AS
BEGIN
    INSERT INTO users_sync (id, name, email)
    VALUES (@p_id, @p_name, @p_email);
END;
GO

CREATE PROCEDURE update_user
    @p_id INT,
    @p_name NVARCHAR(255),
    @p_email NVARCHAR(255)
AS
BEGIN
    UPDATE users_sync
    SET name = @p_name, email = @p_email
    WHERE id = @p_id;
END;
GO

CREATE PROCEDURE delete_user
    @p_id INT
AS
BEGIN
    DELETE FROM users_sync WHERE id = @p_id;
END;
GO
```

### 2. Create the Reaction

```rust
use drasi_reaction_storedproc_mssql::MsSqlStoredProcReaction;
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let reaction = MsSqlStoredProcReaction::builder("user-sync")
        .with_connection(
            "localhost",
            1433,
            "mydb",
            "sa",
            "YourPassword123!"
        )
        .with_query("user-changes")
        .with_added_command("EXEC add_user @id, @name, @email")
        .with_updated_command("EXEC update_user @id, @name, @email")
        .with_deleted_command("EXEC delete_user @id")
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

#### Traditional Username/Password Authentication

```rust
let reaction = MsSqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(1433)
    .with_database("mydb")
    .with_user("sa")
    .with_password("YourPassword123!")
    .with_ssl(true)  // Enable TLS encryption
    .with_query("query1")
    .with_added_command("EXEC add_record @id, @name")
    .with_updated_command("EXEC update_record @id, @name")
    .with_deleted_command("EXEC delete_record @id")
    .with_command_timeout_ms(30000)
    .with_retry_attempts(3)
    .build()
    .await?;
```

#### Cloud Identity Provider Authentication

For cloud-managed SQL Server databases, you can use identity providers instead of passwords:

**Azure AD Authentication (Azure SQL Database/Managed Instance):**

```rust
use drasi_lib::identity::AzureIdentityProvider;

// For Azure Kubernetes Service with Workload Identity
let identity_provider = AzureIdentityProvider::with_workload_identity("myuser@myserver")?;

// For local development or Azure VMs with Managed Identity
let identity_provider = AzureIdentityProvider::with_default_credentials("myuser@myserver").await?;

let reaction = MsSqlStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.database.windows.net")
    .with_port(1433)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_ssl(true)  // Required for Azure
    .with_query("query1")
    .with_added_command("EXEC add_record @id, @name")
    .with_updated_command("EXEC update_record @id, @name")
    .with_deleted_command("EXEC delete_record @id")
    .build()
    .await?;
```

**Password Provider (programmatic username/password):**

```rust
use drasi_lib::identity::PasswordIdentityProvider;

let identity_provider = PasswordIdentityProvider::new("sa", "YourPassword123!");

let reaction = MsSqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(1433)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_query("query1")
    .with_added_command("EXEC add_record @id, @name")
    .with_updated_command("EXEC update_record @id, @name")
    .with_deleted_command("EXEC delete_record @id")
    .build()
    .await?;
```

> **Note:** When using identity providers, do not call `.with_user()` or `.with_password()`. The identity provider handles authentication automatically.
>
> AWS RDS for SQL Server does not currently support IAM authentication. For AWS-hosted SQL Server instances, use traditional username/password authentication.
>
> See the [Identity Provider README](../../../lib/src/identity/README.md) for detailed setup instructions for Azure AD authentication.

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `hostname` | Database hostname | `String` | `"localhost"` |
| `port` | Database port | `u16` | `1433` |
| `user` | Database user | `String` | Required |
| `password` | Database password | `String` | Required |
| `database` | Database name | `String` | Required |
| `ssl` | Enable TLS encryption | `bool` | `false` |
| `added_command` | Procedure for ADD operations | `Option<String>` | `None` |
| `updated_command` | Procedure for UPDATE operations | `Option<String>` | `None` |
| `deleted_command` | Procedure for DELETE operations | `Option<String>` | `None` |
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

## Parameter Mapping

Use the `@fieldName` syntax in your command to reference fields from query results. The reaction will automatically map them to MS SQL parameters:

```rust
.with_added_command("EXEC add_user @id, @name, @email")
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
EXEC add_user @p1=1, @p2='Alice', @p3='alice@example.com'
```

**Note**: The `@fieldName` in your command is a placeholder syntax. The actual SQL uses `@p1, @p2, @p3` parameter names internally.

### Nested Field Access

```rust
.with_added_command("EXEC add_address @user.id, @address.city")
```

## Error Handling

The reaction includes automatic retry logic with exponential backoff:
- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

## Security Considerations

- **Use strong passwords**: The default `sa` account should have a strong password
- **Enable encryption**: Use `.with_ssl(true)` for production deployments
- **Self-signed certificates**: The reaction automatically trusts self-signed certificates

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
