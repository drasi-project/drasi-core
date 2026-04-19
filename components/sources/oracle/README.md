# Oracle LogMiner Source

## Overview

`drasi-source-oracle` captures Oracle Database INSERT, UPDATE, and DELETE changes by polling Oracle LogMiner over SCN windows. It persists the last processed `COMMIT_SCN` in the configured Drasi state store so the source can resume after restarts.

## Prerequisites

### Oracle Client Libraries

This crate links against Oracle client libraries at **runtime**. You must install the Oracle Instant Client (Basic or Basic Light package) on any machine that runs the source plugin.

| Platform | Library | Install |
|----------|---------|---------|
| **macOS** | `libclntsh.dylib` | Download the DMG from [oracle.com/database/technologies/instant-client](https://www.oracle.com/database/technologies/instant-client.html), mount it, and copy the contents to a directory such as `~/oracle/instantclient_23_3`. Set `DYLD_LIBRARY_PATH` to point to that directory. |
| **Linux** | `libclntsh.so` | Install the `.rpm` / `.zip` from the same page or use Oracle's yum/apt repos. Set `LD_LIBRARY_PATH` to the install directory. You may also need `libaio` and `libnsl`. |
| **Windows** | `OCI.dll` | Install the Instant Client `.zip` and add its directory to `PATH`. |

If the client libraries are missing at runtime, the source will fail to start with an `ORA-DPI-1047` error.

> **Note:** The Rust `oracle` crate compiles without Oracle client libraries on the build host — they are only required at runtime.

### Oracle Database Requirements

• Oracle must be in `ARCHIVELOG` mode.
• Database-level supplemental logging must be enabled.
• Each monitored table must enable `SUPPLEMENTAL LOG DATA (ALL) COLUMNS`.

### Required Grants

```sql
GRANT CREATE SESSION TO drasi;
GRANT LOGMINING TO drasi;
GRANT SELECT ON V_$LOGMNR_CONTENTS TO drasi;
GRANT SELECT ON V_$DATABASE TO drasi;
GRANT SELECT ON V_$ARCHIVED_LOG TO drasi;
GRANT EXECUTE ON DBMS_LOGMNR TO drasi;
```

## Configuration

```yaml
source_type: oracle
properties:
  host: localhost
  port: 1521
  service: FREEPDB1
  user: system
  password: secret
  tables:
    - HR.EMPLOYEES
  poll_interval_ms: 1000
  start_position: current
```

## Behavior

• The source validates Oracle connectivity, ARCHIVELOG mode, and primary-key discovery before reporting `Running`.
• INSERT and UPDATE rows are materialized by fetching the final row image via `ROWID` after collapsing multiple changes for the same row within a poll window.
• DELETE rows are materialized from LogMiner `SQL_UNDO`.
• Element IDs use `schema:table:pk1:pk2`.
• Unqualified table names default to the configured Oracle user schema.
• `start_position: beginning` attempts to use the earliest archived log SCN available to the Oracle user.

## Testing

```bash
cargo test -p drasi-source-oracle
cargo test -p drasi-source-oracle --test integration_test -- --ignored --nocapture
```
