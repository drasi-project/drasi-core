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
        updated: None,
        deleted: None,
    })
    .build()
    .await?;
```

## Error Handling

The reaction includes automatic retry logic with exponential backoff:
- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
