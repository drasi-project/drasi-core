# MySQL Bootstrap Provider

Provides a snapshot of MySQL tables for initial query bootstrap.

## Example

```rust
use drasi_bootstrap_mysql::MySqlBootstrapProvider;

let bootstrap = MySqlBootstrapProvider::builder()
    .with_host("localhost")
    .with_database("test")
    .with_user("replication_user")
    .with_password("secret")
    .with_tables(vec!["users".to_string()])
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
| `tables` | `Vec<String>` | **(Required)** | Table allow-list; must contain at least one table |
| `tableKeys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration |

### TableKeyConfig

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name |
| `keyColumns` | `Vec<String>` | Column names to use as primary key |

## Testing

```bash
cargo test -p drasi-bootstrap-mysql
```

## Security

- Configure tables explicitly with `with_tables`; this allowlist is required.
- Table names must use only letters, numbers, and underscores.
- Requested tables not in the allowlist are ignored.
