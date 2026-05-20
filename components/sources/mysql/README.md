# MySQL Source Plugin

Streams MySQL binlog changes into Drasi `SourceChange` events. Uses `mysql_async`'s native `BinlogStream` API for row-based replication.

## Requirements

- MySQL with binlog enabled
- `binlog_format=ROW`
- `binlog_row_image=FULL`
- `binlog_row_metadata=FULL`
- Replication user with `REPLICATION SLAVE` and `REPLICATION CLIENT`

## Example

```rust
use drasi_source_mysql::{MySqlReplicationSource, StartPosition};

let source = MySqlReplicationSource::builder("mysql-source")
    .with_host("localhost")
    .with_port(3306)
    .with_database("test")
    .with_user("replication_user")
    .with_password("secret")
    .with_tables(vec!["users".to_string()])
    .with_start_position(StartPosition::FromEnd)
    .build()?;
```

## Configuration Options

| Property | Type | Default | Description |
|----------|------|---------|-------------|
| `host` | `String` | `"localhost"` | MySQL server hostname or IP address |
| `port` | `u16` | `3306` | MySQL server port number |
| `database` | `String` | **(Required)** | Database name to connect to |
| `user` | `String` | **(Required)** | Database user with replication privileges |
| `password` | `String` | `""` | Database password |
| `tables` | `Vec<String>` | `[]` | List of tables to monitor |
| `sslMode` | `SslMode` | `disabled` | SSL mode: `disabled`, `if_available`, `require`, `require_verify_ca`, `require_verify_full` |
| `tableKeys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration (see below) |
| `startPosition` | `StartPosition` | `from_end` | Where to start replication: `from_start`, `from_end`, `from_position`, or `from_gtid` |
| `serverId` | `u32` | Auto-generated | MySQL server ID for the replication connection. Auto-generated from source instance ID if not specified. |
| `heartbeatIntervalSeconds` | `u64` | `30` | Heartbeat interval in seconds |

### TableKeyConfig

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name |
| `keyColumns` | `Vec<String>` | Column names to use as primary key |

## Limitations

- Packets > 16 MB are not supported.

## Testing

Integration test uses testcontainers:

```bash
cargo test -p drasi-source-mysql -- --ignored --nocapture
```
