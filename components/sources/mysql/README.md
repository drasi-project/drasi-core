# MySQL Source Plugin

Streams MySQL binlog changes into Drasi `SourceChange` events. Uses `mysql_cdc` for row-based replication.

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

## Limitations

- `mysql_cdc` runtime does not support SSL (ssl_mode must be Disabled). This means connections from this source to MySQL are **not encrypted**.
  - **Do not** use this source over untrusted networks or the public internet.
  - In production, run it only on trusted, isolated networks and ensure encryption in transit via infrastructure such as a VPN, service mesh, or a TLS-terminating proxy (for example, stunnel, HAProxy, or a cloud load balancer that terminates MySQL TLS and forwards plain traffic on a private network).
- Packets >16MB are not supported

## Testing

Integration test uses testcontainers:

```bash
cargo test -p drasi-source-mysql -- --ignored --nocapture
```
