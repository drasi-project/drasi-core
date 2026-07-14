# MySQL Stored Procedure Reaction

A Drasi reaction plugin that invokes MySQL stored procedures when continuous query results change.

## Overview

The MySQL Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Map query result fields to stored procedure parameters using Handlebars templates with safe positional parameter binding (`{{param ...}}`)
- Configure a default template for all queries, or per-query routes for specific queries
- Handle multiple queries with different stored procedure configurations
- Automatically retry failed procedure calls with exponential backoff
- Configure connection pooling and timeouts

## How Templating Works

Command templates are rendered with [Handlebars](https://handlebarsjs.com/) in **strict mode**. Two helpers are provided:

- `{{param <path>}}` — resolves the referenced value from the template context, appends it to the ordered parameter list, and emits a `?` placeholder into the rendered SQL. Values referenced with `{{param}}` are bound **positionally** by the MySQL driver — they are never interpolated into the SQL string, so they are safe against SQL injection and preserve value types (numbers stay numbers, strings stay strings). Objects and arrays are bound whole as JSON. **Use `{{param ...}}` for any untrusted value.**
- `{{json <path>}}` — serializes the referenced value to a JSON string and **inlines it directly into the rendered SQL text** (useful when a procedure expects a JSON document as a single argument). Because the result is interpolated into the SQL rather than bound as a parameter, it is **not** protected against SQL injection or quoting problems: a string that contains a single quote (`'`) will break out of a surrounding SQL string literal, and untrusted data could inject SQL. Only use `{{json ...}}` for values you control; for anything derived from source data, prefer `{{param <path>}}`, which binds the object whole as JSON safely.

A template such as:

```
CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})
```

renders to the SQL `CALL add_user(?, ?, ?)` with the three values bound in order.

### Template Context

Every render is given the following context keys:

| Key | Description |
|-----|-------------|
| `query_id` | The originating query id |
| `query_name` | Alias of `query_id` |
| `operation` | `ADD`, `UPDATE`, or `DELETE` |
| `timestamp` | Result timestamp (RFC 3339) |
| `metadata` | Result metadata map |
| `after` | The new row (ADD/UPDATE) |
| `before` | The previous row (UPDATE/DELETE) |
| `data` | The raw result payload |

Use dot notation for nested access, e.g. `{{param after.location.city}}`.

### Strict Mode and Render Failures

Templates are validated (compiled) at construction time; an invalid template causes `build()` to fail. Because rendering runs in strict mode, referencing a field that is absent from the context produces a render error. When a render fails at runtime, that single event is logged and skipped — it does not stop the reaction, and subsequent events continue to be processed.

To bind an explicit `null`, reference a field whose value is JSON `null`; it is bound as SQL `NULL`. Referencing a *missing* field is treated as an error (and skips the event).

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
            added: Some(TemplateSpec::new("CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
            updated: Some(TemplateSpec::new("CALL update_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
            deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
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

#### Username/Password Authentication

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
        added: Some(TemplateSpec::new("CALL add_record({{param after.id}}, {{param after.name}})")),
        updated: Some(TemplateSpec::new("CALL update_record({{param after.id}}, {{param after.name}})")),
        deleted: Some(TemplateSpec::new("CALL delete_record({{param before.id}})")),
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
let identity_provider = AzureIdentityProvider::with_default_credentials("myuser@myserver")?;

let reaction = MySqlStoredProcReaction::builder("my-reaction")
    .with_hostname("myserver.mysql.database.azure.com")
    .with_port(3306)
    .with_database("mydb")
    .with_identity_provider(identity_provider)
    .with_ssl(true)  // Required for Azure
    .with_query("query1")
    .with_default_template(QueryConfig {
        added: Some(TemplateSpec::new("CALL add_record({{param after.id}}, {{param after.name}})")),
        updated: Some(TemplateSpec::new("CALL update_record({{param after.id}}, {{param after.name}})")),
        deleted: Some(TemplateSpec::new("CALL delete_record({{param before.id}})")),
    })
    .build()
    .await?;
```

**AWS IAM Authentication (Amazon RDS for MySQL/Aurora MySQL):**

```rust
use drasi_identity_aws::AwsIdentityProvider;

// Using IAM user credentials
let identity_provider = AwsIdentityProvider::new(
    "myuser",
).await?;

// Or assuming an IAM role
let identity_provider = AwsIdentityProvider::with_assumed_role(
    "myuser",
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
        added: Some(TemplateSpec::new("CALL add_record({{param after.id}}, {{param after.name}})")),
        updated: Some(TemplateSpec::new("CALL update_record({{param after.id}}, {{param after.name}})")),
        deleted: Some(TemplateSpec::new("CALL delete_record({{param before.id}})")),
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
        added: Some(TemplateSpec::new("CALL add_record({{param after.id}}, {{param after.name}})")),
        updated: Some(TemplateSpec::new("CALL update_record({{param after.id}}, {{param after.name}})")),
        deleted: Some(TemplateSpec::new("CALL delete_record({{param before.id}})")),
    })
    .build()
    .await?;
```

> **Note:** When using identity providers, do not call `.with_user()` or `.with_password()`. The identity provider handles authentication automatically. A `user` is only required when no identity provider is configured.
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
        added: Some(TemplateSpec::new("CALL default_add({{param after.id}})")),
        updated: None,
        deleted: None,
    })
    // Special route for critical queries
    .with_route("order-changes", QueryConfig {
        added: Some(TemplateSpec::new("CALL process_order({{param after.order_id}}, {{param after.total}})")),
        updated: Some(TemplateSpec::new("CALL update_order({{param after.order_id}}, {{param after.status}})")),
        deleted: Some(TemplateSpec::new("CALL cancel_order({{param before.order_id}})")),
    })
    .build()
    .await?;
```

Route keys are validated at construction time: every key must match a subscribed query id (or the last dotted segment of one). An unknown route key causes `build()` to fail.

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `hostname` | Database hostname | `String` | `"localhost"` |
| `port` | Database port | `u16` | `3306` |
| `user` | Database user (required unless an identity provider is configured) | `String` | Required |
| `password` | Database password | `String` | Required (unless identity provider) |
| `database` | Database name | `String` | Required |
| `ssl` | Enable SSL/TLS | `bool` | `false` |
| `default_template` | Default template for all queries | `Option<QueryConfig>` | `None` |
| `routes` | Per-query template configurations | `HashMap<String, QueryConfig>` | Empty |
| `command_timeout_ms` | Command timeout | `u64` | `30000` |
| `retry_attempts` | Number of retries | `u32` | `3` |

## Parameter Mapping

### ADD Operations

Use `{{param after.<field>}}` to access the newly added row:

```rust
QueryConfig {
    added: Some(TemplateSpec::new("CALL add_user({{param after.id}}, {{param after.name}}, {{param after.email}})")),
    updated: None,
    deleted: None,
}
```

For a result row `{"id": 1, "name": "Alice", "email": "alice@example.com"}`, this renders `CALL add_user(?, ?, ?)` and binds `1`, `Alice`, and `alice@example.com` in order.

### UPDATE Operations

Use `{{param before.<field>}}` for old values and `{{param after.<field>}}` for new values. The reaction forwards the query result's real `before` and `after` rows, so both are available independently:

```rust
QueryConfig {
    added: None,
    updated: Some(TemplateSpec::new("CALL update_user({{param after.id}}, {{param before.email}}, {{param after.email}})")),
    deleted: None,
}
```

For a diff with `before = {"id": 1, "email": "alice@oldmail.com"}` and `after = {"id": 1, "email": "alice@newmail.com"}`, this binds `1`, `alice@oldmail.com`, and `alice@newmail.com`.

### DELETE Operations

Use `{{param before.<field>}}` to access the deleted row:

```rust
QueryConfig {
    added: None,
    updated: None,
    deleted: Some(TemplateSpec::new("CALL delete_user({{param before.id}})")),
}
```

### Nested Field Access

Access deeply nested fields using dot notation:

```rust
TemplateSpec::new("CALL add_address({{param after.user.id}}, {{param after.location.city}}, {{param after.location.floor}})")
```

### Binding Objects as JSON

To pass an object or array to a procedure as a single JSON argument, reference the object with `{{param ...}}` (it is bound whole as JSON):

```rust
TemplateSpec::new("CALL store_doc({{param after.id}}, {{param after.payload}})")
```

`{{param ...}}` is the recommended way to pass JSON, because the value is bound as a parameter and is never interpolated into the SQL text. The `{{json ...}}` helper also exists for cases where a JSON literal must appear inline in the SQL, but it interpolates the serialized value directly into the statement and is therefore **not** injection- or quoting-safe (see [How Templating Works](#how-templating-works)); only use it with trusted values.

## Template Resolution Priority

When a query result arrives, the reaction selects a template for the operation using the following order:

1. **Full query id route** — `routes[<query_id>]`
2. **Last dotted segment route** — `routes[<last segment of query_id>]` (e.g. a route keyed `my_query` matches a query id `source.my_query`)
3. **Default template** — `default_template`
4. **Skip** — if none of the above provides a template for the operation, the event is skipped

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
        added: Some(TemplateSpec::new("CALL default_add({{param after.id}})")),
        updated: Some(TemplateSpec::new("CALL default_update({{param after.id}})")),
        deleted: None,  // No default for deletes
    })
    // Special handling for critical-query
    .with_route("critical-query", QueryConfig {
        added: Some(TemplateSpec::new("CALL critical_add({{param after.id}}, {{param after.priority}})")),
        updated: None,  // Falls back to default_update
        deleted: Some(TemplateSpec::new("CALL critical_delete({{param before.id}})")),
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

A template that fails to render (e.g. references a missing field) logs a warning and skips only that event; processing continues.

## Connection Pooling

The MySQL reaction uses connection pooling for optimal performance. Connections are automatically managed and reused.

## Plugin Packaging

This reaction is compiled as a dynamic plugin (cdylib) that can be loaded by drasi-server at runtime.

**Key files:**
- `Cargo.toml` — includes `crate-type = ["lib", "cdylib"]`
- `src/descriptor.rs` — implements `ReactionPluginDescriptor` with kind `"storedproc-mysql"`, configuration DTO, and OpenAPI schema generation
- `src/lib.rs` — invokes `drasi_plugin_sdk::export_plugin!` to export the plugin entry point

**Building:**
```bash
cargo build -p drasi-reaction-storedproc-mysql
```

The compiled `.so` (Linux) / `.dylib` (macOS) / `.dll` (Windows) is placed in `target/debug/` and can be copied to the server's `plugins/` directory.

For more details on the plugin descriptor pattern and configuration DTOs, see the [Reaction Developer Guide](../README.md#packaging-as-a-dynamic-plugin).

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0.
