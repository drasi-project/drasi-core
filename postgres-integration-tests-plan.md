# PostgreSQL Source Integration Tests Implementation Plan

## 1. Overview

The PostgreSQL source (`drasi-source-postgres`) currently lacks integration tests that verify end-to-end functionality with a real PostgreSQL database. This plan adds comprehensive integration tests using testcontainers to validate:

- Connection and replication setup
- INSERT/UPDATE/DELETE change detection via logical replication
- Data mapping to Drasi source changes
- Full DrasiLib integration with queries

## 2. Current State Analysis

### Existing Components
- **Source Location**: `components/sources/postgres/`
- **Core Implementation**: `src/lib.rs` (PostgresReplicationSource, PostgresSourceBuilder)
- **Replication Stream**: `src/stream.rs` (WAL decoding via pgoutput)
- **Unit Tests**: Basic construction tests in `src/lib.rs` (lines 678-800)

### Reference Patterns
- **Postgres Helpers**: `components/reactions/storedproc-postgres/tests/postgres_helpers.rs`
  - Uses `testcontainers-modules::postgres::Postgres`
  - `PostgresGuard` pattern for container lifecycle management
- **Integration Test Style**: `components/reactions/sse/tests/integration_tests.rs`
  - Full DrasiLib setup with sources, queries, reactions

### Key Challenges
1. PostgreSQL logical replication requires `wal_level = logical`
2. Need to create publication and replication slot
3. Container must be configured for replication before source connects

## 3. Architecture

### Test Directory Structure
```
components/sources/postgres/
├── src/
│   └── lib.rs  (existing)
├── tests/
│   ├── postgres_helpers.rs      # Container setup with replication config
│   ├── integration_tests.rs     # Full integration tests
│   └── mod.rs                   # Test module declaration
├── Cargo.toml  (updated with dev-dependencies)
└── README.md
```

### PostgreSQL Container Configuration

The standard `postgres` container needs custom configuration for logical replication:

```rust
// Custom container with logical replication enabled
let container = GenericImage::new("postgres", "16-alpine")
    .with_env_var("POSTGRES_PASSWORD", "postgres")
    .with_env_var("POSTGRES_DB", "testdb")
    .with_cmd([
        "-c", "wal_level=logical",
        "-c", "max_replication_slots=10",
        "-c", "max_wal_senders=10",
    ])
    .start().await?;
```

## 4. Implementation Phases

### Phase 1: Test Infrastructure
- [ ] Add dev-dependencies to `Cargo.toml`
- [ ] Create `tests/postgres_helpers.rs` with replication-enabled container setup
- [ ] Create `tests/mod.rs` for test module organization

### Phase 2: Basic Connection Tests
- [ ] Test source connects to PostgreSQL successfully
- [ ] Test replication slot creation
- [ ] Test publication setup

### Phase 3: CDC Integration Tests
- [ ] Test INSERT detection via logical replication
- [ ] Test UPDATE detection via logical replication  
- [ ] Test DELETE detection via logical replication
- [ ] Test multiple table replication

### Phase 4: Full DrasiLib Integration
- [ ] Test PostgreSQL source with DrasiLib and query
- [ ] Verify query results update on data changes
- [ ] Test bootstrap data loading

### Phase 5: Edge Cases & Cleanup
- [ ] Test reconnection handling
- [ ] Test large batch processing
- [ ] Documentation updates

## 5. Detailed Specifications

### 5.1 Cargo.toml Updates

```toml
[dev-dependencies]
tokio-test = "0.4"
env_logger = "0.10.0"
serial_test = "3.0"
testcontainers = "0.23"
testcontainers-modules = { version = "0.11", features = ["postgres"] }
```

### 5.2 postgres_helpers.rs

Key components:
- `ReplicationPostgresConfig` struct with connection details
- `setup_replication_postgres()` function returning configured container
- `ReplicationPostgresGuard` for lifecycle management
- Helper functions:
  - `create_test_table(client, table_name)` 
  - `create_publication(client, publication_name, tables)`
  - `insert_test_row(client, table, id, name)`
  - `update_test_row(client, table, id, new_name)`
  - `delete_test_row(client, table, id)`

### 5.3 Integration Test Scenarios

#### Test 1: `test_source_connects_and_starts`
```rust
#[tokio::test]
#[serial]
async fn test_source_connects_and_starts() {
    // 1. Setup container with replication enabled
    // 2. Create table and publication
    // 3. Build PostgresReplicationSource
    // 4. Start source and verify status is Running
    // 5. Cleanup
}
```

#### Test 2: `test_insert_detection`
```rust
#[tokio::test]
#[serial]
async fn test_insert_detection() {
    // 1. Setup container, table, publication
    // 2. Create source with ApplicationSource collector pattern
    // 3. Start DrasiLib with postgres source and query
    // 4. INSERT row into test table
    // 5. Verify query results contain new row
    // 6. Cleanup
}
```

#### Test 3: `test_update_detection`
```rust
#[tokio::test]
#[serial]
async fn test_update_detection() {
    // 1. Setup with existing row
    // 2. Start DrasiLib  
    // 3. UPDATE the row
    // 4. Verify query results reflect update
    // 5. Cleanup
}
```

#### Test 4: `test_delete_detection`
```rust
#[tokio::test]
#[serial]
async fn test_delete_detection() {
    // 1. Setup with existing row
    // 2. Start DrasiLib
    // 3. DELETE the row
    // 4. Verify row removed from query results
    // 5. Cleanup
}
```

#### Test 5: `test_full_crud_cycle`
```rust
#[tokio::test]
#[serial]
async fn test_full_crud_cycle() {
    // 1. Setup container
    // 2. Start DrasiLib with postgres source + query
    // 3. INSERT -> verify added
    // 4. UPDATE -> verify changed
    // 5. DELETE -> verify removed
    // 6. Cleanup
}
```

### 5.4 Test Query Pattern

```rust
let query = Query::cypher("test-query")
    .query(r#"
        MATCH (u:users)
        RETURN u.id AS id, u.name AS name
    "#)
    .from_source("pg-test-source")
    .auto_start(true)
    .build();
```

### 5.5 Change Verification Strategy

Use `DrasiLib::get_query_results()` to verify changes:

```rust
// Wait for replication to process
tokio::time::sleep(Duration::from_millis(500)).await;

// Get current query results
let results = core.get_query_results("test-query").await?;

// Verify expected data
assert_eq!(results.len(), 1);
assert_eq!(results[0]["name"], "Alice");
```

## 6. Database Setup SQL

```sql
-- Create test table with REPLICA IDENTITY FULL for UPDATE/DELETE
CREATE TABLE users (
    id SERIAL PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    email VARCHAR(255)
);
ALTER TABLE users REPLICA IDENTITY FULL;

-- Create publication for the table
CREATE PUBLICATION drasi_test_pub FOR TABLE users;

-- Verify replication slot (created by source)
SELECT * FROM pg_replication_slots;
```

## 7. Test Execution

```bash
# Run all postgres source tests
cargo test -p drasi-source-postgres

# Run integration tests only
cargo test -p drasi-source-postgres --test integration_tests

# Run with logging
RUST_LOG=debug cargo test -p drasi-source-postgres --test integration_tests -- --nocapture
```

## 8. Known Considerations

### Timing Sensitivity
- Logical replication has inherent latency
- Tests should use appropriate waits (500ms-2s) between operations
- Use retry loops for verification rather than fixed delays

### Container Cleanup
- Use `PostgresGuard` pattern with explicit `cleanup()` calls
- `serial_test` ensures tests don't conflict

### Replication Slot Management
- Each test should use unique slot names OR clean up slots
- Stale slots can cause issues if not properly removed

### REPLICA IDENTITY
- Tables need `REPLICA IDENTITY FULL` for UPDATE/DELETE to include before values
- Alternatively, primary key is sufficient for basic operations

## 9. Definition of Done

- [ ] All dev-dependencies added to `Cargo.toml`
- [ ] `tests/postgres_helpers.rs` implemented with replication-enabled container
- [ ] At least 5 integration tests covering INSERT/UPDATE/DELETE
- [ ] Tests pass reliably with `cargo test -p drasi-source-postgres`
- [ ] No interference with existing unit tests
- [ ] Code follows project conventions (license headers, formatting)

## 10. Files to Create/Modify

| File | Action | Description |
|------|--------|-------------|
| `components/sources/postgres/Cargo.toml` | Modify | Add dev-dependencies |
| `components/sources/postgres/tests/mod.rs` | Create | Test module declaration |
| `components/sources/postgres/tests/postgres_helpers.rs` | Create | Container setup helpers |
| `components/sources/postgres/tests/integration_tests.rs` | Create | Integration test suite |

## 11. References

- Existing postgres helpers: `components/reactions/storedproc-postgres/tests/postgres_helpers.rs`
- SSE integration tests: `components/reactions/sse/tests/integration_tests.rs`
- DrasiLib builder example: `examples/lib/builder/main.rs`
- testcontainers-modules postgres: Uses `postgres:16-alpine` image
