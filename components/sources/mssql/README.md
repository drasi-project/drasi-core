# MS SQL CDC Source for Drasi

A Microsoft SQL Server Change Data Capture (CDC) source plugin for the Drasi platform.

## Quick Start

```rust
use drasi_source_mssql::MsSqlSource;

let source = MsSqlSource::builder("mssql-source")
    .with_host("localhost")
    .with_port(1433)
    .with_database("MyDatabase")
    .with_user("drasi_user")
    .with_password("password")
    .with_tables(vec!["orders".to_string(), "customers".to_string()])
    .with_poll_interval_ms(1000)
    .build()?;

source.start().await?;
```

## Configuration

```yaml
host: localhost           # MS SQL Server hostname
port: 1433               # Port (default: 1433)
database: MyDatabase     # Database name
user: drasi_user         # Username
password: secret         # Password
tables:                  # Tables to monitor
  - orders
  - customers
poll_interval_ms: 1000   # CDC polling interval (default: 1000ms)
table_keys:              # Optional: Override primary keys
  - table: order_items
    key_columns:
      - order_id
      - product_id
```

## Prerequisites

### MS SQL Server Setup
1. Enable CDC on the database:
```sql
EXEC sys.sp_cdc_enable_db;
```

2. Enable CDC on tables:
```sql
EXEC sys.sp_cdc_enable_table
    @source_schema = 'dbo',
    @source_name = 'orders',
    @role_name = NULL;
```

3. Create user with permissions:
```sql
CREATE LOGIN drasi_user WITH PASSWORD = 'password';
CREATE USER drasi_user FOR LOGIN drasi_user;
GRANT SELECT ON SCHEMA::cdc TO drasi_user;
GRANT SELECT ON dbo.orders TO drasi_user;
```

## Architecture

### LSN Checkpoint Flow
```
Start → Load LSN from StateStore
     ↓
     Tail CDC (from LSN to current)
     ↓
     Process changes → Emit SourceChange events
     ↓
     Save new LSN to StateStore
     ↓
     Sleep (poll_interval_ms)
     ↓
     Repeat
```

### Element ID Generation
- **Single PK**: `{table}:{pk_value}` (e.g., `orders:12345`)
- **Composite PK**: `{table}:{pk1}_{pk2}` (e.g., `order_items:12345_67890`)
- **No PK**: `{table}:{uuid}` (fallback with warning)

### CDC Operations
- **DELETE** (op=1): Before image of deleted row
- **INSERT** (op=2): After image of inserted row
- **UPDATE** (op=4): After image of updated row

## Testing

```bash
# Run unit tests
cargo test -p drasi-source-mssql

# Run with real MS SQL (requires instance)
cargo test -p drasi-source-mssql -- --ignored
```

## Modules

- `config.rs` - Configuration structs and builder
- `connection.rs` - Tiberius client wrapper
- `lsn.rs` - LSN type and persistence
- `decoder.rs` - CDC operation types
- `keys.rs` - Primary key discovery
- `types.rs` - Type conversion
- `lib.rs` - Main source implementation


## Dependencies

- `drasi-lib` - Core Drasi types and traits
- `drasi-core` - Element types and models
- `tiberius` - MS SQL client library
- `tokio` - Async runtime
- `serde` - Serialization
- `chrono` - DateTime handling
- `uuid` - UUID generation

## Documentation

- `README.md` - This file
- `IMPLEMENTATION_COMPLETE.md` - Comprehensive implementation summary
- `mssql-implementation-plan.md` - Original detailed plan with CDC research

## License

Apache License 2.0

## Authors

Drasi Contributors

---

**Status**: Production-quality foundation with ~82% completion
