# Real SQL Server qualification fixture

**These expanded scenarios are authored/compiled, not database-qualified.**
Do not interpret compilation, metadata-only checks, or an unavailable database as
a passing SQL Server proof. No new local emulation attempts are required.

**Qualification is now explicitly deferred by the user.** The environment
instructions below are retained for a future authorized resumption, not current
permission to connect to a local or remote database.

## Explicitly approved external instance

The fixture never discovers a connection, adopts an existing application
database, creates/drops an external database, or enables database-level CDC.
An administrator first provisions a **dedicated, isolated test database**.
Every test then creates a fresh, uniquely suffixed schema inside it. Cleanup
verifies that schema's per-run `__DrasiOwned` token before disabling its own CDC
captures and dropping only the known fixture objects. Unexpected remaining
objects stop schema removal for manual review.

All of these inputs must be supplied explicitly:

| Environment variable | Required value |
|---|---|
| `DRASI_MSSQL_TEST_MODE` | `external` |
| `DRASI_MSSQL_TEST_ALLOW_EXTERNAL` | Exact consent string `isolated-database` |
| `DRASI_MSSQL_TEST_HOST` | Approved SQL Server DNS name/IP, without an inferred default |
| `DRASI_MSSQL_TEST_PORT` | Explicit positive TCP port, commonly `1433` |
| `DRASI_MSSQL_TEST_DATABASE` | Pre-provisioned isolated database; identifier must start `drasi_native_` |
| `DRASI_MSSQL_TEST_SCHEMA_PREFIX` | Lowercase identifier starting `drasi_native_`, at most 28 characters |
| `DRASI_MSSQL_TEST_OWNERSHIP_TOKEN` | 32–256 ASCII letters/digits, underscore or hyphen; must exactly match the database extended property described below |
| `DRASI_MSSQL_TEST_SQLCMD` | Explicit path/name of an installed ODBC `sqlcmd` supporting `-f 65001`, `-y 0`, `-t`, and `SQLCMDPASSWORD` |
| `DRASI_MSSQL_TEST_TRUST_CERTIFICATE` | Explicit `true` or `false`; use `false` with a certificate trusted by both ODBC and the native Rust TLS backend |
| `DRASI_MSSQL_TEST_ADMIN_USER` / `DRASI_MSSQL_TEST_ADMIN_PASSWORD` | Dedicated fixture administrator SQL login |
| `DRASI_MSSQL_TEST_APP_USER` / `DRASI_MSSQL_TEST_APP_PASSWORD` | Separate minimally privileged native source/sink SQL login |
| `DRASI_MSSQL_TEST_READER_USER` / `DRASI_MSSQL_TEST_READER_PASSWORD` | Separate standalone snapshot-only SQL login |
| `DRASI_MSSQL_TEST_STATE_ROOT` | Existing canonical, owner-only directory in the approved project/session area, for actual RocksDB reopen tests; not `/tmp`, `/private/tmp`, or `/var/tmp` |
| `DRASI_NATIVE_PLUGIN_DIR` | Absolute directory containing all three freshly rebuilt native MSSQL dynamic libraries |

Inject credentials and the ownership token using an approved secret/configuration
mechanism. Do not commit them, put passwords in command-line arguments, print
them, or copy them into examples. The fixture passes passwords to SQLCMD through
its child environment, and redacts credentials from fixture command diagnostics.
Environment credentials are also resolved into native `settings`; they are not
saved in the Server's generated configuration.

### Administrator provisioning (outside the fixture)

1. Create an isolated empty database with the exact approved `drasi_native_...`
   name. Do not reuse a user/demo/application database.
2. Enable `ALLOW_SNAPSHOT_ISOLATION` on that database.
3. Enable database CDC using `sys.sp_cdc_enable_db`, with the DBA privileges
   required by SQL Server for that one-time operation. Ensure SQL Server Agent
   and the capture job are running. SQL Server 2019/2022 with real CDC is required;
   Azure SQL Edge or a mocked protocol is not a substitute.
4. Add a database-level extended property named
   `DrasiNativeQualificationOwner`, with a unique operator-supplied string equal
   to `DRASI_MSSQL_TEST_OWNERSHIP_TOKEN`. The fixture checks this marker, current
   database name, CDC-enabled state, and SNAPSHOT-enabled state before creating
   any schema. It never adds or changes that ownership property itself.
5. Provision the three SQL logins and matching database users **with the same
   names as their logins**. Login names must use ASCII letters/digits or `_-.@`
   and be at most 128 bytes. The fixture does not create external logins/users.
6. Give the **fixture administrator** `db_owner` in this isolated database so it
   can create/drop its schemas, tables/procedures, enable/disable table CDC,
   perform controlled catalog/retention changes, and grant object permissions.
   Give that administrator `VIEW SERVER STATE` on SQL Server 2019, or the
   applicable `VIEW SERVER PERFORMANCE STATE` permission on SQL Server 2022, for
   the explicitly scoped `sys.dm_tran_locks` observations. These DMV permissions
   are used only by the test administrator.
7. Give the **application user** CONNECT plus SELECT on
   `cdc.change_tables` (catalog metadata). Do not make it `sysadmin`/`db_owner`,
   grant database CREATE TABLE, or grant general data-write privileges.
8. Give the **read-only user** CONNECT only initially. Do not give it CDC SELECT,
   broad data-reader membership, writes, DDL or administrative permissions.

Per-schema setup grants the application SELECT/catalog visibility on the owned
source tables, SELECT only on the three owned CDC tables/functions, INSERT/DELETE
on the owned barrier, and EXECUTE on owned procedures. It grants the read-only
user SELECT/VIEW DEFINITION only on the **non-CDC** `ReadOnlyItems` table.
Ordinary native application credentials never need sysadmin, KILL, database
creation, CDC administration or server-state inspection.

The database's ownership marker and pre-provisioned users/catalog grant remain
after tests. The fixture removes only resources it created. If cleanup fails,
the reported owned schema can be reviewed by the administrator; no blanket
database reset/drop or schema wildcard deletion is attempted.

## Authored assertions

`native_mssql_components::coverage` runs each scenario with static and rebuilt
dynamic libraries under both current-thread and multi-thread Tokio runtimes:

* **Standalone/least privilege:** reject overprivileged fixture accounts, read
  real non-CDC tables using the reader login, preserve key/value identities,
  filter unmatched labels, return no CDC watermark, and fence a partially read
  stream after stop.
* **Coordinated concurrent cut:** hold the owned marker table, observe real
  source-table S locks, start a multi-table writer and observe its blocked lock,
  release the marker, verify the snapshot contains the exact pre-writer state,
  then verify every committed update/insert/delete is strictly after the actual
  watermark in both CDC tables. A rolled-back update must not appear. This
  checks table locking plus commit ordering, not just a marker/LSN.
* **Snapshot errors/cancellation:** exceed the individual encoded-record byte
  budget without truncation, verify lock/marker cleanup, cancel a blocked snapshot, require a failed/timed-out
  stop while server locks remain, release owned work, then verify actual locks
  and markers are gone after cleanup.
* **Tiny limits / large data:** stream hundreds of standalone/coordinated snapshot
  rows with `max_rows=1`, defer the cut until final completion, then consume a
  larger interleaved two-table CDC backlog with one-row pages, update pairs,
  rekey and delete. Row/page bounds must never become a total dataset cap.
* **Activation identity:** real provider credentials must override deliberately
  wrong literals and refresh on restart without changing declared configuration.
  Token-as-SQL-auth-pair behavior is tested separately from any unsupported
  Azure token claim. Database-free Host tests cover binding, provider errors,
  endpoint context and certificate rejection; those are not SQL runtime proof.
* **Persistent partial replay:** use real RocksDB query providers and two actual
  queries; stop one shared producer while a large transaction is partially
  consumed under bounded output backpressure; record actual persistent row
  cursors, destroy/rebuild the runtime, and verify complete replay plus offline
  update/delete/rekey/rollback effects with no missing/duplicate IDs.
* **Catalog/recovery failure:** preserve stored cursors and require explicit
  failure after captured-column type changes, capture recreation, configured
  key changes, or controlled cleanup beyond an actual retained checkpoint.
  Separately clean only older CDC rows, prove the checkpoint row still exists,
  and require normal persistent resumption without an unnecessary reset.
* **Exact procedure effects:** full-ID/short-ID/default routing, ADD/UPDATE/DELETE/
  aggregation contexts, exact `u64::MAX` decimal binding, `i64::MIN`, bool, float,
  NULL, Unicode/quotes and JSON object/array effects. Successful procedures emit
  multiple result sets; a procedure that emits two result sets then THROWs must
  fail after exactly one recorded effect, never silently succeed or retry, and
  must not execute later commands from that failed input batch.
* **Retained procedure work:** observe ENTERED/COMPLETED records in the actual
  server, independently cancel or time out the Host call, require stop to fail
  while work is retained, then explicitly release the owned procedure and verify
  exactly one completion and successful cleanup. Socket closure is not used as
  proof that server-side work stopped.

The Server test reuses this explicit fixture and supplies its uniquely scoped
settings/procedure through the normal configuration path.

## Execution after approval

First rebuild all three matched native libraries. Then, with the required
environment injected and owner-only state root prepared:

```sh
cargo +1.95.0 test --offline --locked -p drasi-host-sdk \
  --test native_mssql_components -- --test-threads=1
# In the existing drasi-server checkout:
cargo +1.95.0 test --offline --locked --test native_mssql_test -- --test-threads=1
```

Missing inputs or failed ownership/preflight checks fail the tests; no case
silently skips or falls back to another database.

The older container fixture remains available only with explicit
`DRASI_MSSQL_TEST_MODE=podman` and an injected
`DRASI_MSSQL_TEST_LOCAL_PASSWORD`; its password is no longer a committed fixture
constant. That mode is **not** approval for more local emulation attempts.

Final disposal of that owned container uses `podman rm --force --volumes` so
image-created anonymous fixture volumes do not survive the fixture. Intermediate
component/graph stop/start cycles retain the same container and data. This is not
a named-volume, image, external-database, or shared-data prune; external mode
continues to use only its ownership-checked schema cleanup.
