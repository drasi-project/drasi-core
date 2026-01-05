# MySQL Stored Procedure Reaction

A Drasi reaction plugin that invokes MySQL stored procedures when continuous query results change.

## Overview

The MySQL Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Map query result fields to stored procedure parameters using `@after.fieldName` and `@before.fieldName` syntax
- Configure default templates for all queries or per-query routes for specific queries
- Handle multiple queries with different stored procedure configurations
- Automatically retry failed procedure calls with exponential backoff
- Configure connection pooling and timeouts

## Installation

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
drasi-reaction-storedproc-mysql = { path = "path/to/drasi-core/components/reactions/storedproc-mysql" }

# For Azure AD authentication support:
drasi-auth-azure = { path = "path/to/drasi-core/components/auth/azure" }
```

## Quick Start

### 1. Create Stored Procedures in MySQL

```sql
DELIMITER //

CREATE PROCEDURE add_user(
    IN p_id INT,
    IN p_name VARCHAR(255),
    IN p_email VARCHAR(255)
)
BEGIN
    INSERT INTO users_sync (id, name, email)
    VALUES (p_id, p_name, p_email);
END //

CREATE PROCEDURE update_user(
    IN p_id INT,
    IN p_name VARCHAR(255),
    IN p_email VARCHAR(255)
)
BEGIN
    UPDATE users_sync
    SET name = p_name, email = p_email
    WHERE id = p_id;
END //

CREATE PROCEDURE delete_user(
    IN p_id INT
)
BEGIN
    DELETE FROM users_sync WHERE id = p_id;
END //

DELIMITER ;
```

### 2. Create the Reaction

```rust
use drasi_reaction_storedproc_mysql::{MySqlStoredProcReaction, QueryConfig, TemplateSpec};
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let reaction = MySqlStoredProcReaction::builder("user-sync")
        .with_hostname("localhost")
        .with_port(3306)
        .with_database("mydb")
        .with_user("root")
        .with_password("password")
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

### Builder API with Default Template

Use a default template that applies to all queries:

```rust
use drasi_reaction_storedproc_mysql::{MySqlStoredProcReaction, QueryConfig, TemplateSpec};

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(3306)
    .with_database("mydb")
    .with_user("root")
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

### Builder API with Query-Specific Routes

Configure different stored procedures for different queries:

```rust
use drasi_reaction_storedproc_mysql::{MySqlStoredProcReaction, QueryConfig, TemplateSpec};

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(3306)
    .with_database("mydb")
    .with_user("root")
    .with_password("secret")
    .with_query("user-changes")
    .with_query("order-changes")
    // Default template for most queries
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL default_add(@after.id)")),
        updated: None,
        deleted: None,
    })
    // Special route for critical queries
    .with_route("order-changes", QueryConfig {
        added: Some(TemplateSpec::new("CALL process_order(@after.order_id, @after.total)")),
        updated: Some(TemplateSpec::new("CALL update_order(@after.order_id, @after.status)")),
        deleted: Some(TemplateSpec::new("CALL cancel_order(@before.order_id)")),
    })
    .build()
    .await?;
```

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `hostname` | Database hostname | `String` | `"localhost"` |
| `port` | Database port | `u16` | `3306` |
| `user` | Database user | `String` | Required |
| `password` | Database password | `String` | Required |
| `database` | Database name | `String` | Required |
| `ssl` | Enable SSL/TLS | `bool` | `false` |
| `default_template` | Default template for all queries | `Option<QueryConfig>` | `None` |
| `routes` | Per-query template configurations | `HashMap<String, QueryConfig>` | Empty |
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

## Authentication

The MySQL Stored Procedure reaction supports two authentication methods:

### Password Authentication (Traditional)

Use username and password credentials:

```rust
let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(3306)
    .with_database("mydb")
    .with_user("root")
    .with_password("secret")
    .build()
    .await?;
```

### Azure AD Authentication with Default Credentials

For Azure Database for MySQL Flexible Server, you can use Azure AD authentication with `DefaultAzureCredential`. This method automatically tries multiple authentication mechanisms in order:

1. **Environment variables** (Service Principal) - for CI/CD
2. **Managed Identity** - for production in Azure
3. **Azure CLI** (`az login`) - for local development
4. **Azure PowerShell**
5. **Interactive browser**

#### Azure Portal Setup

1. **Configure Azure AD Admin:**
   - Navigate to: Azure Portal → MySQL Flexible Server → Settings → Authentication
   - Add your Azure AD account as admin
   - Wait 1-2 minutes for propagation

2. **Configure Firewall Rules:**
   - Navigate to: Azure Portal → MySQL Flexible Server → Settings → Networking
   - Add your client IP address to the firewall rules
   - For local development, add your current IP address
   - For production, add your Azure resource's IP range or enable "Allow Azure services and resources to access this server"

3. **No additional SQL setup needed** - Azure AD admins automatically have full access

#### Code Configuration

```rust
use drasi_auth_azure::get_mysql_token_with_default_credential;
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Get Azure AD token
    let token = get_mysql_token_with_default_credential().await?;

    let reaction = MySqlStoredProcReaction::builder("my-reaction")
        .with_hostname("server.mysql.database.azure.com")
        .with_port(3306)
        .with_database("drasi_test")
        // Username format depends on authentication method:
        // - Local dev (az login): use your email (e.g., "user@<domain>.com")
        // - Managed Identity: use the identity name (e.g., "my-app-identity")
        .with_user("user@<domain>.com")
        .with_aad_token(&token)           // Use token instead of password
        .with_ssl(true)                   // Required for Azure MySQL
        .with_cleartext_plugin(true)      // Required for Azure AD authentication
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
- **Both** `.with_ssl(true)` and `.with_cleartext_plugin(true)` are required
- Token scope: `https://ossrdbms-aad.database.windows.net/.default`

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
- Ensure your Azure AD account is added as an admin in the MySQL server

#### Production Deployment with Managed Identity

When running in Azure (App Service, AKS, VM, Container Apps, etc.):

1. Enable Managed Identity on your Azure resource
2. Grant the Managed Identity access to your MySQL server as an Azure AD admin
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

**"Unknown authentication plugin 'sha256_password'"**
- Ensure you have `.with_cleartext_plugin(true)` in your configuration
- This is required for Azure AD authentication with MySQL

**SSL Connection Errors**
- Ensure both `.with_ssl(true)` and `.with_cleartext_plugin(true)` are set
- Azure MySQL requires SSL/TLS connections

**Firewall Errors**
- Verify your IP address is added to the MySQL server firewall rules
- For Azure services, enable "Allow Azure services and resources to access this server"

## Parameter Mapping

### Context-Aware Field Access

The reaction uses context-aware field mapping with `@after` and `@before` prefixes:

#### ADD Operations
Use `@after.fieldName` to access the newly added data:

```rust
QueryConfig {
    added: Some(TemplateSpec::new("CALL add_user(@after.id, @after.name, @after.email)")),
    updated: None,
    deleted: None,
}
```

Query result for ADD:
```json
{
  "type": "add",
  "data": {
    "id": 1,
    "name": "Alice",
    "email": "alice@example.com"
  }
}
```

Executes:
```sql
CALL add_user(1, 'Alice', 'alice@example.com')
```

#### UPDATE Operations
Use `@before.fieldName` for old values and `@after.fieldName` for new values:

```rust
QueryConfig {
    added: None,
    updated: Some(TemplateSpec::new("CALL update_user(@after.id, @before.email, @after.email)")),
    deleted: None,
}
```

Query result for UPDATE:
```json
{
  "type": "update",
  "data": {
    "before": {
      "id": 1,
      "email": "alice@oldmail.com"
    },
    "after": {
      "id": 1,
      "email": "alice@newmail.com"
    }
  }
}
```

Executes:
```sql
CALL update_user(1, 'alice@oldmail.com', 'alice@newmail.com')
```

#### DELETE Operations
Use `@before.fieldName` to access the deleted data:

```rust
QueryConfig {
    added: None,
    updated: None,
    deleted: Some(TemplateSpec::new("CALL delete_user(@before.id)")),
}
```

Query result for DELETE:
```json
{
  "type": "delete",
  "data": {
    "id": 1,
    "name": "Alice"
  }
}
```

Executes:
```sql
CALL delete_user(1)
```

### Nested Field Access

Access deeply nested fields using dot notation:

```rust
TemplateSpec::new("CALL add_address(@after.user.id, @after.location.city, @after.location.floor)")
```

Query result:
```json
{
  "user": {
    "id": 123
  },
  "location": {
    "city": "Seattle",
    "floor": 5
  }
}
```

Executes:
```sql
CALL add_address(123, 'Seattle', 5)
```

## Template Routing Priority

When a query result arrives, the reaction determines which stored procedure to call using the following priority:

1. **Query-specific route**: If a route exists for the query ID, use its template
2. **Default template**: If no route exists, use the default template
3. **Skip**: If neither exists for the operation type, skip processing

Example:

```rust
let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(3306)
    .with_database("mydb")
    .with_user("root")
    .with_password("secret")
    // Default template - used by most queries
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL default_add(@after.id)")),
        updated: Some(TemplateSpec::new("CALL default_update(@after.id)")),
        deleted: None,  // No default for deletes
    })
    // Special handling for critical-query
    .with_route("critical-query", QueryConfig {
        added: Some(TemplateSpec::new("CALL critical_add(@after.id, @after.priority)")),
        updated: None,  // Falls back to default_update
        deleted: Some(TemplateSpec::new("CALL critical_delete(@before.id)")),
    })
    .with_query("normal-query")
    .with_query("critical-query")
    .build()
    .await?;
```

In this example:
- `normal-query` ADD → Uses `default_add`
- `normal-query` UPDATE → Uses `default_update`
- `normal-query` DELETE → Skipped (no template)
- `critical-query` ADD → Uses `critical_add` (route override)
- `critical-query` UPDATE → Uses `default_update` (fallback)
- `critical-query` DELETE → Uses `critical_delete` (route)

## Error Handling

The reaction includes automatic retry logic with exponential backoff:
- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

## Connection Pooling

The MySQL reaction uses connection pooling for optimal performance. Connections are automatically managed and reused.

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
