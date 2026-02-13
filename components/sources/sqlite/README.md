# SQLite Source

## Overview

`drasi-source-sqlite` is a protocol/local source that owns an embedded SQLite connection and emits Drasi `SourceChange` events for row-level `INSERT`, `UPDATE`, and `DELETE` activity.

Key capabilities:

- Real-time CDC using `rusqlite` `preupdate_hook`
- Transaction-aware dispatch (`commit_hook` flush, `rollback_hook` discard)
- Savepoint-aware buffering for partial rollback support
- Optional table filtering and explicit table key configuration
- Optional REST API for table CRUD and transactional batch operations
- Works with file-backed and in-memory SQLite databases

## Quick Start

```rust
use drasi_source_sqlite::{SqliteSource, TableKeyConfig, RestApiConfig};

let source = SqliteSource::builder("sqlite-source")
    .with_path("data/example.db")
    .with_table_keys(vec![TableKeyConfig {
        table: "sensors".to_string(),
        key_columns: vec!["id".to_string()],
    }])
    .with_rest_api(RestApiConfig {
        host: "127.0.0.1".to_string(),
        port: 9100,
    })
    .build()?;
```

## SQL Handle API

Use `source.handle()` to execute statements through the source-owned SQLite connection:

```rust
let handle = source.handle();
handle.execute("CREATE TABLE sensors(id INTEGER PRIMARY KEY, name TEXT, temp REAL)").await?;
handle.execute("INSERT INTO sensors(id, name, temp) VALUES (1, 'sensor-a', 31.5)").await?;

handle.transaction(|tx| async move {
    tx.execute("INSERT INTO sensors(id, name, temp) VALUES (2, 'sensor-b', 30.0)").await?;
    tx.execute("UPDATE sensors SET temp = 32.1 WHERE id = 1").await?;
    Ok(())
}).await?;
```

## REST API (Optional)

When `with_rest_api()` is configured:

- `GET /health`
- `GET /api/tables`
- `GET /api/tables/{table}`
- `GET /api/tables/{table}/{id}`
- `POST /api/tables/{table}`
- `PUT /api/tables/{table}/{id}`
- `DELETE /api/tables/{table}/{id}`
- `POST /api/batch` (single SQLite transaction for all operations)

## Configuration Notes

- `tables: Option<Vec<String>>` filters monitored tables. `None` means all user tables.
- `table_keys` defines element ID columns per table; if omitted, table PKs are detected from `PRAGMA table_info`.
- If no configured/detected keys exist, rowid is used as fallback for streaming events.

## Testing

From this crate directory:

```bash
cargo test
cargo test -- --ignored --nocapture
cargo clippy --all-targets -- -D warnings
```

The ignored integration tests validate:

- handle-driven INSERT/UPDATE/DELETE flow
- REST CRUD and transactional batch behavior
- bootstrap delivery into query results
- multi-table query routing

## Limitations

- CDC only captures writes executed through the source-owned connection.
- DDL (`CREATE/ALTER/DROP TABLE`) is not emitted as `SourceChange`.
- BLOB values are emitted as base64-encoded strings.

## Troubleshooting

- `source ... is not running`: call `core.start()` before using `SqliteSourceHandle`.
- no events after writes: verify writes go through the source handle or REST API, not an external SQLite connection.
- REST 400 responses: table/column identifiers failed validation.
