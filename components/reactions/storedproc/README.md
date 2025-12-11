# Stored Procedure Reaction

A Drasi reaction plugin that invokes SQL stored procedures when continuous query results change. This reaction allows you to execute pre-defined stored procedures in your PostgreSQL database based on ADD, UPDATE, and DELETE operations from continuous queries.

## Overview

The Stored Procedure reaction enables you to:
- Execute different stored procedures for ADD, UPDATE, and DELETE operations
- Map query result fields to stored procedure parameters using `@fieldName` syntax
- Handle multiple queries with a single reaction
- Automatically retry failed procedure calls with exponential backoff
- Configure connection pooling and timeouts

## Supported Databases

- **PostgreSQL** (current implementation)
- MySQL (planned)
- MS SQL Server (planned)

## Installation

Add the dependency to your `Cargo.toml`:

```toml
[dependencies]
drasi-reaction-storedproc = { path = "path/to/drasi-core/components/reactions/storedproc" }
```

## Quick Start

### 1. Create Stored Procedures in PostgreSQL

First, create the stored procedures you want to call:

```sql
-- Stored procedure for adding users
CREATE OR REPLACE PROCEDURE add_user(
    p_id INTEGER,
    p_name TEXT,
    p_email TEXT,
    p_created_at TIMESTAMP
)
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO users_sync (id, name, email, created_at)
    VALUES (p_id, p_name, p_email, p_created_at);

    RAISE NOTICE 'User added: % (%)', p_name, p_email;
END;
$$;

-- Stored procedure for updating users
CREATE OR REPLACE PROCEDURE update_user(
    p_id INTEGER,
    p_name TEXT,
    p_email TEXT
)
LANGUAGE plpgsql
AS $$
BEGIN
    UPDATE users_sync
    SET name = p_name, email = p_email, updated_at = NOW()
    WHERE id = p_id;

    RAISE NOTICE 'User updated: %', p_id;
END;
$$;

-- Stored procedure for deleting users
CREATE OR REPLACE PROCEDURE delete_user(
    p_id INTEGER
)
LANGUAGE plpgsql
AS $$
BEGIN
    DELETE FROM users_sync WHERE id = p_id;

    RAISE NOTICE 'User deleted: %', p_id;
END;
$$;
```

### 2. Create the Reaction

```rust
use drasi_reaction_storedproc::{StoredProcReaction, DatabaseClient};
use drasi_lib::DrasiLib;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Create the stored procedure reaction
    let reaction = StoredProcReaction::builder("user-sync")
        .with_database_client(DatabaseClient::PostgreSQL)
        .with_connection(
            "localhost",           // hostname
            5432,                  // port
            "mydb",               // database name
            "postgres",           // user
            "password"            // password
        )
        .with_query("user-changes")
        .with_added_command("CALL add_user(@id, @name, @email, @created_at)")
        .with_updated_command("CALL update_user(@id, @name, @email)")
        .with_deleted_command("CALL delete_user(@id)")
        .build()
        .await?;

    // Create DrasiLib instance with the reaction
    let drasi = DrasiLib::builder()
        .with_id("my-app")
        .with_reaction(reaction)  // Add during builder phase
        .build()
        .await?;

    // Start the system
    drasi.start().await?;

    // Keep running...
    tokio::signal::ctrl_c().await?;

    Ok(())
}
```

## Configuration

### Builder API (Recommended)

```rust
let reaction = StoredProcReaction::builder("my-reaction")
    // Database configuration
    .with_database_client(DatabaseClient::PostgreSQL)
    .with_hostname("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_user("postgres")
    .with_password("secret")
    .with_ssl(true)  // Enable SSL/TLS

    // Or use the shorthand:
    // .with_connection("localhost", 5432, "mydb", "postgres", "secret")

    // Query subscriptions
    .with_query("query1")
    .with_query("query2")

    // Stored procedure commands
    .with_added_command("CALL my_schema.add_record(@id, @name, @value)")
    .with_updated_command("CALL my_schema.update_record(@id, @name, @value)")
    .with_deleted_command("CALL my_schema.delete_record(@id)")

    // Performance tuning
    .with_command_timeout_ms(30000)      // 30 second timeout
    .with_retry_attempts(3)              // Retry 3 times on failure
    .with_priority_queue_capacity(5000)  // Queue capacity
    .with_auto_start(true)               // Start automatically

    .build()
    .await?;
```

### Configuration Options

| Option | Description | Type | Default |
|--------|-------------|------|---------|
| `database_client` | Database type | `DatabaseClient` | `PostgreSQL` |
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
| `priority_queue_capacity` | Queue capacity | `usize` | `10000` |
| `auto_start` | Auto-start on add | `bool` | `true` |

## Parameter Mapping

Use the `@fieldName` syntax in stored procedure commands to reference fields from query results:

```rust
.with_added_command("CALL add_user(@id, @name, @email)")
```

When a query result contains:
```json
{
  "id": 1,
  "name": "Alice",
  "email": "alice@example.com"
}
```

The reaction will execute:
```sql
CALL add_user(1, 'Alice', 'alice@example.com')
```

### Nested Field Access

You can access nested fields using dot notation:

```rust
.with_added_command("CALL add_address(@user.id, @address.city, @address.zip)")
```

For a query result:
```json
{
  "user": {"id": 42, "name": "Bob"},
  "address": {"city": "Seattle", "zip": "98101"}
}
```

This executes:
```sql
CALL add_address(42, 'Seattle', '98101')
```

## Complete Example

### Setup PostgreSQL

```bash
# Start PostgreSQL (using Docker)
docker run --name drasi-postgres \
  -e POSTGRES_PASSWORD=mysecret \
  -e POSTGRES_DB=drasi_test \
  -p 5432:5432 \
  -d postgres:15

# Create tables and procedures
psql -h localhost -U postgres -d drasi_test
```

```sql
-- Create target table
CREATE TABLE user_events (
    id SERIAL PRIMARY KEY,
    user_id INTEGER NOT NULL,
    event_type VARCHAR(50),
    name TEXT,
    email TEXT,
    timestamp TIMESTAMP DEFAULT NOW()
);

-- Create stored procedures
CREATE OR REPLACE PROCEDURE log_user_added(
    p_user_id INTEGER,
    p_name TEXT,
    p_email TEXT
)
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO user_events (user_id, event_type, name, email)
    VALUES (p_user_id, 'USER_ADDED', p_name, p_email);
END;
$$;

CREATE OR REPLACE PROCEDURE log_user_updated(
    p_user_id INTEGER,
    p_name TEXT,
    p_email TEXT
)
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO user_events (user_id, event_type, name, email)
    VALUES (p_user_id, 'USER_UPDATED', p_name, p_email);
END;
$$;

CREATE OR REPLACE PROCEDURE log_user_deleted(
    p_user_id INTEGER
)
LANGUAGE plpgsql
AS $$
BEGIN
    INSERT INTO user_events (user_id, event_type)
    VALUES (p_user_id, 'USER_DELETED');
END;
$$;
```

### Rust Application

```rust
use drasi_lib::DrasiLib;
use drasi_lib::builder::Query;
use drasi_reaction_storedproc::{StoredProcReaction, DatabaseClient};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Create the stored procedure reaction
    let reaction = StoredProcReaction::builder("user-event-logger")
        .with_database_client(DatabaseClient::PostgreSQL)
        .with_connection(
            "localhost",
            5432,
            "drasi_test",
            "postgres",
            "mysecret"
        )
        .with_query("user-changes")
        .with_added_command("CALL log_user_added(@id, @name, @email)")
        .with_updated_command("CALL log_user_updated(@id, @name, @email)")
        .with_deleted_command("CALL log_user_deleted(@id)")
        .build()
        .await?;

    // Create a continuous query (example)
    let query = Query::cypher("user-changes")
        .query("MATCH (u:User) RETURN u.id AS id, u.name AS name, u.email AS email")
        .from_source("user-source")
        .build();

    // Build and start DrasiLib
    let drasi = DrasiLib::builder()
        .with_id("user-sync-app")
        // Add your source here
        // .with_source(user_source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await?;

    drasi.start().await?;

    println!("User sync application started. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;

    drasi.stop().await?;
    println!("Application stopped.");

    Ok(())
}
```

## Error Handling

The StoredProc reaction includes automatic retry logic with exponential backoff:

- **Initial retry**: 100ms delay
- **Subsequent retries**: 200ms, 400ms, 800ms, etc.
- **Max retries**: Configurable (default: 3)
- **Timeout**: Configurable per command (default: 30s)

### Logging

Enable logging to see detailed execution information:

```bash
RUST_LOG=drasi_reaction_storedproc=debug cargo run
```

Log levels:
- **INFO**: Connection status, successful executions
- **DEBUG**: Detailed parameter mapping, retry attempts
- **ERROR**: Failed executions, connection errors

## Advanced Usage

### Different Procedures for Different Queries

```rust
// Query 1: User changes -> user sync procedures
let user_reaction = StoredProcReaction::builder("user-sync")
    .with_connection("localhost", 5432, "mydb", "postgres", "pass")
    .with_query("user-changes")
    .with_added_command("CALL sync_user_add(@id, @name)")
    .with_updated_command("CALL sync_user_update(@id, @name)")
    .build()
    .await?;

// Query 2: Order changes -> order sync procedures
let order_reaction = StoredProcReaction::builder("order-sync")
    .with_connection("localhost", 5432, "mydb", "postgres", "pass")
    .with_query("order-changes")
    .with_added_command("CALL sync_order_add(@order_id, @amount)")
    .with_deleted_command("CALL sync_order_cancel(@order_id)")
    .build()
    .await?;
```

### Schema-Qualified Procedure Names

```rust
.with_added_command("CALL my_schema.add_record(@id, @name)")
.with_updated_command("CALL my_schema.update_record(@id, @name)")
```

### No Parameters

For procedures that don't need parameters:

```rust
.with_added_command("CALL trigger_refresh()")
```

## Troubleshooting

### Connection Issues

**Problem**: Cannot connect to PostgreSQL

**Solutions**:
1. Check PostgreSQL is running: `pg_isready -h localhost -p 5432`
2. Verify credentials and database name
3. Check firewall/network settings
4. Enable SSL if required by server

### Procedure Execution Failures

**Problem**: Stored procedure fails to execute

**Solutions**:
1. Verify procedure exists: `\df procedure_name` in psql
2. Check procedure signature matches parameters
3. Ensure user has EXECUTE permissions
4. Review PostgreSQL logs for detailed errors

### Missing Field Errors

**Problem**: `Field 'xyz' not found in query result`

**Solutions**:
1. Verify query returns the required field
2. Check field name spelling and case
3. Use nested access for nested fields: `@user.name`

### Performance Issues

**Problem**: Slow procedure execution

**Solutions**:
1. Increase `command_timeout_ms`
2. Optimize stored procedure code
3. Add indexes to target tables
4. Monitor PostgreSQL performance

## Performance Considerations

- **Connection Management**: Uses a single connection per reaction (connection pooling planned)
- **Retry Logic**: Failed executions retry with exponential backoff
- **Queue Capacity**: Adjust `priority_queue_capacity` based on throughput
- **Timeouts**: Set appropriate timeouts for your procedures

## Limitations

1. **PostgreSQL Only**: MySQL and MS SQL support planned for future releases
2. **Sequential Processing**: Procedures executed sequentially (parallel execution planned)
3. **No Transaction Control**: Each procedure call is independent
4. **Parameter Types**: Automatic type conversion from JSON to SQL types

## Future Enhancements

- [ ] MySQL support
- [ ] MS SQL Server support
- [ ] Connection pooling
- [ ] Parallel procedure execution
- [ ] Transaction support (group multiple procedures)
- [ ] Custom type mappings
- [ ] Batch procedure calls

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
