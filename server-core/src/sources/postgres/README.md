# PostgreSQL Replication Source

## Overview

The PostgreSQL Replication Source is a Change Data Capture (CDC) implementation that streams data changes from PostgreSQL databases in real-time using logical replication and the Write-Ahead Log (WAL). It decodes WAL messages using the `pgoutput` logical decoding plugin and transforms them into Drasi `SourceChange` events for continuous query processing.

This source is designed for production use with PostgreSQL databases and provides reliable, transactionally-consistent change streaming with automatic reconnection and recovery capabilities.

## Use Cases

- Real-time data synchronization from PostgreSQL databases
- Implementing event-driven architectures based on database changes
- Building reactive applications that respond to data mutations
- Maintaining materialized views or derived data sets
- Audit logging and change tracking
- Data replication and ETL pipelines

## Prerequisites

### PostgreSQL Configuration

The PostgreSQL database must be configured for logical replication:

1. **PostgreSQL Version**: PostgreSQL 10 or later (pgoutput plugin availability)

2. **Configuration Parameters** (`postgresql.conf`):
   ```ini
   wal_level = logical
   max_replication_slots = 10  # At least 1 per source
   max_wal_senders = 10        # At least 1 per source
   ```

3. **Database User Permissions**:
   ```sql
   -- Grant replication privilege
   ALTER USER your_user WITH REPLICATION;

   -- Grant necessary table permissions
   GRANT SELECT ON ALL TABLES IN SCHEMA public TO your_user;
   GRANT USAGE ON SCHEMA public TO your_user;
   ```

4. **Publication Setup**:
   ```sql
   -- Create a publication for the tables you want to replicate
   CREATE PUBLICATION drasi_publication FOR TABLE table1, table2, table3;

   -- Or for all tables in the database
   CREATE PUBLICATION drasi_publication FOR ALL TABLES;
   ```

5. **Replication Slot**: The source will automatically create a replication slot named according to the `slot_name` configuration property. If the slot already exists, it will be reused.

6. **Replica Identity**: For UPDATE and DELETE operations to include the full row data:
   ```sql
   -- For tables without primary keys or with complex keys
   ALTER TABLE your_table REPLICA IDENTITY FULL;

   -- Default behavior uses primary key (recommended)
   ALTER TABLE your_table REPLICA IDENTITY DEFAULT;
   ```

## Configuration Properties

| Property | Type | Default | Required | Description |
|----------|------|---------|----------|-------------|
| `host` | String | `"localhost"` | No | PostgreSQL server hostname or IP address |
| `port` | Number | `5432` | No | PostgreSQL server port number |
| `database` | String | - | **Yes** | Name of the PostgreSQL database to connect to |
| `user` | String | - | **Yes** | PostgreSQL username with replication privileges |
| `password` | String | `""` | No | PostgreSQL user password (empty string if not required) |
| `tables` | Array[String] | `[]` | No | List of table names to monitor (empty array monitors all tables in publication) |
| `slot_name` | String | `"drasi_slot"` | No | Name of the replication slot to use or create |
| `publication_name` | String | `"drasi_publication"` | No | Name of the PostgreSQL publication to subscribe to |
| `ssl_mode` | String | `"prefer"` | No | SSL connection mode: `"disable"`, `"prefer"`, `"require"` |
| `table_keys` | Array[Object] | `[]` | No | Manual primary key configuration for tables (see Table Keys Configuration below) |

### Table Keys Configuration

The `table_keys` property allows you to manually specify primary key columns for tables. This is useful when:
- Tables don't have a defined primary key
- You want to use a different set of columns for element ID generation
- You need composite keys beyond what's defined in the database

Each object in the `table_keys` array has the following structure:

| Property | Type | Required | Description |
|----------|------|----------|-------------|
| `table` | String | **Yes** | Fully qualified table name (e.g., `"users"` or `"schema.users"`) |
| `key_columns` | Array[String] | **Yes** | List of column names to use as the primary key |

**Example**:
```yaml
table_keys:
  - table: "users"
    key_columns: ["user_id"]
  - table: "events"
    key_columns: ["event_id", "timestamp"]
```

User-configured keys take precedence over automatically detected primary keys from PostgreSQL system catalogs.

## Configuration Examples

### YAML Configuration

```yaml
sources:
  - id: postgres_source_1
    source_type: postgres
    properties:
      host: "postgres.example.com"
      port: 5432
      database: "production_db"
      user: "drasi_user"
      password: "secure_password"
      tables:
        - "orders"
        - "customers"
        - "products"
      slot_name: "drasi_production_slot"
      publication_name: "drasi_publication"
      ssl_mode: "require"
      table_keys:
        - table: "orders"
          key_columns: ["order_id"]
        - table: "line_items"
          key_columns: ["order_id", "line_number"]
```

### JSON Configuration

```json
{
  "sources": [
    {
      "id": "postgres_source_1",
      "source_type": "postgres",
      "properties": {
        "host": "postgres.example.com",
        "port": 5432,
        "database": "production_db",
        "user": "drasi_user",
        "password": "secure_password",
        "tables": ["orders", "customers", "products"],
        "slot_name": "drasi_production_slot",
        "publication_name": "drasi_publication",
        "ssl_mode": "require",
        "table_keys": [
          {
            "table": "orders",
            "key_columns": ["order_id"]
          },
          {
            "table": "line_items",
            "key_columns": ["order_id", "line_number"]
          }
        ]
      }
    }
  ]
}
```

### Minimal Configuration

```yaml
sources:
  - id: postgres_local
    source_type: postgres
    properties:
      database: "myapp"
      user: "myapp_user"
      password: "myapp_password"
```

## Programmatic Construction (Rust)

```rust
use drasi_server_core::sources::postgres::{PostgresReplicationSource, PostgresReplicationConfig, TableKeyConfig};
use drasi_server_core::config::SourceConfig;
use drasi_server_core::channels::{SourceEventSender, ComponentEventSender};
use std::collections::HashMap;
use serde_json::json;

// Method 1: Using SourceConfig with JSON properties
let mut properties = HashMap::new();
properties.insert("host".to_string(), json!("localhost"));
properties.insert("port".to_string(), json!(5432));
properties.insert("database".to_string(), json!("mydb"));
properties.insert("user".to_string(), json!("postgres"));
properties.insert("password".to_string(), json!("password"));
properties.insert("tables".to_string(), json!(["users", "orders"]));
properties.insert("slot_name".to_string(), json!("my_slot"));
properties.insert("publication_name".to_string(), json!("my_publication"));

let config = SourceConfig {
    id: "postgres_source".to_string(),
    source_type: "postgres".to_string(),
    properties,
    bootstrap_provider: None,
};

let source = PostgresReplicationSource::new(
    config,
    source_event_tx,
    event_tx,
);

// Method 2: Using PostgresReplicationConfig directly (internal)
let table_keys = vec![
    TableKeyConfig {
        table: "users".to_string(),
        key_columns: vec!["user_id".to_string()],
    },
];

let pg_config = PostgresReplicationConfig {
    host: "localhost".to_string(),
    port: 5432,
    database: "mydb".to_string(),
    user: "postgres".to_string(),
    password: "password".to_string(),
    tables: vec!["users".to_string(), "orders".to_string()],
    slot_name: "my_slot".to_string(),
    publication_name: "my_publication".to_string(),
    ssl_mode: "prefer".to_string(),
    table_keys,
};

// Start the source
source.start().await?;

// Later, stop the source
source.stop().await?;
```

## Data Formats

### Input Format: PostgreSQL WAL (Write-Ahead Log)

The source consumes logical replication messages from PostgreSQL's WAL using the `pgoutput` output plugin. The WAL stream includes:

- **Transaction Boundaries**: BEGIN and COMMIT messages
- **Relation Metadata**: Table schema information with column names and types
- **DML Operations**: INSERT, UPDATE, DELETE operations
- **Tuple Data**: Row data in binary or text format

**WAL Message Types**:
- `B` - Begin transaction
- `C` - Commit transaction
- `R` - Relation definition
- `I` - Insert operation
- `U` - Update operation
- `D` - Delete operation
- `T` - Truncate operation

### Output Format: SourceChange Events

The source transforms WAL messages into Drasi `SourceChange` events. All changes are grouped by transaction and emitted atomically.

**SourceChange Variants**:

1. **Insert**:
   ```rust
   SourceChange::Insert {
       element: Element::Node {
           metadata: ElementMetadata {
               reference: ElementReference { source_id, element_id },
               labels: ["TableName"],
               effective_from: timestamp_nanos,
           },
           properties: ElementPropertyMap { ... }
       }
   }
   ```

2. **Update**:
   ```rust
   SourceChange::Update {
       element: Element::Node {
           metadata: ElementMetadata { ... },
           properties: ElementPropertyMap { ... }  // New values
       }
   }
   ```

3. **Delete**:
   ```rust
   SourceChange::Delete {
       metadata: ElementMetadata {
           reference: ElementReference { source_id, element_id },
           labels: ["TableName"],
           effective_from: timestamp_nanos,
       }
   }
   ```

**Element ID Generation**:
- Format: `"table_name:primary_key_value"` or `"table_name:pk1_pk2_..."` for composite keys
- Examples:
  - Single PK: `"users:12345"`
  - Composite PK: `"order_items:1001_5"`
  - No PK (fallback): `"events:550e8400-e29b-41d4-a716-446655440000"`

**Labels**:
- Each element is labeled with its table name (preserving case)
- Example: A row from table `users` gets label `"users"`

### Type Mappings: PostgreSQL to Drasi ElementValue

| PostgreSQL Type | OID | Drasi ElementValue | Notes |
|-----------------|-----|-------------------|--------|
| `boolean` | 16 | `Bool(bool)` | |
| `smallint` (int2) | 21 | `Integer(i64)` | Cast from i16 |
| `integer` (int4) | 23 | `Integer(i64)` | Cast from i32 |
| `bigint` (int8) | 20 | `Integer(i64)` | |
| `real` (float4) | 700 | `Float(OrderedFloat<f64>)` | Cast from f32 |
| `double precision` (float8) | 701 | `Float(OrderedFloat<f64>)` | |
| `numeric` / `decimal` | 1700 | `Float(OrderedFloat<f64>)` | Converted via string parsing |
| `text` | 25 | `String(Arc<str>)` | |
| `varchar` | 1043 | `String(Arc<str>)` | |
| `char` / `bpchar` | 1042 | `String(Arc<str>)` | Trimmed |
| `name` | 19 | `String(Arc<str>)` | |
| `uuid` | 2950 | `String(Arc<str>)` | Formatted as hyphenated string |
| `timestamp` | 1114 | `String(Arc<str>)` | ISO 8601 format |
| `timestamptz` | 1184 | `String(Arc<str>)` | RFC 3339 format with timezone |
| `date` | 1082 | `String(Arc<str>)` | YYYY-MM-DD format |
| `time` | 1083 | `String(Arc<str>)` | HH:MM:SS format |
| `json` | 114 | `String(Arc<str>)` | JSON serialized as string |
| `jsonb` | 3802 | `String(Arc<str>)` | JSON serialized as string |
| `bytea` | 17 | `String(Arc<str>)` | Base64 encoded |
| Unknown types | - | `String(Arc<str>)` | Text representation fallback |
| NULL values | - | `Null` | |

### Example JSON Representation

**INSERT Event**:
```json
{
  "source_id": "postgres_source_1",
  "event": {
    "Change": {
      "Insert": {
        "element": {
          "Node": {
            "metadata": {
              "reference": {
                "source_id": "postgres_source_1",
                "element_id": "users:12345"
              },
              "labels": ["users"],
              "effective_from": 1704067200000000000
            },
            "properties": {
              "user_id": { "Integer": 12345 },
              "username": { "String": "john_doe" },
              "email": { "String": "john@example.com" },
              "created_at": { "String": "2024-01-01T00:00:00Z" },
              "is_active": { "Bool": true }
            }
          }
        }
      }
    }
  },
  "timestamp": "2024-01-01T00:00:00Z"
}
```

**UPDATE Event**:
```json
{
  "source_id": "postgres_source_1",
  "event": {
    "Change": {
      "Update": {
        "element": {
          "Node": {
            "metadata": {
              "reference": {
                "source_id": "postgres_source_1",
                "element_id": "users:12345"
              },
              "labels": ["users"],
              "effective_from": 1704153600000000000
            },
            "properties": {
              "user_id": { "Integer": 12345 },
              "username": { "String": "john_doe" },
              "email": { "String": "john.doe@example.com" },
              "created_at": { "String": "2024-01-01T00:00:00Z" },
              "is_active": { "Bool": true }
            }
          }
        }
      }
    }
  },
  "timestamp": "2024-01-02T00:00:00Z"
}
```

**DELETE Event**:
```json
{
  "source_id": "postgres_source_1",
  "event": {
    "Change": {
      "Delete": {
        "metadata": {
          "reference": {
            "source_id": "postgres_source_1",
            "element_id": "users:12345"
          },
          "labels": ["users"],
          "effective_from": 1704240000000000000
        }
      }
    }
  },
  "timestamp": "2024-01-03T00:00:00Z"
}
```

## Setup Guide

### Step 1: Configure PostgreSQL

1. Edit `postgresql.conf`:
   ```bash
   sudo nano /etc/postgresql/15/main/postgresql.conf
   ```

2. Set the required parameters:
   ```ini
   wal_level = logical
   max_replication_slots = 10
   max_wal_senders = 10
   ```

3. Restart PostgreSQL:
   ```bash
   sudo systemctl restart postgresql
   ```

### Step 2: Create Database User

```sql
-- Create user with replication privileges
CREATE USER drasi_user WITH REPLICATION PASSWORD 'secure_password';

-- Grant necessary permissions
GRANT SELECT ON ALL TABLES IN SCHEMA public TO drasi_user;
GRANT USAGE ON SCHEMA public TO drasi_user;

-- For future tables
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT ON TABLES TO drasi_user;
```

### Step 3: Create Publication

```sql
-- For specific tables
CREATE PUBLICATION drasi_publication FOR TABLE users, orders, products;

-- Or for all tables
CREATE PUBLICATION drasi_publication FOR ALL TABLES;

-- Verify publication
SELECT * FROM pg_publication;
SELECT * FROM pg_publication_tables WHERE pubname = 'drasi_publication';
```

### Step 4: Configure Replica Identity (Optional but Recommended)

For tables where you need full row data on UPDATE/DELETE:

```sql
-- Check current replica identity
SELECT schemaname, tablename, replicaidentity
FROM pg_tables
WHERE schemaname = 'public';

-- Set REPLICA IDENTITY FULL for specific tables
ALTER TABLE users REPLICA IDENTITY FULL;
ALTER TABLE orders REPLICA IDENTITY FULL;

-- Or use default (primary key only)
ALTER TABLE products REPLICA IDENTITY DEFAULT;
```

### Step 5: Configure Drasi Source

Create your source configuration file (e.g., `postgres-source.yaml`):

```yaml
sources:
  - id: my_postgres_source
    source_type: postgres
    properties:
      host: "localhost"
      port: 5432
      database: "myapp"
      user: "drasi_user"
      password: "secure_password"
      publication_name: "drasi_publication"
      slot_name: "drasi_myapp_slot"
```

### Step 6: Monitor Replication

```sql
-- Check replication slots
SELECT * FROM pg_replication_slots;

-- Check WAL sender processes
SELECT * FROM pg_stat_replication;

-- Monitor replication lag
SELECT slot_name,
       pg_current_wal_lsn() AS current_lsn,
       confirmed_flush_lsn,
       pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn) AS lag_bytes
FROM pg_replication_slots;
```

## Troubleshooting

### Issue: "permission denied to create replication slot"

**Cause**: User does not have REPLICATION privilege.

**Solution**:
```sql
ALTER USER your_user WITH REPLICATION;
```

### Issue: "logical decoding requires wal_level >= logical"

**Cause**: PostgreSQL is not configured for logical replication.

**Solution**:
1. Edit `postgresql.conf` and set `wal_level = logical`
2. Restart PostgreSQL: `sudo systemctl restart postgresql`
3. Verify: `SHOW wal_level;` should return `logical`

### Issue: "replication slot already exists"

**Cause**: A replication slot with the configured name already exists.

**Solution**:
```sql
-- View existing slots
SELECT * FROM pg_replication_slots WHERE slot_name = 'your_slot_name';

-- Option 1: Drop the existing slot (data loss)
SELECT pg_drop_replication_slot('your_slot_name');

-- Option 2: Use a different slot_name in your configuration

-- Option 3: Reuse the existing slot (source will continue from last position)
-- Just start the source - it will automatically use the existing slot
```

### Issue: "UPDATE/DELETE operations missing old tuple data"

**Cause**: Table is using `REPLICA IDENTITY DEFAULT` (primary key only) but you need full row data.

**Solution**:
```sql
-- Set REPLICA IDENTITY to FULL
ALTER TABLE your_table REPLICA IDENTITY FULL;

-- Verify
SELECT schemaname, tablename, replicaidentity
FROM pg_tables
WHERE tablename = 'your_table';
```

### Issue: Connection failed with SSL errors

**Cause**: SSL configuration mismatch between client and server.

**Solution**:
```yaml
# Try different ssl_mode values
properties:
  ssl_mode: "disable"   # No SSL (not recommended for production)
  # or
  ssl_mode: "prefer"    # Try SSL, fall back to non-SSL
  # or
  ssl_mode: "require"   # Require SSL
```

### Issue: "No primary key found for table 'tablename'"

**Cause**: Table has no primary key defined and no manual configuration provided.

**Solution**:
```yaml
# Option 1: Add primary key to table (recommended)
# ALTER TABLE tablename ADD PRIMARY KEY (id);

# Option 2: Configure manual keys
properties:
  table_keys:
    - table: "tablename"
      key_columns: ["id"]
```

### Issue: High replication lag

**Cause**: Source is not processing messages fast enough or network latency.

**Solution**:
1. Check replication lag:
   ```sql
   SELECT slot_name,
          pg_wal_lsn_diff(pg_current_wal_lsn(), confirmed_flush_lsn) / 1024 / 1024 AS lag_mb
   FROM pg_replication_slots;
   ```

2. Monitor source logs for errors or slow processing

3. Consider:
   - Filtering tables to reduce data volume
   - Improving network connectivity
   - Scaling your application infrastructure

### Issue: Replication slot fills up disk space

**Cause**: Replication slot is inactive or not advancing, causing WAL files to accumulate.

**Solution**:
1. Check slot status:
   ```sql
   SELECT slot_name, active, restart_lsn
   FROM pg_replication_slots;
   ```

2. If slot is inactive and no longer needed:
   ```sql
   SELECT pg_drop_replication_slot('slot_name');
   ```

3. Set max_slot_wal_keep_size to limit WAL retention:
   ```sql
   ALTER SYSTEM SET max_slot_wal_keep_size = '10GB';
   SELECT pg_reload_conf();
   ```

## Known Limitations

### Truncate Operations
- **Status**: Not implemented
- **Behavior**: TRUNCATE operations are logged with a warning but not processed into SourceChange events
- **Workaround**: Use DELETE operations if change tracking is required

### Composite and Array Types
- **Status**: Partial support
- **Behavior**: Complex PostgreSQL types (arrays, composite types) are converted to text representation
- **Impact**: May lose type information for nested structures

### Large Objects and TOAST Values
- **Status**: Partial support
- **Behavior**: Unchanged TOAST values are decoded as NULL
- **Impact**: Large text/bytea columns with TOAST storage may not capture all changes
- **Workaround**: Set REPLICA IDENTITY FULL on tables with TOAST columns

### Schema Changes
- **Status**: Not automatically handled
- **Behavior**: ALTER TABLE and other DDL operations are not captured
- **Impact**: Schema changes require source restart to pick up new table structures
- **Workaround**: Restart the source after schema modifications

### Multi-Schema Support
- **Status**: Limited
- **Behavior**: Default schema is 'public'. Other schemas require fully qualified table names
- **Example**: Use `"myschema.mytable"` in tables configuration or table_keys

### Binary Data
- **Status**: Supported with limitations
- **Behavior**: bytea (binary) columns are base64 encoded to strings
- **Impact**: Binary data is not preserved in native format

### Numeric Precision
- **Status**: Partial precision loss possible
- **Behavior**: PostgreSQL NUMERIC/DECIMAL types with very high precision are converted to float64
- **Impact**: May lose precision for numbers beyond float64 range (~15-17 decimal digits)
- **Workaround**: Consider using string representation for high-precision financial data

## Performance Considerations

### Network Latency
- The source uses TCP connections to stream WAL data
- High network latency increases replication lag
- Consider co-locating the source application with the PostgreSQL server

### Transaction Size
- Large transactions with many changes are processed atomically
- Memory usage scales with transaction size
- Consider breaking up bulk operations into smaller transactions

### Replication Slot Retention
- Inactive replication slots prevent WAL cleanup and can fill disk
- Configure `max_slot_wal_keep_size` to limit WAL retention
- Monitor and drop unused slots regularly

### Batch Processing
- Bootstrap operations use batching (1000 rows per batch) for efficiency
- Streaming changes are processed per transaction
- Very high throughput may require tuning system resources

### Authentication Methods
- Supported: Cleartext, MD5, SCRAM-SHA-256
- SCRAM-SHA-256 is recommended for security
- Avoid cleartext passwords in production

### Connection Management
- Source maintains a persistent replication connection
- Automatic reconnection on connection loss (5 second retry delay)
- Keepalive messages sent every 10 seconds to maintain connection

### Primary Key Requirements
- Tables without primary keys fall back to UUID generation
- UUID-based IDs are not stable across restarts
- Always define primary keys or use `table_keys` configuration for stable element IDs

### Monitoring Recommendations
1. Monitor replication lag regularly
2. Set up alerts for slot inactivity
3. Track WAL disk usage
4. Monitor source logs for errors and warnings
5. Verify transaction commit latency
