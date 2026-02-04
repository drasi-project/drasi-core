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

## Testing

```bash
cargo test -p drasi-bootstrap-mysql
```

## Security

- Configure tables explicitly with `with_tables`; this allowlist is required.
- Table names must use only letters, numbers, and underscores.
- Requested labels not in the allowlist are ignored.
