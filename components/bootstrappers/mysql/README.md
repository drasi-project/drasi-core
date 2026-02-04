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
