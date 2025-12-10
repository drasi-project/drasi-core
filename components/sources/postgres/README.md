# PostgreSQL Replication Source

## Overview

The PostgreSQL Replication Source is a Change Data Capture (CDC) plugin for Drasi that streams data changes from PostgreSQL databases in real-time using logical replication and the Write-Ahead Log (WAL). It captures INSERT, UPDATE, and DELETE operations as they occur and transforms them into Drasi `SourceChange` events for continuous query processing.

**Key Capabilities**:
- Real-time change streaming via PostgreSQL logical replication
- Transactionally-consistent data capture using WAL decoding
- Initial snapshot bootstrapping with LSN coordination
- Automatic reconnection and recovery on connection failures
- Support for multiple PostgreSQL data types with type-safe conversion
- Primary key detection and custom key configuration
- SCRAM-SHA-256 authentication (recommended) and cleartext fallback

**Use Cases**:
- Real-time data synchronization from PostgreSQL databases
- Event-driven architectures based on database changes
- Building reactive applications that respond to data mutations
- Maintaining materialized views or derived data sets
- Audit logging and change tracking
- Data replication and ETL pipelines

## Architecture

### Components

The PostgreSQL source consists of several specialized modules:

1. **Connection** (`connection.rs`): Manages the PostgreSQL replication protocol connection, authentication (including SCRAM-SHA-256), and message exchange
2. **Stream** (`stream.rs`): Handles the continuous WAL streaming loop, message processing, and transaction coordination
3. **Decoder** (`decoder.rs`): Decodes binary pgoutput messages into structured WAL events with full type support
4. **Bootstrap** (`bootstrap.rs`): Performs consistent snapshots for initial data loading with primary key detection
5. **Protocol** (`protocol.rs`): Implements PostgreSQL wire protocol encoding/decoding
6. **SCRAM** (`scram.rs`): SCRAM-SHA-256 authentication implementation
7. **Types** (`types.rs`): Type definitions for PostgreSQL values and WAL messages

### Data Flow

```
PostgreSQL WAL → Connection → Decoder → Stream → SourceChange Events
                                ↓
                          Transaction
                          Grouping
                                ↓
                          Dispatcher → Queries
```

**Bootstrap Flow**:
```
Bootstrap Request → Snapshot Transaction → Table Scan → SourceChange Events
                          ↓
                    Capture LSN → Coordinate with Streaming
```

## Prerequisites

### PostgreSQL Configuration

The PostgreSQL database must be configured for logical replication:

1. **PostgreSQL Version**: PostgreSQL 10 or later (requires pgoutput plugin)

2. **Configuration Parameters** (`postgresql.conf`):
   ```ini
   wal_level = logical
   max_replication_slots = 10  # At least 1 per source
   max_wal_senders = 10        # At least 1 per source
   ```

3. **Database User Permissions**:
   ```sql
   -- Grant replication privilege
   ALTER USER drasi_user WITH REPLICATION;

   -- Grant table access
   GRANT SELECT ON ALL TABLES IN SCHEMA public TO drasi_user;
   GRANT USAGE ON SCHEMA public TO drasi_user;
   ```

4. **Publication Setup**:
   ```sql
   -- Create a publication for specific tables
   CREATE PUBLICATION drasi_publication FOR TABLE users, orders, products;

   -- Or for all tables
   CREATE PUBLICATION drasi_publication FOR ALL TABLES;
   ```

5. **Replication Slot**: The source automatically creates a replication slot with the configured name. If it exists, it will be reused.

6. **Replica Identity** (recommended for full UPDATE/DELETE data):
   ```sql
   -- For tables without primary keys or needing full row data
   ALTER TABLE your_table REPLICA IDENTITY FULL;

   -- Default behavior (uses primary key)
   ALTER TABLE your_table REPLICA IDENTITY DEFAULT;
   ```

## Configuration

### Builder Pattern (Recommended)

The builder pattern provides type-safe configuration with sensible defaults:

```rust
use drasi_source_postgres::PostgresReplicationSource;
use drasi_lib::config::common::{SslMode, TableKeyConfig};

let source = PostgresReplicationSource::builder("postgres-source-1")
    .with_host("db.example.com")
    .with_port(5432)
    .with_database("production_db")
    .with_user("drasi_user")
    .with_password("secure_password")
    .with_tables(vec!["users".to_string(), "orders".to_string()])
    .with_slot_name("drasi_production_slot")
    .with_publication_name("drasi_publication")
    .with_ssl_mode(SslMode::Require)
    .add_table_key(TableKeyConfig {
        table: "users".to_string(),
        key_columns: vec!["user_id".to_string()],
    })
    .with_dispatch_mode(drasi_lib::channels::DispatchMode::Channel)
    .with_dispatch_buffer_capacity(2000)
    .with_auto_start(true)
    .build()?;
```

### Config Struct Approach

Alternatively, construct the config struct directly:

```rust
use drasi_source_postgres::{PostgresReplicationSource, PostgresSourceConfig};
use drasi_lib::config::common::{SslMode, TableKeyConfig};

let config = PostgresSourceConfig {
    host: "db.example.com".to_string(),
    port: 5432,
    database: "production_db".to_string(),
    user: "drasi_user".to_string(),
    password: "secure_password".to_string(),
    tables: vec!["users".to_string(), "orders".to_string()],
    slot_name: "drasi_production_slot".to_string(),
    publication_name: "drasi_publication".to_string(),
    ssl_mode: SslMode::Require,
    table_keys: vec![
        TableKeyConfig {
            table: "users".to_string(),
            key_columns: vec!["user_id".to_string()],
        },
    ],
};

let source = PostgresReplicationSource::new("postgres-source-1", config)?;
```

## Configuration Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `id` | `String` | **(Required)** | Unique identifier for the source instance |
| `host` | `String` | `"localhost"` | PostgreSQL server hostname or IP address |
| `port` | `u16` | `5432` | PostgreSQL server port number |
| `database` | `String` | **(Required)** | Database name to connect to |
| `user` | `String` | **(Required)** | Database user with replication privileges |
| `password` | `String` | `""` | Database password (supports SCRAM-SHA-256 and cleartext) |
| `tables` | `Vec<String>` | `[]` | List of tables to monitor (empty = all tables in publication) |
| `slot_name` | `String` | `"drasi_slot"` | Replication slot name (created if doesn't exist) |
| `publication_name` | `String` | `"drasi_publication"` | PostgreSQL publication to subscribe to |
| `ssl_mode` | `SslMode` | `SslMode::Prefer` | SSL mode: `Disable`, `Prefer`, or `Require` |
| `table_keys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration (see below) |
| `dispatch_mode` | `Option<DispatchMode>` | `None` | Channel dispatch mode (builder only) |
| `dispatch_buffer_capacity` | `Option<usize>` | `None` | Dispatch buffer size (builder only) |
| `auto_start` | `bool` | `true` | Whether to start automatically when added to DrasiLib |

### TableKeyConfig

Manually specify primary key columns for element ID generation:

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name (e.g., `"users"` or `"schema.table"`) |
| `key_columns` | `Vec<String>` | Column names to use as primary key |

**Example**:
```rust
TableKeyConfig {
    table: "users".to_string(),
    key_columns: vec!["user_id".to_string()],
}

// Composite key
TableKeyConfig {
    table: "order_items".to_string(),
    key_columns: vec!["order_id".to_string(), "item_id".to_string()],
}
```

**Note**: User-configured keys override automatically detected primary keys.

## Input Schema

### PostgreSQL Logical Replication (pgoutput)

The source consumes WAL messages in the pgoutput binary format:

**WAL Message Types**:
- `B` (Begin): Transaction start
- `C` (Commit): Transaction commit
- `R` (Relation): Table metadata (schema, columns, types)
- `I` (Insert): Row insertion
- `U` (Update): Row update (may include old tuple)
- `D` (Delete): Row deletion
- `T` (Truncate): Table truncate (not implemented)

**Relation Metadata** includes:
- Namespace (schema name)
- Table name
- Replica identity mode
- Column definitions (name, OID, type modifier, key flag)

**Tuple Data** encoding:
- Column count (u16)
- Per-column: type marker (`n` = null, `u` = unchanged TOAST, `t` = text) + length + data

### Type Support

The decoder supports PostgreSQL's built-in types via OID mapping:

| PostgreSQL Type | OID | Decoded As |
|-----------------|-----|------------|
| `boolean` | 16 | `PostgresValue::Bool` |
| `int2` (smallint) | 21 | `PostgresValue::Int2` |
| `int4` (integer) | 23 | `PostgresValue::Int4` |
| `int8` (bigint) | 20 | `PostgresValue::Int8` |
| `float4` (real) | 700 | `PostgresValue::Float4` |
| `float8` (double) | 701 | `PostgresValue::Float8` |
| `numeric` / `decimal` | 1700 | `PostgresValue::Numeric` |
| `text` | 25 | `PostgresValue::Text` |
| `varchar` | 1043 | `PostgresValue::Varchar` |
| `char` / `bpchar` | 1042 | `PostgresValue::Char` |
| `uuid` | 2950 | `PostgresValue::Uuid` |
| `timestamp` | 1114 | `PostgresValue::Timestamp` |
| `timestamptz` | 1184 | `PostgresValue::TimestampTz` |
| `date` | 1082 | `PostgresValue::Date` |
| `time` | 1083 | `PostgresValue::Time` |
| `json` | 114 | `PostgresValue::Json` |
| `jsonb` | 3802 | `PostgresValue::Jsonb` |
| `bytea` | 17 | `PostgresValue::Bytea` |
| Unknown | - | `PostgresValue::Text` (fallback) |

## Usage Examples

### Basic Usage

```rust
use drasi_source_postgres::PostgresReplicationSource;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Build the source
    let source = PostgresReplicationSource::builder("pg-source")
        .with_host("localhost")
        .with_database("myapp")
        .with_user("postgres")
        .with_password("password")
        .with_tables(vec!["users".to_string(), "orders".to_string()])
        .build()?;

    // Start streaming
    source.start().await?;

    // Source will now stream changes continuously
    // Stop when done
    tokio::signal::ctrl_c().await?;
    source.stop().await?;

    Ok(())
}
```

### With Custom Primary Keys

```rust
use drasi_source_postgres::PostgresReplicationSource;
use drasi_lib::config::common::TableKeyConfig;

let source = PostgresReplicationSource::builder("pg-source")
    .with_host("localhost")
    .with_database("myapp")
    .with_user("postgres")
    .add_table_key(TableKeyConfig {
        table: "events".to_string(),
        key_columns: vec!["event_id".to_string(), "timestamp".to_string()],
    })
    .build()?;

source.start().await?;
```

### With SSL and Custom Dispatch

```rust
use drasi_source_postgres::PostgresReplicationSource;
use drasi_lib::config::common::SslMode;
use drasi_lib::channels::DispatchMode;

let source = PostgresReplicationSource::builder("pg-source")
    .with_host("db.example.com")
    .with_database("production")
    .with_user("drasi_user")
    .with_password("secure_password")
    .with_ssl_mode(SslMode::Require)
    .with_dispatch_mode(DispatchMode::Channel)
    .with_dispatch_buffer_capacity(5000)
    .build()?;

source.start().await?;
```

### Using Direct Constructor

```rust
use drasi_source_postgres::{PostgresReplicationSource, PostgresSourceConfig};
use drasi_lib::config::common::SslMode;

let config = PostgresSourceConfig {
    host: "localhost".to_string(),
    port: 5432,
    database: "myapp".to_string(),
    user: "postgres".to_string(),
    password: "password".to_string(),
    tables: vec![],  // All tables in publication
    slot_name: "drasi_slot".to_string(),
    publication_name: "drasi_publication".to_string(),
    ssl_mode: SslMode::Prefer,
    table_keys: vec![],
};

let source = PostgresReplicationSource::new("pg-source", config)?;
source.start().await?;
```

## Output Format

### SourceChange Events

All PostgreSQL changes are transformed into Drasi `SourceChange` events:

**Insert Event**:
```rust
SourceChange::Insert {
    element: Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("pg-source", "users:12345"),
            labels: Arc::from([Arc::from("users")]),
            effective_from: 1704067200000000000,
        },
        properties: ElementPropertyMap {
            "user_id" => ElementValue::Integer(12345),
            "username" => ElementValue::String(Arc::from("john_doe")),
            "email" => ElementValue::String(Arc::from("john@example.com")),
            "is_active" => ElementValue::Bool(true),
        }
    }
}
```

**Update Event**:
```rust
SourceChange::Update {
    element: Element::Node {
        metadata: ElementMetadata { /* same as insert */ },
        properties: ElementPropertyMap { /* new values */ }
    }
}
```

**Delete Event**:
```rust
SourceChange::Delete {
    metadata: ElementMetadata {
        reference: ElementReference::new("pg-source", "users:12345"),
        labels: Arc::from([Arc::from("users")]),
        effective_from: 1704240000000000000,
    }
}
```

### Element ID Generation

Element IDs are generated using the following priority:

1. **User-configured keys** (from `table_keys` config)
2. **Detected primary keys** (from PostgreSQL system catalogs)
3. **UUID fallback** (if no keys available)

**Format**:
- Single key: `"table_name:value"` (e.g., `"users:12345"`)
- Composite key: `"table_name:value1_value2"` (e.g., `"order_items:1001_5"`)
- No key: `"table_name:uuid"` (e.g., `"events:550e8400-e29b-41d4-a716-446655440000"`)

### Labels

- Each element receives the table name as its label (case-preserved)
- Schema-qualified tables use fully qualified names for labels
- Example: `"users"` table → `["users"]` label

## Advanced Features

### Transaction Grouping

Changes are grouped by PostgreSQL transaction and dispatched atomically:
- All changes within a transaction are buffered
- Changes are sent together when the transaction commits
- Ensures transactional consistency in downstream processing

### Bootstrap with LSN Coordination

Bootstrap operations capture a consistent snapshot:
1. Opens a repeatable read transaction
2. Captures current WAL LSN using `pg_current_wal_lsn()`
3. Exports snapshot for consistency
4. Scans tables and sends initial data
5. Coordinates streaming to start from captured LSN

This ensures no data is lost between bootstrap and streaming.

### Automatic Reconnection

The source handles connection failures gracefully:
- Detects connection loss and errors
- Waits 5 seconds before reconnecting
- Re-establishes replication from last confirmed LSN
- Continues streaming without data loss

### Keepalive and Feedback

- Sends keepalive responses every 10 seconds
- Reports LSN progress to PostgreSQL
- Responds to server keepalive requests immediately
- Prevents connection timeouts and slot cleanup

### Primary Key Detection

Bootstrap handler queries PostgreSQL system catalogs to detect primary keys:
```sql
SELECT n.nspname, c.relname, a.attname
FROM pg_constraint con
JOIN pg_class c ON con.conrelid = c.oid
JOIN pg_namespace n ON c.relnamespace = n.oid
JOIN pg_attribute a ON a.attrelid = c.oid
WHERE con.contype = 'p'
  AND a.attnum = ANY(con.conkey)
ORDER BY n.nspname, c.relname, array_position(con.conkey, a.attnum)
```

Detected keys are cached and used for element ID generation during both bootstrap and streaming.

## Troubleshooting

### "permission denied to create replication slot"

**Solution**: Grant replication privilege
```sql
ALTER USER drasi_user WITH REPLICATION;
```

### "logical decoding requires wal_level >= logical"

**Solution**: Configure PostgreSQL and restart
```ini
# postgresql.conf
wal_level = logical
```
```bash
sudo systemctl restart postgresql
```

### "replication slot already exists"

**Options**:
1. Drop existing slot: `SELECT pg_drop_replication_slot('slot_name');`
2. Use different `slot_name` in config
3. Reuse existing slot (source will continue from last position)

### "UPDATE/DELETE missing old tuple data"

**Solution**: Set replica identity to FULL
```sql
ALTER TABLE your_table REPLICA IDENTITY FULL;
```

### "No primary key found for table"

**Solution**: Configure manual keys
```rust
.add_table_key(TableKeyConfig {
    table: "your_table".to_string(),
    key_columns: vec!["id".to_string()],
})
```

### Connection/SSL errors

**Solution**: Adjust SSL mode
```rust
.with_ssl_mode(SslMode::Disable)  // Try without SSL
// or
.with_ssl_mode(SslMode::Prefer)   // Prefer SSL but allow fallback
```

## Performance Considerations

### Memory Usage
- Transaction buffers scale with transaction size
- Large transactions consume more memory
- Bootstrap batches 1000 rows at a time

### Network Latency
- High latency increases replication lag
- Co-locate source with PostgreSQL when possible
- Monitor lag via `pg_replication_slots`

### WAL Retention
- Inactive slots prevent WAL cleanup
- Set `max_slot_wal_keep_size` to limit retention
- Monitor and drop unused slots

### Throughput
- Very high write rates may require tuning
- Consider `dispatch_buffer_capacity` for high volume
- Use `DispatchMode::Channel` for backpressure control

## Monitoring

### PostgreSQL Queries

```sql
-- Check replication slots
SELECT * FROM pg_replication_slots;

-- Monitor replication lag
SELECT slot_name,
       pg_current_wal_lsn() AS current_lsn,
       confirmed_flush_lsn,
       pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn) AS lag_bytes
FROM pg_replication_slots;

-- Check active replication connections
SELECT * FROM pg_stat_replication;

-- View publications
SELECT * FROM pg_publication;
SELECT * FROM pg_publication_tables WHERE pubname = 'drasi_publication';
```

### Log Monitoring

The source logs important events:
- `info!`: Connection events, transaction commits, bootstrap progress
- `warn!`: Missing primary keys, unknown message types, recoverable errors
- `error!`: Connection failures, protocol errors, unrecoverable errors
- `debug!`: Detailed message processing, WAL decoding

Enable debug logging:
```bash
RUST_LOG=drasi_source_postgres=debug cargo run
```

## Known Limitations

### Not Implemented
- **TRUNCATE operations**: Logged but not processed into SourceChange events
- **Schema changes**: DDL operations not captured (requires source restart)
- **Composite/Array types**: Partial support (may lose type information)

### Partial Support
- **TOAST values**: Unchanged TOAST values decoded as NULL (use REPLICA IDENTITY FULL)
- **Binary data**: bytea columns base64-encoded to strings
- **Numeric precision**: Very high-precision decimals may lose precision (converted to f64)

### Known Issues
- Multi-schema support requires fully qualified table names
- UUID fallback IDs not stable across restarts (define primary keys)
- Large objects may not capture all changes (use REPLICA IDENTITY FULL)

## Developer Notes

### Testing
```bash
# Unit tests
cargo test -p drasi-source-postgres

# With PostgreSQL instance
docker run -e POSTGRES_PASSWORD=password -p 5432:5432 postgres:15
cargo test -p drasi-source-postgres -- --test-threads=1
```

### Authentication Methods Supported
- SCRAM-SHA-256 (recommended, see `scram.rs`)
- Cleartext password (not recommended for production)

**Note**: MD5 authentication is explicitly not supported due to security concerns. If your PostgreSQL server requests MD5 authentication, you will receive an error instructing you to configure `scram-sha-256` in `pg_hba.conf`.

### Wire Protocol
The source implements PostgreSQL wire protocol 3.0:
- Startup messages with replication mode
- Query messages for replication commands
- CopyData streaming for WAL messages
- Standby status updates for feedback

See `protocol.rs` and `connection.rs` for implementation details.

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0. See LICENSE file for details.
