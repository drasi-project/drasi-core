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
| `sslMode` | `SslMode` | `if_available` | SSL mode: `disabled`, `if_available`, `require`, `require_verify_ca`, `require_verify_full` (see [SSL Modes](#ssl-modes)) |
| `tableKeys` | `Vec<TableKeyConfig>` | `[]` | Manual primary key configuration |

### TableKeyConfig

| Field | Type | Description |
|-------|------|-------------|
| `table` | `String` | Table name |
| `keyColumns` | `Vec<String>` | Column names to use as primary key |

### SSL Modes

TLS support (rustls) is always compiled in — there is no feature flag to enable.
`sslMode` maps 1:1 to MySQL's `--ssl-mode`:

| Value | MySQL `--ssl-mode` | TLS | Verifies |
|-------|--------------------|-----|----------|
| `disabled` | `DISABLED` | no | — |
| `if_available` (default) | `PREFERRED` | opportunistic; falls back to plaintext | no |
| `require` | `REQUIRED` | required | no (encrypt-only) |
| `require_verify_ca` | `VERIFY_CA` | required | CA chain (skips hostname) |
| `require_verify_full` | `VERIFY_IDENTITY` | required | CA chain + hostname |

`if_available` and `require` skip certificate verification (matching MySQL), so they
protect against passive eavesdropping but not an active man-in-the-middle. Use
`require_verify_ca` or `require_verify_full` when server authenticity matters.

## Testing

```bash
cargo test -p drasi-bootstrap-mysql
```

## Security

- Configure tables explicitly with `with_tables`; this allowlist is required.
- Table names must use only letters, numbers, and underscores.
- Requested tables not in the allowlist are ignored.
