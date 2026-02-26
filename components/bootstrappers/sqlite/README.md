# SQLite Bootstrap Provider

## Overview

`drasi-bootstrap-sqlite` provides initial snapshot loading for SQLite-backed sources by reading table rows and emitting `SourceChange::Insert` bootstrap events.

Key capabilities:

- file-backed SQLite snapshot loading
- optional table allow-list
- table key override support for stable element IDs
- automatic PK detection via `PRAGMA table_info`

## Usage

```rust
use drasi_bootstrap_sqlite::{SqliteBootstrapProvider, TableKeyConfig};

let bootstrap = SqliteBootstrapProvider::builder()
    .with_path("data/example.db")
    .with_tables(vec!["sensors".to_string()])
    .with_table_keys(vec![TableKeyConfig {
        table: "sensors".to_string(),
        key_columns: vec!["id".to_string()],
    }])
    .build();
```

Attach the provider in `SqliteSource::builder(...).with_bootstrap_provider(bootstrap)`.

## Behavior

- For file-backed databases, the provider opens a read-only connection and streams table rows.
- For in-memory databases, bootstrap is skipped (no independent snapshot connection exists).
- Labels map directly to table names.
- Element IDs use configured keys first, then detected PK columns.

## Testing

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Bootstrap behavior is also validated through `drasi-source-sqlite` ignored integration tests.
