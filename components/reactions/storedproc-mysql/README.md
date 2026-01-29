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

### Builder API

#### Traditional Username/Password Authentication

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

#### Cloud Identity Provider Authentication

For cloud-managed MySQL databases, you can use identity providers instead of passwords:

**Azure AD Authentication (Azure Database for MySQL):**

```rust
use drasi_lib::identity::AzureIdentityProvider;

// For Azure Kubernetes Service with Workload Identity
let identity_provider = AzureIdentityProvider::with_workload_identity("myuser@myserver")?;

// For local development or Azure VMs with Managed Identity
let identity_provider = AzureIdentityProvider::with_default_credentials("myuser@myserver").await?;

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.mysql.database.azure.com")
    .with_port(3306)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_ssl(true)  // Required for Azure
    .with_query("query1")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL add_record(@after.id, @after.name)")),
        updated: Some(TemplateSpec::new("CALL update_record(@after.id, @after.name)")),
        deleted: Some(TemplateSpec::new("CALL delete_record(@before.id)")),
    })
    .build()
    .await?;
```

**AWS IAM Authentication (Amazon RDS for MySQL/Aurora MySQL):**

```rust
use drasi_lib::identity::AwsIdentityProvider;

// Using IAM user credentials
let identity_provider = AwsIdentityProvider::new(
    "myuser",
    "mydb.rds.amazonaws.com",
    3306
).await?;

// Or assuming an IAM role
let identity_provider = AwsIdentityProvider::with_assumed_role(
    "myuser",
    "mydb.rds.amazonaws.com",
    3306,
    "arn:aws:iam::123456789012:role/RDSAccessRole",
    None
).await?;

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("mydb.rds.amazonaws.com")
    .with_port(3306)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_ssl(true)  // Recommended for RDS
    .with_query("query1")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL add_record(@after.id, @after.name)")),
        updated: Some(TemplateSpec::new("CALL update_record(@after.id, @after.name)")),
        deleted: Some(TemplateSpec::new("CALL delete_record(@before.id)")),
    })
    .build()
    .await?;
```

**Password Provider (programmatic username/password):**

```rust
use drasi_lib::identity::PasswordIdentityProvider;

let identity_provider = PasswordIdentityProvider::new("root", "secret");

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(3306)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_query("query1")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL add_record(@after.id, @after.name)")),
        updated: Some(TemplateSpec::new("CALL update_record(@after.id, @after.name)")),
        deleted: Some(TemplateSpec::new("CALL delete_record(@before.id)")),
    })
    .build()
    .await?;
```

> **Note:** When using identity providers, do not call `.with_user()` or `.with_password()`. The identity provider handles authentication automatically.
>
> See the [Identity Provider README](../../../lib/src/identity/README.md) for detailed setup instructions for Azure AD and AWS IAM authentication.

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
