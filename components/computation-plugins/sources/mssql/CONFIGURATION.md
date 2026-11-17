# Native SQL Server source: configuration

`drasi.mssql/changes` takes one flat camelCase object: `stream` plus the
shared settings below. The bootstrap providers in
[`drasi-computation-bootstrap-mssql`](../../bootstrappers/mssql) take the
same settings beside their `source`, `stream` and `labels`, so the source
and its provider select the same tables and keys, map values identically and
compute one binding.

```json
{
  "stream": "orders/out",
  "host": "sql.internal",
  "database": "shop",
  "user": "drasi_cdc",
  "password": {"secret": "mssql-password"},
  "tables": ["dbo.Orders", "[Sales Ops].[Order Lines]"],
  "barrierTable": "drasi.Barrier"
}
```

`password` is the only secret field; a host accepts only a secret reference
for it. It may be omitted when the host binds an activation identity, whose
credentials then replace `user` and `password` for each activation.

> Live SQL Server qualification is deferred (CW-78). The settings, SQL and
> runbook below are verified by unit tests and database-free Host checks
> only.

## Settings

| Field | Type | Default | Range and notes | Secret | Update |
|---|---|---|---|---|---|
| `stream` | string | required | Output stream identifier on every change. | | unsupported |
| `host` | string | `localhost` | Host name or address (≤ 253 bytes). Also the endpoint given to an identity provider. Not part of the cursor binding. | | Restart |
| `port` | integer | `1433` | 1..=65535. | | Restart |
| `database` | string | required | ≤ 128 characters. | | unsupported |
| `user` | string | none | SQL login; required unless an activation identity is bound. | | Restart |
| `password` | string | none | Secret reference; required unless an activation identity is bound. | yes | Restart |
| `encryption` | string | `required` | `required` (TLS for the session, certificate verified), `loginOnly` (TLS for the login packet only) or `disabled` (plaintext). The last two log a warning. | | Restart |
| `trustServerCertificate` | boolean | `false` | Skip certificate validation (testing only; warns). Exclusive with `trustedCaPem`; requires encryption. | | Restart |
| `trustedCaPem` | string | none | Extra trusted CA certificates (PEM, ≤ 256 KiB). | | Restart |
| `hostNameInCertificate` | string | `host` | Expected certificate name when it differs from `host`. | | Restart |
| `connectTimeoutMs` | integer | `15000` | 1..=3600000. TCP, TLS and login of one attempt. | | Restart |
| `commandTimeoutMs` | integer | `30000` | 1..=3600000. Each server round trip. Pause and Drain stop wait up to this long for an in-flight query. | | Restart |
| `serverTimeZone` | string | detected | A `sys.time_zone_info` name (`UTC`, `Pacific Standard Time`) for converting commit times to UTC. Detected with `CURRENT_TIMEZONE_ID()` on SQL Server 2022 and Azure SQL; **required on older servers**. | | Restart |
| `tables` | array of strings | `[]` | `table` (in `dbo`) or `schema.table`; bracket-quote other names (`[Sales Ops].[Order Lines]`). At most 256, no duplicates, not `barrierTable`. Empty selects every CDC-enabled table except `barrierTable`; a capture instance on a new table then stops the source with `SchemaChanged`. | | unsupported |
| `tableKeys` | array | `[]` | `{"table": "schema.table", "keyColumns": ["a", "b"]}` for tables without a primary key, or to override one. Columns nonempty, unique and captured. | | unsupported |
| `startPosition` | string | `current` | Where a group without any committed cursor starts: `current` (the CDC maximum LSN at its first start) or `beginning` (each capture instance's oldest retained change). No effect once cursors exist. | | Restart |
| `barrierTable` | string | none | Administrator-provisioned CDC table with a `token uniqueidentifier` column, used by the coordinated bootstrap provider. Excluded from an all-tables selection; erasing removal deletes this source's leftover markers from it. | | unsupported |
| `pollIntervalMs` | integer | `1000` | 1..=3600000. Delay before polling again after a page with no change. | | InPlace |
| `maxRows` | integer | `10000` | 1..=1000000. Page size: change groups read and changes emitted per poll. | | InPlace |
| `maxBytes` | integer | `67108864` | 1024..=268435456. Record payload bytes per page (both images of an update count); one change must fit. | | InPlace |
| `reconnectBudgetMs` | integer | `300000` | 0..=86400000. Total time to keep reconnecting after a transport failure, with full-jitter backoff from 250 ms doubling to 30 s. `0` disables reconnection. | | InPlace |

Unknown fields are rejected, including every field of the previous nested
format (see [Migrating](#migrating-from-the-previous-configuration)).

## Server prerequisites

| Requirement | Why |
|---|---|
| SQL Server 2016 or later, or Azure SQL / Managed Instance, with SQL Server Agent (or the Azure scheduler) running the CDC capture job | Changes reach the change tables only through the capture job. |
| `sys.sp_cdc_enable_db` on the database, `sys.sp_cdc_enable_table` on each table, with one capture instance per selected table that includes every key column | The source reads the capture instance's change table. Two capture instances on a selected table fail with `SchemaChanged`; drop the old one. |
| A primary key, or a `tableKeys` entry, on each selected table | Stable element identity. Keys of type xml, json, spatial, hierarchyid, sql_variant or unmapped types are rejected. |
| `serverTimeZone` on SQL Server 2016–2019 | Those servers cannot report their time zone. |
| CDC retention (`sys.sp_cdc_change_job @job_type = 'cleanup', @retention = …`) above the longest expected downtime **and** idle period of the selected tables | Committed cursors advance only with emitted changes; a cursor older than retention is `PositionUnavailable`. |

### Permissions (MS-07)

The source reads change tables directly (`cdc.<capture_instance>_CT`), not
through `cdc.fn_cdc_get_all_changes_*`. The login needs:

```sql
CREATE USER drasi_cdc FOR LOGIN drasi_cdc;
GRANT SELECT ON cdc.change_tables TO drasi_cdc;        -- capture catalog
GRANT SELECT ON cdc.dbo_Orders_CT TO drasi_cdc;        -- each selected change table
GRANT VIEW DEFINITION ON OBJECT::dbo.Orders TO drasi_cdc; -- key and column catalog
-- Erasing removal only:
GRANT DELETE ON drasi.Barrier TO drasi_cdc;
```

Membership in a capture instance's gating role is not enough on its own:
the role must also hold SELECT on the change table. Start checks
`HAS_PERMS_BY_NAME(<change table>, 'OBJECT', 'SELECT')` for every selected
table and fails with an `Unsupported` error naming the missing grant, so a
least-privilege login fails at start rather than at the first poll. The
coordinated bootstrap provider additionally needs SELECT on the source
tables and INSERT/DELETE on `barrierTable`.

## Encryption and authentication

- Encryption is required by default and verifies the server certificate
  against the system roots plus `trustedCaPem`. Plaintext is an explicit
  opt-in (`loginOnly`, `disabled`) and logs a warning at every start.
- SQL authentication uses `user` and `password`.
- With an activation identity bound by the host, each start receives fresh
  credentials: a username/password pair is used as SQL authentication; a
  token is used as a **Microsoft Entra ID access token** (for
  `https://database.windows.net/`), which Azure SQL requires for managed
  identities. The literal `user`/`password` are then ignored; a bound source
  never falls back to them. The identity provider sees `host` and `port`.
- Azure SQL gateway redirects are followed once.
- Windows integrated authentication is not supported.

## Updates

Each field's boundary is in the Update column. InPlace changes need a
completed pause and take effect at the next poll; Restart changes are
applied while stopped and take effect at the next start, which resumes at
the members' committed minimum (a host rename keeps cursors, because the
binding uses the database's `service_broker_guid`, not the endpoint).
Unsupported fields are what committed cursors and owned markers are bound
to: remove the source with erase and add it again, which rebuilds downstream
queries from a new snapshot. Consumer membership is a host dependency;
changing it replaces the source.

## Removal and erase

Removal without erase keeps everything. Removal with erase deletes the
barrier markers this source owns — rows of `barrierTable` whose token starts
with this source's derived prefix, left behind by a crashed or failed
coordinated snapshot — and nothing else. It is idempotent: a missing table
or no rows is success. Without `barrierTable` the source owns no external
state and erase succeeds without connecting. When the source authenticates
through an activation identity, erase uses the last activation's credentials;
if it never started in this process, erase fails and names the manual
statement:

```sql
DELETE FROM drasi.Barrier WHERE UPPER(LEFT(CONVERT(char(36), token), 18)) = '<prefix>';
```

CDC itself (capture instances, change tables, jobs) is never created or
dropped by the source.

## Failures

| Error | Meaning | Action |
|---|---|---|
| `SchemaChanged` | A selected table's capture instance, key columns or key types changed; a second capture instance appeared on a selected table; with empty `tables`, a new captured table appeared; or a committed cursor belongs to another database or definition. Non-key column changes are tolerated (decision D-7). | Remove the source with erase and add it again. |
| `PositionUnavailable` | CDC cleanup removed changes after a committed cursor, a capture instance disappeared, or a cursor is ahead of the CDC maximum (restore, CDC re-enabled). | Remove with erase and re-add, or restore retention. |
| `Unsupported` | Missing SELECT on a change table, a table without a usable key, an undecodable change group (MS-15), an unknown `serverTimeZone`, or a server that cannot report its time zone. | Fix the grant, keys or configuration. |

Transport failures (connection loss, timeouts, failover and Azure transient
error numbers) reconnect within `reconnectBudgetMs` and resume without loss.
Permanent failures never reconnect: the source reports them through `next`
and the host stops it.

## Operations

- **Lag.** Compare `sys.fn_cdc_get_max_lsn()` with the consumers' committed
  cursors; `sys.dm_cdc_log_scan_sessions` shows capture latency.
- **Pause** stops at a page boundary; an in-flight query finishes first
  (at most `commandTimeoutMs`). Queued changes are held.
- **Stop** with `Drain` finishes the in-flight query and closes the session;
  `Abort` drops the connection (Tiberius cannot send a TDS ATTENTION, so
  SQL Server ends the request when it notices). An expired stop keeps the
  worker owned; retry the stop.
- **Throughput.** One connection per activation; each page costs a
  fingerprint query, an LSN-bounds query, one key query, one range query per
  table with changes in the page, and a retention recheck. Raise `maxRows`
  for large backlogs.
- **Consumers restarting** hold the source (logged) instead of failing it.
- **Schema discovery** describes the selected tables with the labels and
  property types the source emits.

## Migrating from the previous configuration

This is a breaking change with no automatic migration. Committed cursors of
the previous release used a binding that included host and port; they are
reported as `SchemaChanged`, so remove and re-add existing sources.

| Previous | Now |
|---|---|
| `settings: {connection: {...}, ...}`, the whole object secret | Flat fields; only `password` is secret. |
| `connection.host`, `port`, `database`, `user`, `password` | Same names, top level. |
| `connection.auth_mode` | Removed: SQL authentication, or activation credentials (including Entra ID tokens). |
| `connection.encryption` (`off`/`on`/`not_supported`, default `not_supported`) | `encryption`: `disabled`/`required`/`loginOnly`, default `required`. |
| `connection.trust_server_certificate` | `trustServerCertificate`; new `trustedCaPem`, `hostNameInCertificate`. |
| `connection.tables`, `connection.table_keys[].key_columns` | `tables`, `tableKeys[].keyColumns`; bracket-quoted names allowed. |
| `connection.start_position` | `startPosition`. |
| `connection.poll_interval_ms` | `pollIntervalMs`. |
| `barrier_table`, `max_rows`, `max_bytes` | `barrierTable`, `maxRows` (now the real page size, default 10000), `maxBytes`. |
| `operation_timeout_ms` | `commandTimeoutMs` (per round trip) and `connectTimeoutMs`. |
| `shutdown_timeout_ms` | Removed: Stop uses the host deadline. |
| (none) | `serverTimeZone`, `reconnectBudgetMs`. |
