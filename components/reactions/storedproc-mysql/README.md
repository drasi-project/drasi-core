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
