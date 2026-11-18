# Native SQL Server bootstrap providers

**Live SQL Server qualification is explicitly deferred by the user (CW-78);
this family is verified by unit tests and database-free Host checks only.**

One bootstrap-role crate exports two explicit factories, both through the real
static SDK and the native ABI 5.0 dynamic library:

| Factory | Permissions and behavior |
|---|---|
| `drasi.mssql/snapshot` | Standalone **read-only** table snapshot. Explicit `tables`, primary/custom keys, SELECT/catalog visibility and the database's SNAPSHOT-isolation option. No CDC catalogs, marker, DDL or data writes. Returns **no CDC handover watermark**. |
| `drasi.mssql/bootstrap` | Opt-in coordinated CDC snapshot, paired with a `drasi.mssql/changes` source that owns an administrator-provisioned CDC `barrierTable`. Writes and deletes its own marker in the source's barrier table. Holds shared table locks only while it fixes a SNAPSHOT-isolation view and commits the marker, then reads row versions without blocking writers; needs `ALLOW_SNAPSHOT_ISOLATION ON`. |

Both declare their bootstrap output, which the graph checks when the
provider's edge binds; see the
[native bootstrap provider standard](../NATIVE-BOOTSTRAP-PROVIDER-GUIDE.md).

## Configuration

`drasi.mssql/bootstrap` takes **no configuration**. The source owns its
connection settings, tables, keys and `barrierTable`, and declares the
`drasi.mssql/coordinated-changes` coordination protocol when it has a
`barrierTable`. Connect the provider node to that source's `bootstrap` input:
the source's coordinator opens each query's provider from the source's own
request and connects with the source's current activation credentials
(including an Entra ID token), so the markers it writes are the ones the
source erases. The graph refuses the provider on a source that does not
declare the protocol.

`drasi.mssql/snapshot` takes one flat camelCase object: optional `labels` (an
upper bound on the tables read) and optional `stopTimeoutMs` (how long `stop`
waits for snapshot cleanup, default `30000`), beside **the same settings the
native source accepts** (see the source's
[CONFIGURATION.md](../../sources/mssql/CONFIGURATION.md)) except
`barrierTable`, which it refuses. `tables` must be explicit. `password` is the
only secret; the standalone snapshot uses `user` and `password` (no activation
credentials), and encryption is required by default, as for the source. The
served source and stream are not configured: the graph relation supplies the
source whose `bootstrap` input the provider is connected to (or the single
source a query reads, for a query-local provider).

```json
{"labels": ["Orders"], "host": "sql.internal", "database": "shop",
 "user": "drasi_cdc", "password": {"secret": "mssql-password"},
 "tables": ["dbo.Orders"]}
```

Each query reads only the selected tables whose label (the table name) it
declares, within `labels` when configured.

Schema-qualified table identities and
type conversion agree with the source. Unknown labels produce no rows, without releasing the coordinated table fence.
Rows flow through an owned, row/byte-bounded channel with backpressure. `maxRows`
limits queued rows and each server-cursor FETCH batch, **not total snapshot size**.
`maxBytes` counts actual serialized graph-record payload bytes, not Debug output;
one oversized individual record fails rather than being truncated. A worker may
also hold the currently decoded row and driver/catalog state.

Both modes use a session-owned forward-only SQL cursor and bounded FETCH batches.
Cancellation requests cleanup without dropping a pending SQL future; stop can
time out while that owned work remains. Cleanup closes/deallocates the cursor,
rolls back any unfinished transaction and, in coordinated mode, deletes only the
owned marker. Unconfirmed cleanup remains an error on subsequent stop attempts.
Streams are generation-fenced. **No watermark is returned at stream startup**:
only `complete_snapshot`, after EOF, successful commit and confirmed cleanup,
returns the real CDC cut. Standalone mode never returns a CDC cut.

## Why the modes are separate

The old bootstrapper opened a SNAPSHOT transaction and sampled
`sys.fn_cdc_get_max_lsn()`. SQL Server CDC capture is asynchronous: that number
does **not** prove that every row visible in the snapshot is captured, even when
read inside the transaction. Returning it as an exact handover could overlap or
lose updates. The standalone factory preserves the useful read-only snapshot
without repeating that unsupported handover claim.

The coordinated algorithm:

1. Discover and validate all selected tables/capture/key definitions, and
   refuse a database without `ALLOW_SNAPSHOT_ISOLATION ON` (both modes read
   under SNAPSHOT isolation).
2. On a separate marker connection, begin a transaction and take
   `TABLOCK,HOLDLOCK` read locks on **every selected source table**, including
   tables excluded by a query's labels. Granting them waits for in-flight
   writers; from then on no selected-table write can commit.
3. On the reader connection, begin a SNAPSHOT transaction and touch every
   selected table, which fixes its row-version view inside that window.
4. On the marker connection, insert a unique marker into the explicitly
   provisioned CDC marker table (which must not be a selected source table) and
   commit, which releases the table locks.
5. Wait for that exact INSERT to appear in its actual CDC capture table; its
   `__$start_lsn` is the boundary.
6. Read selected rows from the SNAPSHOT view, verify metadata and CDC
   retention, commit, and delete only the owned marker.

The view holds exactly the changes committed before the marker's LSN: nothing
on a selected table committed between the locks being granted and the marker's
commit. Writers wait only for that short window (bounded by
`commandTimeoutMs` as the lock timeout), never for the scan, the CDC capture or
downstream backpressure; the scan reads row versions and takes no locks. Plan
`tempdb` version-store space for the snapshot's duration, and enough CDC
retention for the scan.

No plugin performs CREATE/ALTER/CDC administration, and normal credentials need
no sysadmin or KILL privileges. A DBA provisions CDC and a table such as
`dbo.DrasiBarrier(token uniqueidentifier primary key)` once; the application
needs INSERT/DELETE on it plus appropriate selected/CDC-table SELECT.

There is no public DrasiLib SQL API or general SQL service.

### Marker ownership (decision D-1)

Each marker's `token` starts with 64 bits derived from the graph and the
paired **source** (`SHA-256("drasi.mssql/marker/v1", graph, source)`),
followed by 64 random bits, so every marker belongs to exactly one source.
The source is the one whose coordinator opened the provider, never a
configured identity, and the barrier table is the source's own.
The provider deletes its marker after each attempt. A process crash or a
failed cleanup may leave one behind; **the source owns those markers** and
deletes every marker with its prefix on erasing removal, idempotently. The
providers do not sweep: they have no erase hook, and a sweep at snapshot
start could delete the marker of another snapshot still in progress. Do not
configure a retention cleanup policy that could delete an in-flight marker.

## Qualification

**Not qualified: live qualification is deferred by the user (CW-78).**
Static/dynamic metadata, configuration and shared decoder tests are not proof
of concurrent snapshot correctness. Concurrent writers,
multi-table transactions, lock timeout/rollback, marker cleanup failure, stream
cancellation, and read-only least-privilege behavior require real-server
qualification. A read-only snapshot must never be advertised as coordinated
snapshot/live delivery.
