# PostgreSQL Bootstrap Provider

A bootstrap provider for Drasi that reads initial data snapshots from PostgreSQL databases. This component enables continuous queries to start with a complete view of existing data before processing incremental changes.

## Overview

The PostgreSQL Bootstrap Provider creates consistent point-in-time snapshots of PostgreSQL tables and streams the data to Drasi queries. It provides the foundation for continuous queries by establishing the initial state before change tracking begins.

### Key Capabilities

- **Consistent Snapshots**: Uses PostgreSQL's REPEATABLE READ isolation level to ensure data consistency
- **Automatic Primary Key Detection**: Discovers primary keys from PostgreSQL system catalogs
- **Custom Key Configuration**: Supports user-defined key columns for tables without primary keys
- **Label Mapping**: Automatically maps Cypher query labels to PostgreSQL tables
- **Batch Processing**: Efficiently streams data in batches of 1000 records
- **Type Conversion**: Handles PostgreSQL data types including integers, floats, strings, booleans, timestamps, decimals, JSON, and UUIDs
- **LSN Tracking**: Captures Write-Ahead Log (WAL) position for coordination with replication sources

### Use Cases

- **Initial Query State**: Populate continuous queries with existing database records
- **Data Migration**: Bootstrap queries when connecting to existing PostgreSQL databases
- **Testing and Development**: Create reproducible initial states for query testing
- **Multi-Table Queries**: Initialize joins across multiple PostgreSQL tables

## Configuration

The PostgreSQL Bootstrap Provider can be configured using either the builder pattern (preferred) or a configuration struct.

### Builder Pattern (Preferred)

The builder pattern provides a fluent API for constructing the provider:

```rust
use drasi_bootstrap_postgres::PostgresBootstrapProvider;

let provider = PostgresBootstrapProvider::builder()
    .with_host("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_user("postgres")
    .with_password("secret")
    .with_tables(vec!["users".to_string(), "orders".to_string()])
    .with_slot_name("drasi_slot")
    .with_publication_name("drasi_pub")
    .with_table_key("orders", vec!["order_id".to_string()])
    .build();
```

### Configuration Struct

Alternatively, use the `PostgresSourceConfig` struct directly:

```rust
use drasi_bootstrap_postgres::{PostgresBootstrapProvider, PostgresSourceConfig};
use drasi_lib::config::common::{SslMode, TableKeyConfig};

let config = PostgresSourceConfig {
    host: "localhost".to_string(),
    port: 5432,
    database: "mydb".to_string(),
    user: "postgres".to_string(),
    password: "secret".to_string(),
    tables: vec!["users".to_string(), "orders".to_string()],
    slot_name: "drasi_slot".to_string(),
    publication_name: "drasi_pub".to_string(),
    ssl_mode: SslMode::Disable,
    table_keys: vec![
        TableKeyConfig {
            table: "orders".to_string(),
            key_columns: vec!["order_id".to_string()],
        }
    ],
};

let provider = PostgresBootstrapProvider::new(config);
```

## Configuration Options

| Name | Description | Data Type | Valid Values | Default |
|------|-------------|-----------|--------------|---------|
| `host` | PostgreSQL server hostname or IP address | `String` | Any valid hostname or IP | `"localhost"` |
| `port` | PostgreSQL server port | `u16` | 1-65535 | `5432` |
| `database` | Database name to connect to | `String` | Any valid PostgreSQL database name | Required (no default) |
| `user` | Database user for authentication | `String` | Valid PostgreSQL username | Required (no default) |
| `password` | Database password for authentication | `String` | Any string | `""` (empty) |
| `tables` | List of tables to bootstrap | `Vec<String>` | Valid table names | `[]` (empty) |
| `slot_name` | Replication slot name | `String` | Valid PostgreSQL identifier | `"drasi_slot"` |
| `publication_name` | Publication name for logical replication | `String` | Valid PostgreSQL identifier | `"drasi_pub"` |
| `ssl_mode` | SSL/TLS connection mode | `SslMode` | `Disable`, `Prefer`, `Require` | `Disable` |
| `table_keys` | Custom primary key configurations | `Vec<TableKeyConfig>` | List of table/column mappings | `[]` (empty) |

### TableKeyConfig Structure

```rust
pub struct TableKeyConfig {
    pub table: String,          // Table name
    pub key_columns: Vec<String>, // Column names that form the key
}
```

### SSL Mode Values

- `SslMode::Disable` - No SSL encryption
- `SslMode::Prefer` - Prefer SSL but allow unencrypted connections
- `SslMode::Require` - Require SSL encryption (connection fails without SSL)

## Input Schema

### PostgreSQL Table Format

The bootstrap provider reads standard PostgreSQL tables from the `public` schema. Tables are automatically mapped from Cypher query labels using lowercase conversion.

**Example Mapping:**
- Cypher label `User` → PostgreSQL table `user`
- Cypher label `Order` → PostgreSQL table `order`
- Cypher label `Product` → PostgreSQL table `product`

### Primary Key Detection

The provider automatically detects primary keys using PostgreSQL system catalogs:

```sql
SELECT a.attname as column_name
FROM pg_constraint con
JOIN pg_class c ON con.conrelid = c.oid
JOIN pg_attribute a ON a.attrelid = c.oid
WHERE con.contype = 'p'  -- Primary key constraint
  AND a.attnum = ANY(con.conkey)
```

**Automatic Detection Example:**

```sql
-- PostgreSQL table definition
CREATE TABLE users (
    user_id SERIAL PRIMARY KEY,
    username VARCHAR(100) NOT NULL,
    email VARCHAR(255)
);
```

The provider automatically detects `user_id` as the primary key and uses it for element ID generation.

### Custom Key Configuration

For tables without primary keys or when you need custom key columns:

```rust
.with_table_key("users", vec!["username".to_string(), "email".to_string()])
```

**Multiple Column Keys:**

```rust
.with_table_key("user_sessions", vec![
    "user_id".to_string(),
    "session_id".to_string()
])
```

Element IDs are generated as: `table:value1_value2_...`

**Example:** `user_sessions:123_abc-def-456`

### Fallback Behavior

If no primary key is found and no custom key is configured:
1. A warning is logged
2. A UUID is generated for each row
3. Element ID format: `table:uuid`

**Warning message:**
```
No primary key found for table 'users'. Consider adding 'table_keys' configuration.
```

### Supported Data Types

The provider supports common PostgreSQL data types with automatic conversion:

| PostgreSQL Type | Drasi ElementValue | Notes |
|----------------|-------------------|-------|
| `boolean` | `Bool` | True/false values |
| `smallint`, `integer`, `bigint` | `Integer` | Converted to i64 |
| `real`, `double precision` | `Float` | Converted to f64 |
| `numeric`, `decimal` | `Float` | Parsed via decimal conversion |
| `varchar`, `text`, `char` | `String` | Text data |
| `timestamp`, `timestamptz` | `String` | ISO 8601 format |
| `date` | `String` | ISO 8601 format |
| `uuid` | `String` | Hyphenated UUID format |
| `json`, `jsonb` | `String` | JSON serialized |
| Other types | `String` | Best-effort string conversion |

NULL values are represented as `ElementValue::Null`.

## Usage Examples

### Basic Bootstrap

```rust
use drasi_bootstrap_postgres::PostgresBootstrapProvider;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::channels::bootstrap_channel;

// Create provider
let provider = PostgresBootstrapProvider::builder()
    .with_host("localhost")
    .with_port(5432)
    .with_database("production_db")
    .with_user("drasi_user")
    .with_password("secure_password")
    .with_tables(vec!["customers".to_string()])
    .build();

// Create channel for receiving bootstrap events
let (tx, mut rx) = bootstrap_channel(1000);

// Create bootstrap request (from query)
let request = BootstrapRequest {
    query_id: "customer-query".to_string(),
    node_labels: vec!["Customer".to_string()],
    relation_labels: vec![],
};

// Create context
let context = BootstrapContext {
    source_id: "postgres-source".to_string(),
    sequence_counter: Arc::new(AtomicU64::new(0)),
};

// Execute bootstrap
let count = provider.bootstrap(request, &context, tx).await?;
println!("Bootstrapped {} records", count);

// Receive events
while let Some(event) = rx.recv().await {
    println!("Received: {:?}", event);
}
```

### Multi-Table Bootstrap with Custom Keys

```rust
use drasi_bootstrap_postgres::PostgresBootstrapProvider;

let provider = PostgresBootstrapProvider::builder()
    .with_host("db.example.com")
    .with_port(5432)
    .with_database("ecommerce")
    .with_user("app_user")
    .with_password("app_password")
    // Add multiple tables
    .with_table("users")
    .with_table("orders")
    .with_table("order_items")
    // Configure custom keys for tables without primary keys
    .with_table_key("order_items", vec![
        "order_id".to_string(),
        "product_id".to_string()
    ])
    .build();

let request = BootstrapRequest {
    query_id: "order-analytics".to_string(),
    node_labels: vec![
        "User".to_string(),
        "Order".to_string(),
        "OrderItem".to_string(),
    ],
    relation_labels: vec![],
};

// Bootstrap will process all three tables
let count = provider.bootstrap(request, &context, tx).await?;
```

### SSL-Enabled Connection

```rust
use drasi_bootstrap_postgres::PostgresBootstrapProvider;
use drasi_lib::config::common::SslMode;

let provider = PostgresBootstrapProvider::builder()
    .with_host("secure-db.example.com")
    .with_port(5432)
    .with_database("secure_db")
    .with_user("secure_user")
    .with_password("secure_password")
    .with_ssl_mode(SslMode::Require)
    .with_tables(vec!["sensitive_data".to_string()])
    .build();
```

### Integration with Drasi Query

```rust
use drasi_lib::{Query, DrasiCore};
use drasi_bootstrap_postgres::PostgresBootstrapProvider;

// Create bootstrap provider
let bootstrap = PostgresBootstrapProvider::builder()
    .with_host("localhost")
    .with_port(5432)
    .with_database("mydb")
    .with_user("postgres")
    .with_password("password")
    .build();

// Create query that will be bootstrapped
let query = Query::cypher("user-monitor")
    .query("MATCH (u:User) WHERE u.active = true RETURN u")
    .from_source("postgres-source")
    .build();

// Bootstrap provider will populate initial state when query starts
```

## Implementation Details

### Bootstrap Process

1. **Connection**: Establishes PostgreSQL connection with provided credentials
2. **Primary Key Discovery**: Queries PostgreSQL system catalogs for primary key information
3. **Snapshot Creation**: Creates REPEATABLE READ transaction for consistency
4. **LSN Capture**: Records current WAL position for replication coordination
5. **Label Mapping**: Maps Cypher query labels to PostgreSQL table names
6. **Table Validation**: Verifies that mapped tables exist in the database
7. **Data Streaming**: Reads rows from each table and converts to Drasi elements
8. **Batch Transmission**: Sends events in batches of 1000 for efficiency
9. **Transaction Commit**: Releases snapshot after all data is sent

### Element ID Generation

Element IDs follow these rules:

1. **With Primary Key**: `table:pk_value1_pk_value2_...`
   - Example: `users:123`
   - Example: `user_sessions:456_abc`

2. **Without Primary Key**: `table:uuid`
   - Example: `logs:550e8400-e29b-41d4-a716-446655440000`

### Consistency Guarantees

- **Snapshot Isolation**: All tables are read within a single REPEATABLE READ transaction
- **Point-in-Time Consistency**: All data represents the database state at the captured LSN
- **Batch Ordering**: Events maintain sequential ordering within each table
- **Sequence Numbers**: Each event receives a unique, monotonically increasing sequence number

### Performance Characteristics

- **Batch Size**: 1000 records per batch (configurable in code)
- **Memory Efficiency**: Streaming approach avoids loading entire tables into memory
- **Connection Management**: Single connection with async task for connection lifecycle
- **Type Conversion**: Zero-copy conversions where possible

### Error Handling

The provider returns errors for:
- Connection failures (invalid credentials, network issues)
- Invalid database/table names
- Type conversion failures (logged as warnings, converted to Null)
- Channel send failures (indicates downstream consumer issues)

## Advanced Topics

### Schema Discovery

Tables are discovered dynamically based on query labels. The provider does not require pre-registration of tables—it automatically maps labels to tables and verifies their existence.

### Multi-Schema Support

Tables from schemas other than `public` are supported by including the schema in the table name:

```rust
.with_table("analytics.user_metrics")
.with_table_key("analytics.user_metrics", vec!["metric_id".to_string()])
```

### Composite Primary Keys

Composite keys are fully supported and automatically detected:

```sql
CREATE TABLE user_permissions (
    user_id INTEGER,
    resource_id INTEGER,
    permission VARCHAR(50),
    PRIMARY KEY (user_id, resource_id)
);
```

Element IDs will be generated as: `user_permissions:123_456`

### Null Handling

NULL values in primary key columns trigger UUID fallback:
- Primary key columns with NULL are excluded from ID generation
- If all primary key columns are NULL, a UUID is generated
- A warning is logged for each NULL primary key occurrence

### Logging

The provider uses the `log` crate with standard levels:

- **Info**: Connection status, table counts, completion statistics
- **Debug**: Column mappings, primary key details, per-table progress
- **Warn**: Missing tables, missing primary keys, type conversion issues
- **Error**: Connection failures, query errors, channel failures

Enable logging in your application:

```rust
env_logger::init();
// Set RUST_LOG=debug for detailed logging
```

## Troubleshooting

### "Table does not exist" Warning

**Symptom:** Warning message about missing table

**Solution:** Verify label-to-table mapping:
- Cypher label `User` maps to table `user` (lowercase)
- Specify exact table names in configuration
- Check that tables exist in `public` schema

### "No primary key found" Warning

**Symptom:** Warning about missing primary keys, UUIDs used for element IDs

**Solution:** Configure custom key columns:

```rust
.with_table_key("my_table", vec!["id_column".to_string()])
```

### Type Conversion Errors

**Symptom:** Warnings about type conversion failures

**Solution:**
- NULL values are converted to `ElementValue::Null`
- Unsupported types default to string conversion
- Check PostgreSQL logs for actual data types

### Connection Failures

**Symptom:** Error connecting to PostgreSQL

**Solution:**
- Verify host, port, database, user, password
- Check PostgreSQL is running and accepting connections
- Verify network connectivity and firewall rules
- Check PostgreSQL `pg_hba.conf` for authentication settings

### Memory Issues with Large Tables

**Symptom:** High memory usage during bootstrap

**Solution:**
- Batch processing (1000 records) is already optimized
- Consider bootstrapping tables separately
- Monitor downstream consumer to ensure it's processing events
- Increase channel buffer size if needed

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
