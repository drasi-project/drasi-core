# MySQL Stored Procedure Reaction

A Drasi reaction plugin that invokes MySQL stored procedures when continuous query results change.

## Overview

The MySQL Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Map query result fields to stored procedure parameters using `@fieldName` syntax
- Handle multiple queries with a single reaction
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
use drasi_reaction_storedproc_mysql::MySqlStoredProcReaction;
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let reaction = MySqlStoredProcReaction::builder("user-sync")
        .with_connection(
            "localhost",
            3306,
            "mydb",
            "root",
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
let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("localhost")
    .with_port(3306)
    .with_database("mydb")
    .with_user("root")
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
| `port` | Database port | `u16` | `3306` |
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
