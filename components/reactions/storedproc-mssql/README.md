# MS SQL Server Stored Procedure Reaction

A Drasi reaction plugin that invokes MS SQL Server stored procedures when continuous query results change.

## Overview

The MS SQL Server Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Bind query result fields to stored procedure parameters using Handlebars `{{param ...}}` templates
- Handle multiple queries with a single reaction
- Automatically retry failed procedure calls with exponential backoff
- Configure connection parameters and timeouts

## Installation

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
drasi-reaction-storedproc-mssql = { path = "path/to/drasi-core/components/reactions/storedproc-mssql" }
```

### TLS backend

Connecting to SQL Server with `ssl = true` (`.with_ssl(true)`) requires a TLS
backend. This crate exposes two mutually exclusive Cargo features, with
`native-tls` enabled by default:

| Feature      | Backend                                   | Default |
|--------------|-------------------------------------------|---------|
| `native-tls` | platform TLS (OpenSSL / Secure Transport) | ✅       |
| `rustls`     | rustls                                    |         |

The default works out of the box on Linux. Use rustls instead when the platform
TLS stack rejects the server handshake — notably **macOS**, whose Secure
Transport stack fails the TLS handshake against SQL Server 2022:

```toml
[dependencies]
drasi-reaction-storedproc-mssql = { path = "...", default-features = false, features = ["rustls"] }
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
        .with_added_command("EXEC add_user {{param after.id}}, {{param after.name}}, {{param after.email}}")
        .with_updated_command("EXEC update_user {{param after.id}}, {{param after.name}}, {{param after.email}}")
        .with_deleted_command("EXEC delete_user {{param before.id}}")
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
    .with_added_command("EXEC add_record {{param after.id}}, {{param after.name}}")
    .with_updated_command("EXEC update_record {{param after.id}}, {{param after.name}}")
    .with_deleted_command("EXEC delete_record {{param before.id}}")
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
let identity_provider = AzureIdentityProvider::with_default_credentials("myuser@myserver")?;

let reaction = MsSqlStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.database.windows.net")
    .with_port(1433)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_ssl(true)  // Required for Azure
    .with_query("query1")
    .with_added_command("EXEC add_record {{param after.id}}, {{param after.name}}")
    .with_updated_command("EXEC update_record {{param after.id}}, {{param after.name}}")
    .with_deleted_command("EXEC delete_record {{param before.id}}")
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
    .with_added_command("EXEC add_record {{param after.id}}, {{param after.name}}")
    .with_updated_command("EXEC update_record {{param after.id}}, {{param after.name}}")
    .with_deleted_command("EXEC delete_record {{param before.id}}")
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
| `default_template` | Command templates applied to all queries unless overridden | `Option<QueryConfig>` | `None` |
| `routes` | Per-query command templates (keyed by query id) | `Map<String, QueryConfig>` | `{}` |
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

Each `QueryConfig` holds optional `added`, `updated`, and `deleted` command
templates. The `with_added_command` / `with_updated_command` /
`with_deleted_command` builder setters populate the `default_template`.

## Templating

Commands are [Handlebars](https://handlebarsjs.com/) templates that render to a
SQL batch (typically an `EXEC` statement). Instead of interpolating values into
the SQL text, use the `{{param <path>}}` helper to bind each value as a
positional SQL parameter through the driver:

```rust
.with_added_command("EXEC add_user {{param after.id}}, {{param after.name}}, {{param after.email}}")
```

Given the query result row:

```json
{
  "id": 1,
  "name": "Alice",
  "email": "alice@example.com"
}
```

the reaction renders and executes:

```sql
EXEC add_user @P1, @P2, @P3   -- bound: @P1=1, @P2='Alice', @P3='alice@example.com'
```

Because every value is bound as a parameter, contents such as quotes and
semicolons are stored verbatim and cannot alter the executed SQL.

### Template context

Templates have access to the following keys:

- `after` — the row after the change (ADD and UPDATE)
- `before` — the row before the change (UPDATE and DELETE)
- `data` — the raw data field (UPDATE)
- `query_name` / `query_id` — the id of the query that produced the result
- `operation` — the operation type (`"ADD"`, `"UPDATE"`, or `"DELETE"`)
- `timestamp` — the result timestamp (RFC3339)
- `metadata` — the result metadata map

Rendering runs in **strict mode**: a template that references a field absent from
the current row fails to render and the event is skipped rather than executing a
command with missing data.

### Helpers

- `{{param <path>}}` binds the value at `<path>` as a positional SQL parameter and
  renders the matching placeholder.
- `{{json <path>}}` renders the value at `<path>` as a JSON string. Combine it with
  `param` to bind a whole object, e.g. `EXEC ingest {{param (json after)}}`.

### Nested field access

```rust
.with_added_command("EXEC add_address {{param after.user.id}}, {{param after.address.city}}")
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

## Plugin Packaging

This reaction is compiled as a dynamic plugin (cdylib) that can be loaded by drasi-server at runtime.

**Key files:**
- `Cargo.toml` — includes `crate-type = ["lib", "cdylib"]`
- `src/descriptor.rs` — implements `ReactionPluginDescriptor` with kind `"storedproc-mssql"`, configuration DTO, and OpenAPI schema generation
- `src/lib.rs` — invokes `drasi_plugin_sdk::export_plugin!` to export the plugin entry point

**Building:**
```bash
cargo build -p drasi-reaction-storedproc-mssql
```

The compiled `.so` (Linux) / `.dylib` (macOS) / `.dll` (Windows) is placed in `target/debug/` and can be copied to the server's `plugins/` directory.

For more details on the plugin descriptor pattern and configuration DTOs, see the [Reaction Developer Guide](../README.md#packaging-as-a-dynamic-plugin).

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
