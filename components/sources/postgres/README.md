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

## Native complete-transaction source

`native::PostgresTransactionSource` (also exported as `NativePostgresSource`)
is a separate, opt-in Rust ComputationGraph source. It does **not** replace the
legacy `PostgresReplicationSource` per-change contract or its plugin ABI. Use a native graph/pipeline,
not the ordinary legacy source builder.

It emits one `drasi.source-transaction` envelope for each committed PostgreSQL
transaction. Repeated updates, key changes and deletions remain in that group.
A query configured with `QueryExecutionSettings::source_transactions` applies
the group atomically and publishes only its final result changes.

Bind the **same** `Arc<QuerySourceProgress>` to the source constructor and the
query's `with_source_progress` method. The query must have real atomic,
persistent storage. Every source branch must lead to that one immediate owner;
another query needs its own source/slot. Explicitly bind the source's `out`
stream and connect it to the query's complete-transaction `in` port.
WAL feedback follows this owner's committed input, never queue acceptance.
This protects query state/output recovery, not atomic external reaction effects.

The source and coordinated constructors also accept a `SourceProgressReader`.
Local readers retain the actual progress owner; an external read-only provider
does not supply local ownership proof. Source/bootstrap pairing requires the
same reader handle and read failures propagate before WAL feedback. This is
preparation for host-provided progress, not a completed native plugin ABI service.

`PostgresTransactionConfig` has these fields (required unless marked optional):

| Field | Meaning |
|-------|---------|
| `connection` | Existing `PostgresSourceConfig`, including credentials, publication, tables, keys and SSL mode. |
| `tls_ca_pem` | Optional additional trusted CA certificate in PEM form, bounded by `max_protocol_bytes` and 64 KiB. Omitted configurations keep platform trust unchanged. This does not disable hostname verification or enable TLS when the SSL mode is `Disable`. |
| `start_lsn` | Explicit initial position in replay-only mode, used only before the first committed checkpoint. Coordinated snapshot mode derives its own boundary. |
| `transactions` | Positive `max_changes`, `max_bytes` and `max_duration_ms`. Bytes bound the complete binary transaction payload; time runs from received Begin through Commit. A key change can produce two graph changes. |
| `max_protocol_bytes` | Additional limits for an individual wire frame, one catalog result set and accounted cached relation metadata; not a total process-memory limit. |
| `io_timeout_ms` | Database startup, feedback writes, snapshot I/O and incomplete wire-frame timeout. Waiting for the query's initial snapshot is not database I/O. An otherwise idle connection is allowed. |
| `feedback_interval_ms` | Heartbeat interval; configure below PostgreSQL's sender timeout. |

For replay-only mode, provision a persistent, exclusively owned `pgoutput` slot before starting.
Native operation requires PostgreSQL 13 or newer, `max_slot_wal_keep_size = -1`,
and, where available, `idle_replication_slot_timeout = 0`. Startup rejects
temporary/missing slots, unavailable cursors and unsafe automatic slot-retention
settings. Operators must preserve the slot and retained WAL across outages and
monitor disk use; this does not promise recovery after administrative deletion,
storage loss or power loss.

SSL `Prefer` negotiates TLS when offered; `Require` rejects plaintext.
TLS validates the hostname and certificate using the platform trust store plus
the optional `tls_ca_pem` certificate. The same settings protect coordinated
snapshot and streaming connections. An advertised TLS handshake or certificate
failure is an error, including in `Prefer` mode; it never retries plaintext.
There is no insecure certificate-verification bypass or change to system trust.

One owned worker keeps sending confirmed-position heartbeats while its
capacity-one output queue is full. At most one additional complete group waits
for queue capacity. Stop cancels network work and joins that owner before
restart; cancelled or timed-out cleanup cannot authorize a replacement worker.
Protocol/decoding errors, unsupported operations, deadlines and size/count limits
fail explicitly without acknowledging the rejected group. Stop/reconstruct with
higher limits to replay the whole group; no oversized disk staging or partial
publication occurs.

Commit LSNs supply stable logical identity, independently of increasing transport
sequences. Checkpoints also bind the PostgreSQL system/database, slot, publication,
selected key definitions, publication flags/filters/columns, column types and
replica identity, source ID and stream. A different binding cannot reuse old
progress. Numeric limit changes do not change that binding.

Native row IDs use the shared `drasi-postgres-common::transaction_element_id`
encoding: `pg:v1:` followed by JSON containing schema, table and sorted
column-name/key-value pairs. This avoids ambiguous underscore-joined composite
keys and random replay identities. It intentionally differs from legacy row IDs.
SQL NULL stays NULL; unchanged TOAST properties are preserved. A key-changing
update with omitted TOAST values requires a complete old image from
`REPLICA IDENTITY FULL`, otherwise it fails rather than constructing a partial row.

`PostgresTransactionSourceFactory` exposes implementation
`drasi/postgres-transactions`, version `1`. Its fields are `stream` and `settings`;
because settings include credentials, supply them through a secret
`ConfigurationResolverResource` reference, not a literal. Its required
`source_progress` dependency is the actual `QuerySourceProgressResource`.
Use `PostgresTransactionSource::describe(id)` for its port declaration.

### Coordinated initial snapshot

`PostgresTransactionSource::coordinated(id, stream, settings, progress)` returns
the native source and an `Arc<PostgresSnapshot>`. Attach that same provider to
the owner query with `with_bootstrap`. This explicit mode requires PostgreSQL
15+ and uses `connection.slot_name` as a prefix of at most 30 characters, not as
an externally managed slot name. A dedicated UUID-suffixed persistent slot is
created for this query/source pair; existing slots are never adopted.

The query first persists bounded initialization ownership metadata in its own
atomic storage. PostgreSQL then creates a logical slot with an exported snapshot;
a second, directly owned connection imports that snapshot. A server-side cursor
reads one bounded row at a time while the slot retains concurrent WAL. Snapshot
and live rows use the same key encoding and value conversion. Snapshot rows build
initial state, not live transaction notifications; they are not represented as
one fictitious upstream transaction.

Only successful stream exhaustion commits the snapshot's source boundary,
completed marker and ownership state together. The live source then starts at
that boundary and acknowledges only committed query progress. No extra worker or
local WAL staging is needed during the snapshot. Dropping the stream closes its
connection and cancels the read-only database transaction.

Interrupted initial loads remain incomplete under strict recovery. With an
explicitly selected `QueryRecoveryPolicy::AutoReset`, the query discards its
unfinished initial state and the provider replaces only its recorded, unfinished
slot. Completed slots survive stop/restart. Lost completed history, changed
bindings or missing ownership metadata fail rather than silently reinitializing.
Completed-slot retirement is an operator action: retire it before deprovisioning
the query and deleting its ownership metadata. Do not share that state or managed
slot between concurrently running query owners.

The source factory also accepts `coordinated_snapshot: true` with a `snapshot`
dependency: a `ResourceRole::Bootstrap` handle containing the paired
`PostgresSnapshot`. The query uses that same object as its bootstrap provider
(or inside `QueryBootstrapResource`). Construction checks the actual progress
resource, source, stream and settings rather than trusting resource names.

The initial implementation accepts ordinary non-partitioned tables and complete
insert/update/delete publications. It rejects row/column filters, inheritance and
partition-root remapping for coordinated snapshots. Keep schema/publication
definitions stable while running. The legacy bootstrapper is not upgraded to this
coordinated contract.

**Remaining limitations:** no native cdylib/Server registration, TRUNCATE, streaming/two-phase transaction protocol,
automatic schema/key/publication migration or cross-database transaction.
Configured keys must be complete and carried by replica identity. Runtime policy
owns restart after a failure; the source does not silently reconnect at a newer
cursor. These limitations are not upgrades to the legacy source's guarantees.
Keep table definitions and publication semantics unchanged during operation and
recovery; automatic DDL migration and failover/timeline transitions are not
qualified by this source.

`cargo test -p drasi-source-postgres --lib --test native_transactions` includes
real PostgreSQL/RocksDB cases, direct/factory graph pipes in both modes,
backpressure beyond the server timeout, cancelled stop/restart, rejected-group
recovery, concurrent snapshot/WAL handover and four required abrupt child-process
exits. Temporary, short-lived certificates qualify successful verified TLS for
streaming, coordinated initial loading and persistent restart. Tests inspect
server-side encryption and session retirement, and reject unknown CAs, wrong
hostnames, expired certificates and invalid certificate settings without advancing
query progress. The existing TLS-refusal case also remains required.

## Architecture

### Components

The PostgreSQL source consists of several specialized modules:

1. **Connection** (`connection.rs`): Manages the PostgreSQL replication protocol connection, authentication (including SCRAM-SHA-256), and message exchange
2. **Stream** (`stream.rs`): Handles the continuous WAL streaming loop, message processing, and transaction coordination
3. **Decoder** (`decoder.rs`): Decodes binary pgoutput messages into structured WAL events with full type support
4. **Protocol** (`protocol.rs`): Implements PostgreSQL wire protocol encoding/decoding
5. **SCRAM** (`scram.rs`): SCRAM-SHA-256 authentication implementation
6. **Types** (`types.rs`): Type definitions for PostgreSQL values and WAL messages

**Note**: Bootstrap functionality is provided by the separate `drasi-bootstrap-postgres` crate via the pluggable bootstrap provider pattern.

### Data Flow

```
PostgreSQL WAL → Connection → Decoder → Stream → SourceChange Events
                                ↓
                          Transaction
                          Grouping
                                ↓
                          Dispatcher → Queries
```

**Bootstrap Flow** (via pluggable bootstrap provider):
```
Bootstrap Request → Bootstrap Provider → SourceChange Events
                                              ↓
                                        Coordinate with Streaming
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

### Bootstrap Support

The PostgreSQL source supports pluggable bootstrap providers via the `BootstrapProvider` trait. Any bootstrap provider implementation can be used with this source:

```rust
use drasi_source_postgres::PostgresReplicationSource;

let source = PostgresReplicationSource::builder("pg-source")
    .with_host("localhost")
    .with_database("myapp")
    .with_user("postgres")
    .with_password("password")
    .with_bootstrap_provider(my_bootstrap_provider)  // Any BootstrapProvider impl
    .build()?;
```

Common bootstrap provider options include:
- `PostgresBootstrapProvider` (`drasi-bootstrap-postgres`) - Snapshots directly from PostgreSQL
- `ScriptFileBootstrapProvider` (`drasi-bootstrap-scriptfile`) - Loads initial data from JSONL files
- `NoopBootstrapProvider` (`drasi-bootstrap-noop`) - Skips bootstrap entirely
- Custom implementations of the `BootstrapProvider` trait

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

On connect, the streaming source queries the PostgreSQL system catalogs to
auto-detect each table's primary key columns, then derives a stable,
primary-key-based element id for every CDC change. This means INSERT, UPDATE, and
DELETE events for the same row share the same element id and correlate correctly,
even without any `table_keys` configuration. The bootstrap provider performs the
same catalog lookup, so bootstrap and CDC element ids agree.

The catalog query used for detection is:
```sql
SELECT n.nspname, c.relname, a.attname
FROM pg_constraint con
JOIN pg_class c ON con.conrelid = c.oid
JOIN pg_namespace n ON c.relnamespace = n.oid
JOIN pg_attribute a ON a.attrelid = c.oid
WHERE con.contype = 'p'
  AND a.attnum = ANY(con.conkey)
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
ORDER BY n.nspname, c.relname, array_position(con.conkey, a.attnum)
```

User-configured `table_keys` override automatically detected primary keys in both
streaming and bootstrap. A random UUID element id is used only as a last resort
for tables that have no primary key and no configured `table_keys`.

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

## Plugin Packaging

This source is compiled as a dynamic plugin (cdylib) that can be loaded by drasi-server at runtime.

**Key files:**
- `Cargo.toml` — includes `crate-type = ["lib", "cdylib"]`
- `src/descriptor.rs` — implements `SourcePluginDescriptor` with kind `"postgres"`, configuration DTO, and OpenAPI schema generation
- `src/lib.rs` — invokes `drasi_plugin_sdk::export_plugin!` to export the plugin entry point

**Building:**
```bash
cargo build -p drasi-source-postgres
```

The compiled `.so` (Linux) / `.dylib` (macOS) / `.dll` (Windows) is placed in `target/debug/` and can be copied to the server's `plugins/` directory.

For more details on the plugin descriptor pattern and configuration DTOs, see the [Source Developer Guide](../README.md#packaging-as-a-dynamic-plugin).

## License

Copyright 2025 The Drasi Authors.

Licensed under the Apache License, Version 2.0. See LICENSE file for details.
