# Oracle Getting Started Example

## Prerequisites

This example requires the **Oracle Instant Client** (Basic or Basic Light) to be installed on your machine:

| Platform | Steps |
|----------|-------|
| **macOS** | Download the DMG from [oracle.com](https://www.oracle.com/database/technologies/instant-client.html), mount it, and copy the contents to a directory (e.g. `~/oracle/instantclient_23_3`). Then export `DYLD_LIBRARY_PATH` to point there. |
| **Linux** | Install the `.rpm` or `.zip` package and export `LD_LIBRARY_PATH`. You may also need `libaio` and `libnsl`. |
| **Windows** | Install the `.zip` package and add its directory to `PATH`. |

If the client libraries are missing you will see an `ORA-DPI-1047` error at startup.

You also need **Docker** to run the Oracle container used by `setup.sh`.

## Quick Start

```bash
./setup.sh
export DYLD_LIBRARY_PATH=$HOME/oracle/instantclient_23_3:$DYLD_LIBRARY_PATH  # adjust path as needed
export ORACLE_PORT=1522
cargo run --manifest-path ./Cargo.toml
```

## How to Verify It's Working

1. Start the example.
2. In another shell run `./test-updates.sh`.
3. Watch the log reaction output for add, update, and delete notifications for `drasi_products`.

## Helper Scripts

- `setup.sh` starts Oracle, waits for readiness, enables ARCHIVELOG, enables supplemental logging, and recreates `SYSTEM.DRASI_PRODUCTS`.
- `quickstart.sh` runs setup and then starts the example.
- `diagnose.sh` prints Oracle health and logging configuration.
- `test-updates.sh` issues INSERT, UPDATE, and DELETE statements.

## Troubleshooting

- If the example fails with `DPI-1047`, Oracle Instant Client is not available on the host. Install it and set `DYLD_LIBRARY_PATH` or `LD_LIBRARY_PATH`.
- If Oracle startup takes longer than expected, rerun `./diagnose.sh` and inspect `docker logs drasi-oracle-example`.
- If no changes arrive, confirm `diagnose.sh` shows `ARCHIVELOG` and supplemental logging enabled.
