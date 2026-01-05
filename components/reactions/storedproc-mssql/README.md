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

# For Azure AD authentication support:
drasi-auth-azure = { path = "path/to/drasi-core/components/auth/azure" }
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

## Authentication

The MS SQL Server Stored Procedure reaction supports two authentication methods:

### Password Authentication (Traditional)

Use username and password credentials:

```rust
let reaction = MsSqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(1433)
    .with_database("mydb")
    .with_user("sa")
    .with_password("YourPassword123!")
    .build()
    .await?;
```

### Azure AD Authentication with Default Credentials

For Azure SQL Database, you can use Azure AD authentication with `DefaultAzureCredential`. This method automatically tries multiple authentication mechanisms in order:

1. **Environment variables** (Service Principal) - for CI/CD
2. **Managed Identity** - for production in Azure
3. **Azure CLI** (`az login`) - for local development
4. **Azure PowerShell**
5. **Interactive browser**

#### Azure Portal Setup

1. **Configure Azure AD Admin:**
   - Navigate to: Azure Portal → SQL Server → Settings → Azure Active Directory admin
   - Click "Set admin"
   - Add your Azure AD account
   - Wait 1-2 minutes for propagation

2. **Configure Firewall Rules:**
   - Navigate to: Azure Portal → SQL Server → Security → Networking
   - Add your client IP address to the firewall rules
   - For local development, add your current IP address
   - For production, add your Azure resource's IP range or enable "Allow Azure services and resources to access this server"

3. **No additional SQL setup needed** - Azure AD admins automatically have full access

#### Code Configuration

```rust
use drasi_auth_azure::get_sql_token_with_default_credential;
use drasi_reaction_storedproc_mssql::MsSqlStoredProcReaction;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Get Azure AD token
    let token = get_sql_token_with_default_credential().await?;

    let reaction = MsSqlStoredProcReaction::builder("my-reaction")
        .with_hostname("server.database.windows.net")
        .with_port(1433)
        .with_database("drasi_test")
        // Username format depends on authentication method:
        // - Local dev (az login): use your email (e.g., "user@<domain>.com")
        // - Managed Identity: use the identity name (e.g., "my-app-identity")
        .with_user("user@<domain>.com")
        .with_aad_token(&token)           // Use token instead of password
        .with_ssl(true)                   // Required for Azure SQL
        .with_query("my-query")
        .with_added_command("EXEC add_record @id")
        .with_updated_command("EXEC update_record @id")
        .with_deleted_command("EXEC delete_record @id")
        .build()
        .await?;

    // ... rest of your application
    Ok(())
}
```

**Key Requirements:**
- Use `.with_aad_token(&token)` instead of `.with_password()`
- Username format depends on your authentication method (see below)
- SSL is automatically enabled for Azure SQL
- Token scope: `https://database.windows.net/.default`
- Requires `tiberius` with `rustls` feature

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
- Ensure your Azure AD account is added as an admin in the SQL Server

#### Production Deployment with Managed Identity

When running in Azure (App Service, AKS, VM, Container Apps, etc.):

1. Enable Managed Identity on your Azure resource
2. Grant the Managed Identity access to your SQL Server as an Azure AD admin
3. Use the **Managed Identity name** as the username (e.g., `my-app-identity`)
4. `DefaultAzureCredential` automatically uses Managed Identity - no code changes needed
5. No secrets to manage or rotate

**Important:** When using Managed Identity:
- Use the **identity name** as the username, not an email address
- System-assigned identities typically use the resource name
- User-assigned identities use the identity resource name
- Example: `.with_user("my-app-identity")`

#### CI/CD with Service Principal

For automated deployments, use environment variables:

```bash
export AZURE_TENANT_ID="your-tenant-id"
export AZURE_CLIENT_ID="your-client-id"
export AZURE_CLIENT_SECRET="your-client-secret"
```

#### Troubleshooting

**"Access denied" Error**
- Verify you're added as Azure AD admin in Azure Portal
- Wait 1-2 minutes for changes to propagate
- Ensure using correct username format (email for local dev, identity name for managed identity)

**"Failed to get token"**
```bash
# Login to Azure CLI
az login

# Verify subscription
az account show

# If needed, set the correct subscription
az account set --subscription "subscription-name"
```

**"TLS encryption is not enabled"**
- Ensure you have the `rustls` feature enabled for `tiberius` dependency
- Update `Cargo.toml`: `tiberius = { version = "0.12", features = ["rustls"] }`

**Firewall Errors**
- Verify your IP address is added to the SQL Server firewall rules
- For Azure services, enable "Allow Azure services and resources to access this server"

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
